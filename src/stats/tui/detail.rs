//! Single-conversation interactive turn inspector (`agy-statusline stats <CONVERSATION_ID>`).

use std::cmp::Ordering;
use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table, TableState};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use super::{
    compute_header_hits, render_table_with_scrollbar, right_cell, token_breakdown_lines,
    HeaderHits, Palette, TableChrome, TableHit,
};
use crate::agy::db::compute_median;
use crate::stats::metric::{
    format_pct, format_usd_short, SortDir, TurnSort, TurnSortField, UsageCounters,
};
use crate::stats::pricing::round_usd_6;
use crate::stats::render::{
    format_subagent_short_label, format_time_span, format_ttft_compact, format_turn_time,
};
use crate::stats::{ConversationStatsReport, ConversationTurnStats, PeriodSummary, UsageDelta};
use crate::sys::fmt::{format_tokens_string as fmt_tok, truncate_to_width};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RightTab {
    Models,
    Subagents,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetailPane {
    Turns,
    Breakdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DetailAction {
    Continue,
    Refresh,
    OpenConversations,
    Back,
    Quit,
}

struct DetailView {
    turn_indices: Vec<usize>,
    summary: PeriodSummary,
    median_tok_per_sec: Option<f64>,
    median_ttft_secs: Option<f64>,
    peak_context_tokens: Option<u64>,
    multi_day: bool,
    show_cost: bool,
    show_model_col: bool,
    subagent_short_labels: HashMap<String, String>,
}

fn turn_prompt_context(t: &ConversationTurnStats) -> u64 {
    t.input_tokens
        .saturating_add(t.cache_read_tokens)
        .saturating_add(t.cache_write_tokens)
}

/// Compares optional floats with `None` always placed last regardless of direction.
fn cmp_opt_f64(a: Option<f64>, b: Option<f64>, dir: SortDir) -> Ordering {
    match (a, b) {
        (Some(va), Some(vb)) => dir.apply(va.total_cmp(&vb)),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

/// Compares optional strings with `None` always placed last regardless of direction.
fn cmp_opt_str(a: Option<&str>, b: Option<&str>, dir: SortDir) -> Ordering {
    match (a, b) {
        (Some(va), Some(vb)) => dir.apply(va.cmp(vb)),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn cmp_turns(ta: &ConversationTurnStats, tb: &ConversationTurnStats, sort: TurnSort) -> Ordering {
    let ord = match sort.field {
        TurnSortField::Turn => sort.dir.apply(ta.index.cmp(&tb.index)),
        TurnSortField::Time => cmp_opt_str(
            ta.timestamp_local.as_deref(),
            tb.timestamp_local.as_deref(),
            sort.dir,
        ),
        TurnSortField::Model => sort.dir.apply(
            ta.model
                .to_ascii_lowercase()
                .cmp(&tb.model.to_ascii_lowercase()),
        ),
        TurnSortField::Context => sort
            .dir
            .apply(turn_prompt_context(ta).cmp(&turn_prompt_context(tb))),
        TurnSortField::Input => sort.dir.apply(ta.input_tokens.cmp(&tb.input_tokens)),
        TurnSortField::Cache => {
            let ca = ta.cache_read_tokens.saturating_add(ta.cache_write_tokens);
            let cb = tb.cache_read_tokens.saturating_add(tb.cache_write_tokens);
            sort.dir.apply(ca.cmp(&cb))
        }
        TurnSortField::Output => sort.dir.apply(ta.output_tokens.cmp(&tb.output_tokens)),
        TurnSortField::CacheHit => cmp_opt_f64(ta.cache_hit_pct, tb.cache_hit_pct, sort.dir),
        TurnSortField::Speed => cmp_opt_f64(ta.tok_per_sec, tb.tok_per_sec, sort.dir),
        TurnSortField::Ttft => cmp_opt_f64(ta.ttft_secs, tb.ttft_secs, sort.dir),
        TurnSortField::Cost => cmp_opt_f64(ta.priced_usd, tb.priced_usd, sort.dir),
    };
    ord.then_with(|| ta.index.cmp(&tb.index))
}

pub(super) struct DetailScreen {
    report: ConversationStatsReport,
    sort: TurnSort,
    right_tab: RightTab,
    active_pane: DetailPane,
    misses_only: bool,
    model_filter: Option<String>,
    subagent_filter: Option<String>,
    turn_idx: usize,
    breakdown_idx: usize,
    status_msg: Option<String>,
    view: DetailView,
    turns_table: TableState,
    breakdown_table: TableState,
    turns_hit: TableHit,
    turns_header_hits: HeaderHits<TurnCol>,
    breakdown_hit: TableHit,
}

impl DetailScreen {
    pub(super) fn new(report: ConversationStatsReport, sort: TurnSort) -> Self {
        let right_tab = if report.models.len() <= 1 && !report.subagents.is_empty() {
            RightTab::Subagents
        } else {
            RightTab::Models
        };
        let mut screen = Self {
            view: DetailView {
                turn_indices: Vec::new(),
                summary: PeriodSummary::default(),
                median_tok_per_sec: None,
                median_ttft_secs: None,
                peak_context_tokens: None,
                multi_day: false,
                show_cost: false,
                show_model_col: false,
                subagent_short_labels: HashMap::new(),
            },
            report,
            sort,
            right_tab,
            active_pane: DetailPane::Turns,
            misses_only: false,
            model_filter: None,
            subagent_filter: None,
            turn_idx: 0,
            breakdown_idx: 0,
            status_msg: None,
            turns_table: TableState::default(),
            breakdown_table: TableState::default(),
            turns_hit: TableHit::default(),
            turns_header_hits: HeaderHits::default(),
            breakdown_hit: TableHit::default(),
        };
        screen.recompute_view();
        screen
    }

    pub(super) fn conversation_id(&self) -> &str {
        &self.report.conversation_id
    }

    pub(super) fn subagent_ids(&self) -> Vec<String> {
        self.report
            .subagents
            .iter()
            .map(|s| s.conversation_id.clone())
            .collect()
    }

    pub(super) fn sort(&self) -> TurnSort {
        self.sort
    }

    pub(super) fn set_status_msg(&mut self, msg: impl Into<String>) {
        self.status_msg = Some(msg.into());
    }

    /// Replaces the report from a live watch tick or manual refresh while preserving
    /// the selected turn (or following the tail in default `# ↑` mode when new turns arrive).
    pub(super) fn apply_live_report(&mut self, report: ConversationStatsReport) {
        let selected_turn_key = self.view.turn_indices.get(self.turn_idx).map(|&raw_idx| {
            let t = &self.report.turns[raw_idx];
            (t.conversation_id.clone(), t.index)
        });
        let was_at_tail_asc = self.sort == TurnSort::default()
            && !self.view.turn_indices.is_empty()
            && self.turn_idx + 1 == self.view.turn_indices.len();
        let old_turn_count = self.report.turns.len();
        let selected_model = (self.right_tab == RightTab::Models)
            .then(|| {
                self.report
                    .models
                    .get(self.breakdown_idx)
                    .map(|m| m.model.clone())
            })
            .flatten();
        let selected_sub = (self.right_tab == RightTab::Subagents)
            .then(|| {
                self.report
                    .subagents
                    .get(self.breakdown_idx)
                    .map(|s| s.conversation_id.clone())
            })
            .flatten();

        self.report = report;
        self.recompute_view();

        if was_at_tail_asc
            && self.report.turns.len() > old_turn_count
            && !self.view.turn_indices.is_empty()
        {
            self.turn_idx = self.view.turn_indices.len() - 1;
        } else if let Some((cid, idx)) = selected_turn_key {
            if let Some(pos) = self.view.turn_indices.iter().position(|&raw_idx| {
                let t = &self.report.turns[raw_idx];
                t.index == idx && t.conversation_id == cid
            }) {
                self.turn_idx = pos;
            }
        }

        if let Some(model) = selected_model {
            if let Some(pos) = self.report.models.iter().position(|m| m.model == model) {
                self.breakdown_idx = pos;
            }
        } else if let Some(sub_id) = selected_sub {
            if let Some(pos) = self
                .report
                .subagents
                .iter()
                .position(|s| s.conversation_id == sub_id)
            {
                self.breakdown_idx = pos;
            }
        }
    }

    fn cycle_sort_field(&mut self) {
        let fields: Vec<TurnSortField> = if !self.turns_header_hits.cols.is_empty() {
            self.turns_header_hits
                .cols
                .iter()
                .map(|c| c.col.sort_field())
                .collect()
        } else {
            let mut all = vec![
                TurnSortField::Turn,
                TurnSortField::Time,
                TurnSortField::Model,
                TurnSortField::Context,
                TurnSortField::Input,
                TurnSortField::Cache,
                TurnSortField::Output,
                TurnSortField::CacheHit,
                TurnSortField::Speed,
                TurnSortField::Ttft,
                TurnSortField::Cost,
            ];
            if !self.view.show_model_col {
                all.retain(|&f| f != TurnSortField::Model);
            }
            if !self.view.show_cost {
                all.retain(|&f| f != TurnSortField::Cost);
            }
            all
        };
        let next_field = fields
            .iter()
            .position(|&f| f == self.sort.field)
            .map_or(TurnSortField::Turn, |idx| fields[(idx + 1) % fields.len()]);
        self.sort = TurnSort {
            field: next_field,
            dir: next_field.default_dir(),
        };
        self.turn_idx = 0;
        self.recompute_view();
    }

    fn click_sort_field(&mut self, field: TurnSortField) {
        self.sort = self.sort.click(field);
        self.turn_idx = 0;
        self.recompute_view();
    }

    fn has_active_filters(&self) -> bool {
        self.misses_only || self.model_filter.is_some() || self.subagent_filter.is_some()
    }

    fn breakdown_len(&self) -> usize {
        match self.right_tab {
            RightTab::Models => self.report.models.len(),
            RightTab::Subagents => self.report.subagents.len(),
        }
    }

    fn recompute_view(&mut self) {
        let mut turn_indices: Vec<usize> = self
            .report
            .turns
            .iter()
            .enumerate()
            .filter(|(_, t)| !self.misses_only || t.cache_hit_pct.is_some_and(|p| p < 50.0))
            .filter(|(_, t)| self.model_filter.as_deref().is_none_or(|m| t.model == m))
            .filter(|(_, t)| {
                self.subagent_filter
                    .as_deref()
                    .is_none_or(|id| t.conversation_id == id)
            })
            .map(|(idx, _)| idx)
            .collect();

        turn_indices
            .sort_by(|&a, &b| cmp_turns(&self.report.turns[a], &self.report.turns[b], self.sort));

        let mut summary = PeriodSummary::default();
        let mut speeds = Vec::new();
        let mut ttfts = Vec::new();
        let mut peak_ctx: Option<u64> = None;

        for &idx in &turn_indices {
            let t = &self.report.turns[idx];
            summary.record_usage(UsageDelta {
                input_tokens: t.input_tokens,
                cache_read_tokens: t.cache_read_tokens,
                cache_write_tokens: t.cache_write_tokens,
                output_tokens: t.output_tokens,
                thinking_tokens: t.thinking_tokens,
                total_tokens: t.total_tokens,
                cost_usd: t.priced_usd,
            });
            if let Some(spd) = t.tok_per_sec {
                speeds.push(spd);
            }
            if let Some(ttft) = t.ttft_secs {
                ttfts.push(ttft);
            }
            let ctx = turn_prompt_context(t);
            if ctx > 0 {
                peak_ctx = Some(peak_ctx.map_or(ctx, |p| p.max(ctx)));
            }
        }
        summary.priced_usd = round_usd_6(summary.priced_usd);

        let multi_day = match (
            self.report
                .first_turn_local
                .as_deref()
                .and_then(|s| s.get(..10)),
            self.report
                .last_turn_local
                .as_deref()
                .and_then(|s| s.get(..10)),
        ) {
            (Some(a), Some(b)) => a != b,
            _ => false,
        };

        let subagent_short_labels = self
            .report
            .subagents
            .iter()
            .map(|sub| {
                (
                    sub.conversation_id.clone(),
                    format_subagent_short_label(&sub.conversation_id, sub.agent_name.as_deref()),
                )
            })
            .collect();

        self.view = DetailView {
            turn_indices,
            summary,
            median_tok_per_sec: compute_median(&speeds),
            median_ttft_secs: compute_median(&ttfts),
            peak_context_tokens: peak_ctx,
            multi_day,
            show_cost: self.report.totals.priced_requests > 0,
            show_model_col: self.report.models.len() > 1 || !self.report.subagents.is_empty(),
            subagent_short_labels,
        };

        if self.view.turn_indices.is_empty() {
            self.turn_idx = 0;
        } else if self.turn_idx >= self.view.turn_indices.len() {
            self.turn_idx = self.view.turn_indices.len() - 1;
        }
        let bd_len = self.breakdown_len();
        if bd_len == 0 {
            self.breakdown_idx = 0;
        } else if self.breakdown_idx >= bd_len {
            self.breakdown_idx = bd_len - 1;
        }
    }

    fn toggle_breakdown_filter(&mut self) {
        match self.right_tab {
            RightTab::Models => {
                if let Some(m) = self.report.models.get(self.breakdown_idx) {
                    self.model_filter = (self.model_filter.as_deref() != Some(m.model.as_str()))
                        .then(|| m.model.clone());
                    self.turn_idx = 0;
                }
            }
            RightTab::Subagents => {
                if let Some(sub) = self.report.subagents.get(self.breakdown_idx) {
                    self.subagent_filter = (self.subagent_filter.as_deref()
                        != Some(sub.conversation_id.as_str()))
                    .then(|| sub.conversation_id.clone());
                    self.turn_idx = 0;
                }
            }
        }
        self.recompute_view();
    }

    pub(super) fn handle_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
        has_back: bool,
    ) -> DetailAction {
        self.status_msg = None;

        if modifiers.contains(KeyModifiers::CONTROL) && matches!(code, KeyCode::Char('c' | 'C')) {
            return DetailAction::Quit;
        }

        let page_size = self.turns_hit.data_rows.max(5);
        let has_subs = !self.report.subagents.is_empty();
        let active_len = match self.active_pane {
            DetailPane::Turns => self.view.turn_indices.len(),
            DetailPane::Breakdown => self.breakdown_len(),
        };
        let active_idx = match self.active_pane {
            DetailPane::Turns => &mut self.turn_idx,
            DetailPane::Breakdown => &mut self.breakdown_idx,
        };

        match code {
            KeyCode::Char('q') => return DetailAction::Quit,
            KeyCode::Char('b') if has_back => return DetailAction::Back,
            KeyCode::Char('c') => return DetailAction::OpenConversations,
            KeyCode::Char('r') => return DetailAction::Refresh,
            KeyCode::Esc => {
                if self.has_active_filters() {
                    self.misses_only = false;
                    self.model_filter = None;
                    self.subagent_filter = None;
                    self.turn_idx = 0;
                    self.recompute_view();
                } else {
                    return if has_back {
                        DetailAction::Back
                    } else {
                        DetailAction::Quit
                    };
                }
            }
            KeyCode::Tab | KeyCode::BackTab => {
                self.active_pane = match self.active_pane {
                    DetailPane::Turns => DetailPane::Breakdown,
                    DetailPane::Breakdown => DetailPane::Turns,
                };
            }
            KeyCode::Char('s') => {
                self.cycle_sort_field();
            }
            KeyCode::Char('S') => {
                self.sort = self.sort.toggle_dir();
                self.turn_idx = 0;
                self.recompute_view();
            }
            KeyCode::Char('m') => {
                self.misses_only = !self.misses_only;
                self.turn_idx = 0;
                self.recompute_view();
            }
            KeyCode::Char('v') if has_subs => {
                self.right_tab = match self.right_tab {
                    RightTab::Models => RightTab::Subagents,
                    RightTab::Subagents => RightTab::Models,
                };
                self.breakdown_idx = 0;
            }
            KeyCode::Enter if self.active_pane == DetailPane::Breakdown => {
                self.toggle_breakdown_filter();
            }
            KeyCode::Up | KeyCode::Char('k') => *active_idx = active_idx.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') if active_len > 0 => {
                *active_idx = (*active_idx + 1).min(active_len - 1);
            }
            KeyCode::PageUp => *active_idx = active_idx.saturating_sub(page_size),
            KeyCode::PageDown if active_len > 0 => {
                *active_idx = (*active_idx + page_size).min(active_len - 1);
            }
            KeyCode::Home | KeyCode::Char('g') => *active_idx = 0,
            KeyCode::End | KeyCode::Char('G') if active_len > 0 => *active_idx = active_len - 1,
            _ => {}
        }
        DetailAction::Continue
    }

    pub(super) fn handle_mouse(&mut self, mouse: MouseEvent) {
        let pos = Position::new(mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.status_msg = None;
                if self.breakdown_hit.contains(pos) {
                    self.active_pane = DetailPane::Breakdown;
                    self.breakdown_idx = self.breakdown_idx.saturating_sub(1);
                } else {
                    self.active_pane = DetailPane::Turns;
                    self.turn_idx = self.turn_idx.saturating_sub(3);
                }
            }
            MouseEventKind::ScrollDown => {
                self.status_msg = None;
                if self.breakdown_hit.contains(pos) {
                    self.active_pane = DetailPane::Breakdown;
                    let len = self.breakdown_len();
                    if len > 0 {
                        self.breakdown_idx = (self.breakdown_idx + 1).min(len - 1);
                    }
                } else {
                    self.active_pane = DetailPane::Turns;
                    let len = self.view.turn_indices.len();
                    if len > 0 {
                        self.turn_idx = (self.turn_idx + 3).min(len - 1);
                    }
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.status_msg = None;
                if self.turns_hit.contains(pos) {
                    self.active_pane = DetailPane::Turns;
                    if let Some(col) = self.turns_header_hits.col_at(pos) {
                        self.click_sort_field(col.sort_field());
                    } else if let Some(target) = self.turns_hit.row_at(
                        pos,
                        self.turns_table.offset(),
                        self.view.turn_indices.len(),
                    ) {
                        self.turn_idx = target;
                    }
                } else if self.breakdown_hit.contains(pos) {
                    if mouse.row == self.breakdown_hit.pane.y && !self.report.subagents.is_empty() {
                        self.right_tab = match self.right_tab {
                            RightTab::Models => RightTab::Subagents,
                            RightTab::Subagents => RightTab::Models,
                        };
                        self.breakdown_idx = 0;
                    } else {
                        let was_active = self.active_pane == DetailPane::Breakdown;
                        self.active_pane = DetailPane::Breakdown;
                        if let Some(target) = self.breakdown_hit.row_at(
                            pos,
                            self.breakdown_table.offset(),
                            self.breakdown_len(),
                        ) {
                            if was_active && self.breakdown_idx == target {
                                self.toggle_breakdown_filter();
                            } else {
                                self.breakdown_idx = target;
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    pub(super) fn draw(&mut self, frame: &mut Frame<'_>, has_back: bool, pal: &Palette) {
        let area = frame.area();
        let root = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Min(8),
                Constraint::Length(1),
            ])
            .split(area);

        self.draw_header(frame, root[0], pal);
        self.draw_kpi_strip(frame, root[1], pal);

        let body = root[2];
        self.turns_hit = TableHit::default();
        self.turns_header_hits = HeaderHits::default();
        self.breakdown_hit = TableHit::default();

        if body.width >= 96 {
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(64), Constraint::Percentage(36)])
                .split(body);
            self.turns_hit = self.draw_turns_pane(frame, cols[0], pal);

            let detail_h = if cols[1].height >= 20 { 11 } else { 10 };
            let right = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(detail_h), Constraint::Min(6)])
                .split(cols[1]);
            self.draw_turn_detail_pane(frame, right[0], pal);
            self.breakdown_hit = self.draw_breakdown_pane(frame, right[1], pal);
        } else if self.active_pane == DetailPane::Turns {
            self.turns_hit = self.draw_turns_pane(frame, body, pal);
        } else {
            let detail_h = if body.height >= 18 { 11 } else { 9 };
            let stacked = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(detail_h), Constraint::Min(6)])
                .split(body);
            self.draw_turn_detail_pane(frame, stacked[0], pal);
            self.breakdown_hit = self.draw_breakdown_pane(frame, stacked[1], pal);
        }

        self.draw_footer(frame, root[3], has_back, pal);
    }

    fn draw_header(&self, frame: &mut Frame<'_>, area: Rect, pal: &Palette) {
        let sep = pal.sep();
        let short_id: String = self.report.conversation_id.chars().take(8).collect();
        let model_tag = if self.report.models.len() == 1 {
            format!(" {sep} {}", self.report.models[0].model)
        } else {
            String::new()
        };
        let live_mark = if pal.ascii { "* Live" } else { "● Live" };
        let title = format!(
            " agy-statusline stats {sep} Conversation {short_id}{model_tag} {sep} {} {sep} {} {sep} {live_mark} ",
            self.report.source, self.report.timezone
        );

        let inner_w = usize::from(area.width.saturating_sub(2));
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut used = 0usize;
        let mut push = |text: String, style: Style| -> bool {
            let w = text.width();
            if used + w > inner_w {
                return false;
            }
            spans.push(Span::styled(text, style));
            used += w;
            true
        };

        let mut controls: Vec<(String, Style)> =
            vec![(format!(" {} ", self.sort.label(pal.ascii)), pal.pill(true))];
        if self.misses_only {
            let style = if pal.no_color {
                Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
            } else {
                Style::default()
                    .bg(Color::Red)
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            };
            controls.push((" Misses ".to_string(), style));
        }
        let badge_style = if pal.no_color {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
                .bg(Color::Yellow)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD)
        };
        let close = if pal.ascii { "x" } else { "×" };
        let ellipsis = if pal.ascii { "..." } else { "…" };
        if let Some(model) = self.model_filter.as_deref() {
            controls.push((
                format!(" {close} {} ", truncate_to_width(model, 18, ellipsis)),
                badge_style,
            ));
        }
        if let Some(sub_id) = self.subagent_filter.as_deref() {
            let label = self
                .view
                .subagent_short_labels
                .get(sub_id)
                .map_or(sub_id, String::as_str);
            controls.push((
                format!(" {close} {} ", truncate_to_width(label, 18, ellipsis)),
                badge_style,
            ));
        }

        let controls_w: usize = controls.iter().map(|(s, _)| s.width() + 1).sum::<usize>() + 2;
        let conv_label = self
            .report
            .title
            .as_deref()
            .or(self.report.agent_name.as_deref())
            .unwrap_or(&self.report.conversation_id);
        let title_budget = inner_w.saturating_sub(controls_w + 2);
        if title_budget >= 8 {
            let _ = push(
                format!(" {}", truncate_to_width(conv_label, title_budget, ellipsis)),
                pal.bold(Color::White),
            );
            let _ = push(format!(" {sep} "), pal.dim());
        } else {
            let _ = push(" ".to_string(), Style::default());
        }

        for (idx, (text, style)) in controls.into_iter().enumerate() {
            if idx > 0 && !push(" ".to_string(), Style::default()) {
                break;
            }
            if !push(text, style) {
                break;
            }
        }

        frame.render_widget(
            Paragraph::new(Line::from(spans)).block(pal.pane_block(title, false)),
            area,
        );
    }

    fn draw_kpi_strip(&self, frame: &mut Frame<'_>, area: Rect, pal: &Palette) {
        let sep = pal.sep();
        let arrow = if pal.ascii { "->" } else { "→" };
        let filtered = self.has_active_filters();
        let title = if filtered {
            format!(
                " Summary (Filtered {}/{} turns) ",
                self.view.turn_indices.len(),
                self.report.turns.len()
            )
        } else if let Some(span) = format_time_span(
            self.report.first_turn_local.as_deref(),
            self.report.last_turn_local.as_deref(),
            self.report.duration_secs,
            arrow,
        ) {
            format!(" Summary {sep} {span} ")
        } else {
            " Summary ".to_string()
        };

        let inner_w = usize::from(area.width.saturating_sub(2));
        let sep_str = format!(" {sep} ");
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut used = 0usize;
        let mut push_segment = |label: &str, value: String, value_style: Style| {
            let prefix = if spans.is_empty() {
                " "
            } else {
                sep_str.as_str()
            };
            let seg_w = prefix.width() + label.width() + value.width();
            if !spans.is_empty() && used + seg_w > inner_w {
                return;
            }
            spans.push(Span::styled(
                prefix.to_string(),
                if spans.is_empty() {
                    Style::default()
                } else {
                    pal.dim()
                },
            ));
            spans.push(Span::styled(label.to_string(), pal.dim()));
            spans.push(Span::styled(value, value_style));
            used += seg_w;
        };

        let s = &self.view.summary;
        let turns_val = if filtered {
            format!("{}/{}", s.requests, self.report.turns.len())
        } else {
            s.requests.to_string()
        };
        push_segment("Turns ", turns_val, pal.bold(Color::White));
        push_segment("Total ", fmt_tok(s.total_tokens), pal.bold(Color::Cyan));
        let hit_pct = s.cache_hit_pct();
        push_segment(
            "Hit ",
            format_pct(hit_pct),
            pal.cache_hit_style(hit_pct).add_modifier(Modifier::BOLD),
        );
        if let Some(peak) = self.view.peak_context_tokens {
            push_segment("Peak ", fmt_tok(peak), pal.bold(Color::White));
        }
        if self.view.show_cost && s.priced_requests > 0 {
            push_segment(
                "Cost ",
                format_usd_short(s.priced_usd),
                pal.bold(Color::Green),
            );
        }
        let spd = self
            .view
            .median_tok_per_sec
            .map_or_else(|| "-".to_string(), |v| format!("{v:.0}t/s"));
        let ttft = self
            .view
            .median_ttft_secs
            .map_or_else(|| "-".to_string(), format_ttft_compact);
        push_segment("Med ", format!("{spd}/{ttft}"), pal.bold(Color::White));

        if !filtered && self.report.mid_session_misses > 0 {
            let miss_val = if self.report.mid_session_miss_usd > 0.0 {
                format!(
                    "{} miss ({})",
                    self.report.mid_session_misses,
                    format_usd_short(self.report.mid_session_miss_usd)
                )
            } else {
                format!("{} miss", self.report.mid_session_misses)
            };
            push_segment("", miss_val, pal.bold(Color::Red));
        }

        frame.render_widget(
            Paragraph::new(Line::from(spans)).block(pal.pane_block(title, false)),
            area,
        );
    }

    fn draw_turns_pane(&mut self, frame: &mut Frame<'_>, area: Rect, pal: &Palette) -> TableHit {
        let active = self.active_pane == DetailPane::Turns;
        let sep = pal.sep();
        let count_label = if self.has_active_filters() {
            format!(
                "{}/{}",
                self.view.turn_indices.len(),
                self.report.turns.len()
            )
        } else {
            self.view.turn_indices.len().to_string()
        };
        let title = format!(
            " 1. Turns ({count_label} {sep} {}) ",
            self.sort.label(pal.ascii)
        );

        if self.view.turn_indices.is_empty() {
            let msg = if self.has_active_filters() {
                "  No turns match active filters (press Esc to clear)."
            } else {
                "  No turns recorded in this conversation."
            };
            frame.render_widget(
                Paragraph::new(Line::styled(msg, pal.dim())).block(pal.pane_block(title, active)),
                area,
            );
            return TableHit {
                pane: area,
                ..Default::default()
            };
        }

        let inner_w = usize::from(area.width.saturating_sub(2));
        let has_footer = usize::from(area.height.saturating_sub(2)) >= 5;
        let cols = turn_columns(
            inner_w,
            self.view.show_model_col,
            self.view.show_cost,
            self.view.multi_day,
        );
        let widths: Vec<Constraint> = cols
            .iter()
            .map(|c| c.constraint(self.view.multi_day, self.sort.field == c.sort_field()))
            .collect();
        let header = Row::new(cols.iter().map(|c| c.header(self.sort, pal))).style(pal.dim());
        let chrome = TableChrome {
            highlight: true,
            header: true,
            footer: has_footer,
            border: true,
        };
        self.turns_header_hits = compute_header_hits(area, &cols, &widths, chrome, 2);

        let val_style = pal.bold(Color::Cyan);
        let summary_hit = self.view.summary.cache_hit_pct();
        let summary_cache = self
            .view
            .summary
            .cache_read_tokens
            .saturating_add(self.view.summary.cache_write_tokens);

        let footer_opt = has_footer.then(|| {
            let cells = cols.iter().map(|col| match col {
                TurnCol::Idx => right_cell("Tot", val_style),
                TurnCol::Time => {
                    Cell::from(format!("{}t", self.view.summary.requests)).style(val_style)
                }
                TurnCol::Model => Cell::from(""),
                TurnCol::Ctx => right_cell(
                    self.view
                        .peak_context_tokens
                        .map_or_else(|| "-".to_string(), fmt_tok),
                    val_style,
                ),
                TurnCol::Input => right_cell(fmt_tok(self.view.summary.input_tokens), val_style),
                TurnCol::Cache => right_cell(fmt_tok(summary_cache), val_style),
                TurnCol::Output => right_cell(fmt_tok(self.view.summary.output_tokens), val_style),
                TurnCol::Hit => right_cell(
                    format_pct(summary_hit),
                    pal.cache_hit_style(summary_hit)
                        .add_modifier(Modifier::BOLD),
                ),
                TurnCol::Speed => right_cell(
                    self.view
                        .median_tok_per_sec
                        .map_or_else(|| "-".to_string(), |s| format!("{s:.0}")),
                    val_style,
                ),
                TurnCol::Ttft => right_cell(
                    self.view
                        .median_ttft_secs
                        .map_or_else(|| "-".to_string(), format_ttft_compact),
                    val_style,
                ),
                TurnCol::Cost => right_cell(
                    if self.view.summary.priced_requests > 0 {
                        format_usd_short(self.view.summary.priced_usd)
                    } else {
                        "-".to_string()
                    },
                    pal.bold(Color::Green),
                ),
            });
            Row::new(cells).style(val_style)
        });

        let ellipsis = if pal.ascii { "..." } else { "…" };
        let rows: Vec<Row<'static>> = self
            .view
            .turn_indices
            .iter()
            .map(|&idx| {
                let t = &self.report.turns[idx];
                let ctx = turn_prompt_context(t);
                let cache_tot = t.cache_read_tokens.saturating_add(t.cache_write_tokens);
                let cells = cols.iter().map(|col| match col {
                    TurnCol::Idx => right_cell(format!("#{}", t.index), pal.dim()),
                    TurnCol::Time => Cell::from(
                        format_turn_time(t.timestamp_local.as_deref(), self.view.multi_day)
                            .to_string(),
                    ),
                    TurnCol::Model => {
                        let raw = if t.is_subagent {
                            format!("sub:{}", t.model)
                        } else {
                            t.model.clone()
                        };
                        let style = if t.is_subagent {
                            pal.fg(Color::Magenta)
                        } else {
                            Style::default()
                        };
                        Cell::from(truncate_to_width(&raw, 18, ellipsis)).style(style)
                    }
                    TurnCol::Ctx => right_cell(fmt_tok(ctx), Style::default()),
                    TurnCol::Input => right_cell(fmt_tok(t.input_tokens), Style::default()),
                    TurnCol::Cache => right_cell(fmt_tok(cache_tot), Style::default()),
                    TurnCol::Output => right_cell(fmt_tok(t.output_tokens), Style::default()),
                    TurnCol::Hit => right_cell(
                        format_pct(t.cache_hit_pct),
                        pal.cache_hit_style(t.cache_hit_pct),
                    ),
                    TurnCol::Speed => right_cell(
                        t.tok_per_sec
                            .map_or_else(|| "-".to_string(), |s| format!("{s:.0}")),
                        if t.tok_per_sec.is_some() {
                            Style::default()
                        } else {
                            pal.dim()
                        },
                    ),
                    TurnCol::Ttft => right_cell(
                        t.ttft_secs
                            .map_or_else(|| "-".to_string(), format_ttft_compact),
                        ttft_style(t.ttft_secs, pal),
                    ),
                    TurnCol::Cost => right_cell(
                        t.priced_usd
                            .map_or_else(|| "-".to_string(), format_usd_short),
                        cost_style(t.priced_usd, pal),
                    ),
                });
                Row::new(cells)
            })
            .collect();

        let mut table = Table::new(rows, widths)
            .header(header)
            .block(pal.pane_block(title, active))
            .column_spacing(1)
            .row_highlight_style(pal.selected_row(active))
            .highlight_symbol(if pal.ascii { "> " } else { "▸ " });
        if let Some(footer) = footer_opt {
            table = table.footer(footer);
        }

        render_table_with_scrollbar(
            frame,
            table,
            (area, area),
            &mut self.turns_table,
            self.turn_idx,
            self.view.turn_indices.len(),
            chrome,
        )
    }

    fn draw_turn_detail_pane(&self, frame: &mut Frame<'_>, area: Rect, pal: &Palette) {
        let Some(&raw_idx) = self.view.turn_indices.get(self.turn_idx) else {
            frame.render_widget(
                Paragraph::new(Line::styled("  Select a turn to inspect.", pal.dim()))
                    .block(pal.pane_block(" 2. Turn Detail ".to_string(), false)),
                area,
            );
            return;
        };

        let t = &self.report.turns[raw_idx];
        let sep = pal.sep();
        let title = format!(" 2. Turn #{} ", t.index);
        let ts = t.timestamp_local.as_deref().unwrap_or("-");
        let ctx = turn_prompt_context(t);
        let cost_str = t
            .priced_usd
            .map_or_else(|| "-".to_string(), format_usd_short);
        let spd_str = t
            .tok_per_sec
            .map_or_else(|| "-".to_string(), |s| format!("{s:.1} tok/s"));
        let ttft_str = t
            .ttft_secs
            .map_or_else(|| "-".to_string(), format_ttft_compact);

        let turn_summary = PeriodSummary {
            requests: 1,
            input_tokens: t.input_tokens,
            cache_read_tokens: t.cache_read_tokens,
            cache_write_tokens: t.cache_write_tokens,
            output_tokens: t.output_tokens,
            thinking_tokens: t.thinking_tokens,
            total_tokens: t.total_tokens,
            priced_usd: t.priced_usd.unwrap_or(0.0),
            priced_requests: u64::from(t.priced_usd.is_some()),
            unpriced_requests: u64::from(t.priced_usd.is_none()),
        };

        let owner_span = if t.is_subagent {
            let sub_tag = self
                .view
                .subagent_short_labels
                .get(&t.conversation_id)
                .cloned()
                .unwrap_or_else(|| t.conversation_id.chars().take(8).collect());
            Span::styled(format!("sub:{sub_tag}"), pal.bold(Color::Magenta))
        } else {
            Span::styled(t.model.clone(), pal.bold(Color::White))
        };

        let mut lines = vec![
            Line::from(vec![
                Span::raw(" "),
                Span::styled(ts.to_string(), pal.dim()),
                Span::styled(format!(" {sep} "), pal.dim()),
                Span::styled(cost_str, pal.bold(Color::Green)),
            ]),
            Line::from(vec![
                Span::raw(" "),
                owner_span,
                Span::styled(format!(" {sep} {spd_str} {sep} "), pal.dim()),
                Span::styled(ttft_str, ttft_style(t.ttft_secs, pal)),
            ]),
            Line::from(vec![
                Span::styled(" Ctx ", pal.dim()),
                Span::styled(fmt_tok(ctx), pal.bold(Color::Cyan)),
                Span::styled(format!(" {sep} Hit "), pal.dim()),
                Span::styled(
                    format_pct(t.cache_hit_pct),
                    pal.cache_hit_style(t.cache_hit_pct)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!(" {sep} Tot "), pal.dim()),
                Span::styled(fmt_tok(t.total_tokens), pal.bold(Color::White)),
            ]),
        ];
        lines.extend(token_breakdown_lines(&turn_summary, area, pal));
        frame.render_widget(
            Paragraph::new(lines).block(pal.pane_block(title, false)),
            area,
        );
    }

    fn draw_breakdown_pane(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        pal: &Palette,
    ) -> TableHit {
        let active = self.active_pane == DetailPane::Breakdown;
        let has_subs = !self.report.subagents.is_empty();
        let title = match (self.right_tab, has_subs) {
            (RightTab::Models, true) => format!(" 3. Models ({}) (v) ", self.report.models.len()),
            (RightTab::Models, false) => format!(" 3. Models ({}) ", self.report.models.len()),
            (RightTab::Subagents, _) => {
                format!(" 3. Subagents ({}) (v) ", self.report.subagents.len())
            }
        };

        let item_count = self.breakdown_len();
        if item_count == 0 {
            frame.render_widget(
                Paragraph::new(Line::styled("  No entries recorded.", pal.dim()))
                    .block(pal.pane_block(title, active)),
                area,
            );
            return TableHit {
                pane: area,
                ..Default::default()
            };
        }

        let show_hit = usize::from(area.width.saturating_sub(2)) >= 34;
        let show_cost = self.report.totals.priced_requests > 0;
        let filter_glyph = if pal.ascii { "* " } else { "● " };

        let rows: Vec<Row<'static>> = match self.right_tab {
            RightTab::Models => self
                .report
                .models
                .iter()
                .map(|m| {
                    let is_filtered = self.model_filter.as_deref() == Some(m.model.as_str());
                    let name = if is_filtered {
                        format!("{filter_glyph}{}", m.model)
                    } else {
                        m.model.clone()
                    };
                    let (val_str, val_dim) = if show_cost {
                        if m.priced_requests() > 0 {
                            (format_usd_short(m.priced_usd), false)
                        } else {
                            ("-".to_string(), true)
                        }
                    } else {
                        (fmt_tok(m.total_tokens), false)
                    };
                    let mut cells = vec![
                        Cell::from(name).style(if is_filtered {
                            pal.bold(Color::Yellow)
                        } else {
                            Style::default()
                        }),
                        right_cell(format!("{}t", m.requests), pal.dim()),
                        right_cell(
                            val_str,
                            if val_dim {
                                pal.dim()
                            } else {
                                pal.bold(if is_filtered {
                                    Color::Yellow
                                } else {
                                    Color::Cyan
                                })
                            },
                        ),
                    ];
                    if show_hit {
                        let hit = m.cache_hit_pct();
                        cells.push(right_cell(format_pct(hit), pal.cache_hit_style(hit)));
                    }
                    Row::new(cells)
                })
                .collect(),
            RightTab::Subagents => self
                .report
                .subagents
                .iter()
                .map(|sub| {
                    let is_filtered =
                        self.subagent_filter.as_deref() == Some(sub.conversation_id.as_str());
                    let base = format_subagent_short_label(
                        &sub.conversation_id,
                        sub.agent_name.as_deref(),
                    );
                    let name = if is_filtered {
                        format!("{filter_glyph}{base}")
                    } else {
                        base
                    };
                    let (val_str, val_dim) = if show_cost {
                        if sub.summary.priced_requests > 0 {
                            (format_usd_short(sub.summary.priced_usd), false)
                        } else {
                            ("-".to_string(), true)
                        }
                    } else {
                        (fmt_tok(sub.summary.total_tokens), false)
                    };
                    let mut cells = vec![
                        Cell::from(name).style(if is_filtered {
                            pal.bold(Color::Yellow)
                        } else {
                            pal.fg(Color::Magenta)
                        }),
                        right_cell(format!("{}t", sub.summary.requests), pal.dim()),
                        right_cell(
                            val_str,
                            if val_dim {
                                pal.dim()
                            } else {
                                pal.bold(if is_filtered {
                                    Color::Yellow
                                } else {
                                    Color::Cyan
                                })
                            },
                        ),
                    ];
                    if show_hit {
                        let hit = sub.summary.cache_hit_pct();
                        cells.push(right_cell(format_pct(hit), pal.cache_hit_style(hit)));
                    }
                    Row::new(cells)
                })
                .collect(),
        };

        let widths = if show_hit {
            vec![
                Constraint::Min(10),
                Constraint::Length(4),
                Constraint::Length(7),
                Constraint::Length(6),
            ]
        } else {
            vec![
                Constraint::Min(10),
                Constraint::Length(4),
                Constraint::Length(7),
            ]
        };

        let table = Table::new(rows, widths)
            .block(pal.pane_block(title, active))
            .column_spacing(1)
            .row_highlight_style(pal.selected_row(active))
            .highlight_symbol(if pal.ascii { "> " } else { "▸ " });

        render_table_with_scrollbar(
            frame,
            table,
            (area, area),
            &mut self.breakdown_table,
            self.breakdown_idx,
            item_count,
            TableChrome {
                highlight: active,
                header: false,
                footer: false,
                border: true,
            },
        )
    }

    fn draw_footer(&self, frame: &mut Frame<'_>, area: Rect, has_back: bool, pal: &Palette) {
        let sep = format!(" {} ", pal.sep());
        let back_hint = if has_back {
            format!("{sep}b Back")
        } else {
            String::new()
        };
        let text = if let Some(msg) = &self.status_msg {
            format!(" {msg}")
        } else if self.has_active_filters() {
            format!(" Esc Clear{sep}c Convs{sep}s/S Sort{sep}m Misses{sep}Tab Pane{sep}r Refresh{back_hint}{sep}q Quit")
        } else if area.width < 92 {
            format!(" c Convs{sep}s/S Sort{sep}m Misses{sep}Tab Pane{back_hint}{sep}q Quit")
        } else {
            let sub_hint = if self.report.subagents.is_empty() {
                String::new()
            } else {
                format!("{sep}v Models/Sub")
            };
            let nav = if pal.ascii { "j/k" } else { "↑↓/jk" };
            format!(" {nav} Select{sep}c Convs{sep}s/S Sort (click header){sep}m Misses{sub_hint}{sep}Tab Pane{sep}r Refresh{back_hint}{sep}q Quit")
        };
        let style = if self.status_msg.is_some() {
            pal.bold(Color::Yellow)
        } else {
            pal.dim()
        };
        frame.render_widget(Paragraph::new(Line::styled(text, style)), area);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnCol {
    Idx,
    Time,
    Model,
    Ctx,
    Input,
    Cache,
    Output,
    Hit,
    Speed,
    Ttft,
    Cost,
}

impl TurnCol {
    const fn sort_field(self) -> TurnSortField {
        match self {
            Self::Idx => TurnSortField::Turn,
            Self::Time => TurnSortField::Time,
            Self::Model => TurnSortField::Model,
            Self::Ctx => TurnSortField::Context,
            Self::Input => TurnSortField::Input,
            Self::Cache => TurnSortField::Cache,
            Self::Output => TurnSortField::Output,
            Self::Hit => TurnSortField::CacheHit,
            Self::Speed => TurnSortField::Speed,
            Self::Ttft => TurnSortField::Ttft,
            Self::Cost => TurnSortField::Cost,
        }
    }

    const fn is_right_aligned(self) -> bool {
        !matches!(self, Self::Time | Self::Model)
    }

    fn constraint(self, multi_day: bool, active: bool) -> Constraint {
        match self {
            Self::Idx => Constraint::Length(4),
            Self::Time => Constraint::Length(if multi_day { 11 } else { 8 }),
            Self::Model => Constraint::Min(12),
            Self::Ctx | Self::Hit | Self::Ttft => Constraint::Length(6),
            Self::Input | Self::Cache => Constraint::Length(if active { 7 } else { 6 }),
            Self::Output => Constraint::Length(5),
            Self::Speed => Constraint::Length(if active { 7 } else { 5 }),
            Self::Cost => Constraint::Length(7),
        }
    }

    fn header(self, sort: TurnSort, pal: &Palette) -> Cell<'static> {
        let field = self.sort_field();
        let active = sort.field == field;
        let label = if active {
            format!("{} {}", field.header_label(), sort.dir.arrow(pal.ascii))
        } else {
            field.header_label().to_string()
        };
        let style = if active {
            if pal.no_color {
                Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
            } else {
                pal.bold(Color::Cyan)
            }
        } else {
            pal.dim()
        };
        if self.is_right_aligned() {
            right_cell(label, style)
        } else {
            Cell::from(label).style(style)
        }
    }
}

fn turn_columns(
    inner_w: usize,
    show_model: bool,
    show_cost: bool,
    multi_day: bool,
) -> Vec<TurnCol> {
    use TurnCol::*;
    let extra = usize::from(show_model) * 14 + usize::from(multi_day) * 3;
    let mut cols: Vec<TurnCol> = if inner_w >= 72 + extra {
        vec![
            Idx, Time, Model, Ctx, Input, Cache, Output, Hit, Speed, Ttft, Cost,
        ]
    } else if inner_w >= 58 + extra {
        vec![Idx, Time, Model, Ctx, Output, Hit, Speed, Ttft, Cost]
    } else if inner_w >= 46 + extra {
        vec![Idx, Time, Model, Ctx, Hit, Ttft, Cost]
    } else {
        vec![Idx, Time, Ctx, Hit, Cost]
    };
    if !show_model {
        cols.retain(|&c| c != Model);
    }
    if !show_cost {
        cols.retain(|&c| c != Cost);
    }
    cols
}

fn ttft_style(ttft: Option<f64>, pal: &Palette) -> Style {
    match ttft {
        Some(t) if t >= 60.0 => pal.fg(Color::Red),
        Some(t) if t >= 15.0 => pal.fg(Color::Yellow),
        Some(_) => Style::default(),
        None => pal.dim(),
    }
}

fn cost_style(cost: Option<f64>, pal: &Palette) -> Style {
    match cost {
        Some(c) if c >= 0.50 => pal.fg(Color::Yellow),
        Some(_) => Style::default(),
        None => pal.dim(),
    }
}

#[cfg(test)]
mod tests {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    use super::*;
    use crate::stats::metric::cache_hit_pct;
    use crate::stats::{ModelStats, SqliteStats, SubagentConversationStats};

    #[allow(clippy::too_many_arguments)]
    fn make_turn(
        index: usize,
        conv_id: &str,
        is_subagent: bool,
        ts: &str,
        model: &str,
        input: u64,
        cache_r: u64,
        out: u64,
        usd: f64,
    ) -> ConversationTurnStats {
        ConversationTurnStats {
            index,
            conversation_id: conv_id.to_string(),
            is_subagent,
            timestamp_local: Some(ts.to_string()),
            model: model.to_string(),
            input_tokens: input,
            cache_read_tokens: cache_r,
            cache_write_tokens: 0,
            output_tokens: out,
            thinking_tokens: out / 3,
            total_tokens: input + cache_r + out,
            cache_hit_pct: cache_hit_pct(input, cache_r, 0),
            priced_usd: Some(usd),
            tok_per_sec: Some(220.0),
            ttft_secs: Some(1.4),
        }
    }

    fn sample_report() -> ConversationStatsReport {
        let root_id = "8d0371e1-5a05-423b-a8bc-ed75995f9a8b";
        let sub_id = "9a1482f2-6b16-434c-99cd-fe86aa60ab9c";
        ConversationStatsReport {
            conversation_id: root_id.to_string(),
            title: Some("Cache Hit Investigation".to_string()),
            source: "cli".to_string(),
            timezone: "UTC+08:00".to_string(),
            first_turn_local: Some("2026-11-24 10:00:00".to_string()),
            last_turn_local: Some("2026-11-24 10:12:30".to_string()),
            duration_secs: Some(750),
            peak_context_tokens: Some(150_000),
            mid_session_misses: 1,
            mid_session_miss_usd: 0.62,
            totals: PeriodSummary {
                requests: 3,
                input_tokens: 162_000,
                cache_read_tokens: 118_000,
                output_tokens: 3_500,
                thinking_tokens: 1_100,
                total_tokens: 283_500,
                priced_usd: 0.74,
                priced_requests: 3,
                ..Default::default()
            },
            models: vec![
                ModelStats {
                    model: "Model Alpha".to_string(),
                    requests: 2,
                    input_tokens: 160_000,
                    cache_read_tokens: 110_000,
                    total_tokens: 273_000,
                    priced_usd: 0.72,
                    ..Default::default()
                },
                ModelStats {
                    model: "Model Beta".to_string(),
                    requests: 1,
                    input_tokens: 2_000,
                    cache_read_tokens: 8_000,
                    total_tokens: 10_500,
                    priced_usd: 0.02,
                    ..Default::default()
                },
            ],
            subagents: vec![SubagentConversationStats {
                conversation_id: sub_id.to_string(),
                agent_name: Some("research".to_string()),
                summary: PeriodSummary {
                    requests: 1,
                    input_tokens: 2_000,
                    cache_read_tokens: 8_000,
                    total_tokens: 10_500,
                    priced_usd: 0.02,
                    priced_requests: 1,
                    ..Default::default()
                },
                ..Default::default()
            }],
            turns: vec![
                make_turn(
                    1,
                    root_id,
                    false,
                    "2026-11-24 10:00:00",
                    "Model Alpha",
                    10_000,
                    110_000,
                    1_500,
                    0.10,
                ),
                make_turn(
                    2,
                    sub_id,
                    true,
                    "2026-11-24 10:05:00",
                    "Model Beta",
                    2_000,
                    8_000,
                    500,
                    0.02,
                ),
                make_turn(
                    3,
                    root_id,
                    false,
                    "2026-11-24 10:12:30",
                    "Model Alpha",
                    150_000,
                    0,
                    1_500,
                    0.62,
                ),
            ],
            sqlite: SqliteStats {
                db_files: 2,
                completed_turns: 3,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn detail_screen_handles_sort_filters_render_and_mouse() {
        let mut screen = DetailScreen::new(sample_report(), TurnSort::default());
        assert_eq!(screen.sort, TurnSort::default());
        assert_eq!(screen.view.turn_indices, vec![0, 1, 2]);

        // 'S' reverses current sort direction (# ↑ -> # ↓); 's' cycles visible columns.
        screen.handle_key(KeyCode::Char('S'), KeyModifiers::NONE, false);
        assert_eq!(
            screen.sort,
            TurnSort {
                field: TurnSortField::Turn,
                dir: SortDir::Desc,
            }
        );
        assert_eq!(screen.view.turn_indices, vec![2, 1, 0]);
        screen.handle_key(KeyCode::Char('s'), KeyModifiers::NONE, false);
        assert_eq!(
            screen.sort,
            TurnSort {
                field: TurnSortField::Time,
                dir: SortDir::Asc,
            }
        );
        screen.click_sort_field(TurnSortField::Turn);
        assert_eq!(screen.sort, TurnSort::default());
        assert_eq!(screen.view.turn_indices, vec![0, 1, 2]);

        // 'm' filters to cache anomalies (<50%) -> turn #3 only; Esc clears filters before quitting.
        screen.handle_key(KeyCode::Char('m'), KeyModifiers::NONE, false);
        assert_eq!(screen.view.turn_indices, vec![2]);
        assert_eq!(
            screen.handle_key(KeyCode::Esc, KeyModifiers::NONE, false),
            DetailAction::Continue
        );
        assert_eq!(screen.view.turn_indices.len(), 3);

        // Render wide viewport and verify exact mouse column-header click sorting + row clicks.
        let pal = Palette {
            no_color: true,
            ascii: false,
        };
        let mut terminal = Terminal::new(TestBackend::new(120, 32)).unwrap();
        terminal
            .draw(|frame| screen.draw(frame, true, &pal))
            .unwrap();
        let wide: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(wide.contains("Conversation 8d0371e1"), "{wide}");
        assert!(wide.contains("● Live"), "{wide}");
        assert!(wide.contains("1 miss (~$0.62)"), "{wide}");
        assert!(wide.contains("sub:Model Beta"), "{wide}");
        assert!(wide.contains("b Back"), "{wide}");

        // Verify computed Cost header hit box actually contains the rendered "Cost" text.
        let header_y = screen.turns_header_hits.y;
        let cost_hit = screen
            .turns_header_hits
            .cols
            .iter()
            .find(|c| c.col == TurnCol::Cost)
            .copied()
            .unwrap();
        let cost_header_text: String = (cost_hit.x0..cost_hit.x1)
            .map(|x| terminal.backend().buffer()[(x, header_y)].symbol())
            .collect();
        assert!(cost_header_text.contains("Cost"), "{cost_header_text:?}");

        let m = |kind, column, row| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };

        // Click Cost header -> sorts Cost descending (turn #3 $0.62, #1 $0.10, #2 $0.02).
        screen.handle_mouse(m(
            MouseEventKind::Down(MouseButton::Left),
            cost_hit.x0 + 1,
            header_y,
        ));
        assert_eq!(
            screen.sort,
            TurnSort {
                field: TurnSortField::Cost,
                dir: SortDir::Desc,
            }
        );
        assert_eq!(screen.view.turn_indices, vec![2, 0, 1]);

        // Click Cost header again -> toggles to Cost ascending (turn #2 $0.02, #1 $0.10, #3 $0.62).
        screen.handle_mouse(m(
            MouseEventKind::Down(MouseButton::Left),
            cost_hit.x0 + 1,
            header_y,
        ));
        assert_eq!(
            screen.sort,
            TurnSort {
                field: TurnSortField::Cost,
                dir: SortDir::Asc,
            }
        );
        assert_eq!(screen.view.turn_indices, vec![1, 0, 2]);

        // Click '#' header -> restores default turn ascending.
        let idx_hit = screen
            .turns_header_hits
            .cols
            .iter()
            .find(|c| c.col == TurnCol::Idx)
            .copied()
            .unwrap();
        screen.handle_mouse(m(
            MouseEventKind::Down(MouseButton::Left),
            idx_hit.x0,
            header_y,
        ));
        assert_eq!(screen.sort, TurnSort::default());
        assert_eq!(screen.view.turn_indices, vec![0, 1, 2]);

        let tx = screen.turns_hit.pane.x + 4;
        let t2_y = screen.turns_hit.data_y0 + 2;
        screen.handle_mouse(m(MouseEventKind::Down(MouseButton::Left), tx, t2_y));
        assert_eq!(screen.turn_idx, 2);
        screen.handle_mouse(m(MouseEventKind::ScrollUp, tx, t2_y));
        assert_eq!(screen.turn_idx, 0);

        let bx = screen.breakdown_hit.pane.x + 3;
        let b0_y = screen.breakdown_hit.data_y0;
        screen.handle_mouse(m(MouseEventKind::Down(MouseButton::Left), bx, b0_y));
        assert_eq!(screen.active_pane, DetailPane::Breakdown);
        assert!(screen.model_filter.is_none());
        screen.handle_mouse(m(MouseEventKind::Down(MouseButton::Left), bx, b0_y));
        assert_eq!(screen.model_filter.as_deref(), Some("Model Alpha"));

        // Clicking breakdown title toggles Models -> Subagents.
        screen.handle_mouse(m(
            MouseEventKind::Down(MouseButton::Left),
            bx,
            screen.breakdown_hit.pane.y,
        ));
        assert_eq!(screen.right_tab, RightTab::Subagents);
    }

    #[test]
    fn detail_screen_live_update_follows_tail_in_default_asc_and_pins_mid_selection() {
        let root_id = "8d0371e1-5a05-423b-a8bc-ed75995f9a8b";
        let mut screen = DetailScreen::new(sample_report(), TurnSort::default());
        // Select the last turn (#3) while in default `# ↑` mode.
        screen.turn_idx = 2;

        let mut updated = sample_report();
        updated.turns.push(make_turn(
            4,
            root_id,
            false,
            "2026-11-24 10:15:00",
            "Model Alpha",
            8_000,
            92_000,
            900,
            0.05,
        ));
        screen.apply_live_report(updated.clone());
        assert_eq!(screen.view.turn_indices.len(), 4);
        assert_eq!(screen.turn_idx, 3);

        // Selecting an earlier turn (#1) stays pinned when another turn arrives.
        screen.turn_idx = 0;
        updated.turns.push(make_turn(
            5,
            root_id,
            false,
            "2026-11-24 10:16:00",
            "Model Alpha",
            9_000,
            95_000,
            1_000,
            0.06,
        ));
        screen.apply_live_report(updated);
        assert_eq!(screen.view.turn_indices.len(), 5);
        assert_eq!(screen.turn_idx, 0);
    }
}
