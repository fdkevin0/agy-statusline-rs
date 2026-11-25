//! Unified interactive `ratatui` + `crossterm` TUI for `agy-statusline stats` and `conversations`.
//!
//! Single terminal session & navigation stack (`Dashboard` <-> `Conversations` <-> `Detail`)
//! with shared styling, scrollable table/scrollbar/hit-test primitives, and token breakdown bar.

mod dashboard;
mod dashboard_state;
mod detail;
mod picker;

use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyEventKind};
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Scrollbar, ScrollbarOrientation, ScrollbarState, Table,
    TableState,
};
use ratatui::Frame;

use super::metric::{ConversationSort, StatsMetric, StatsTuiPrefs, TurnSort};
use super::pricing::{
    fetch_models_dev_via_curl, PricingCatalog, SyncMode, DEFAULT_PRICING_TTL_SECS,
};
use super::scan::{
    collect_conversation_stats_report, conversation_watch_fingerprint,
    conversations_list_fingerprint, list_conversations, ConversationWatchFingerprint,
    ConversationsListFingerprint,
};
use super::{ConversationStatsReport, PeriodSummary};
use crate::sys::fmt::format_tokens_string as fmt_tok;
use crate::sys::terminal::enter_tui;
use crate::sys::time::TimeZoneSpec;

use dashboard::DashboardScreen;
use dashboard_state::StatsAction;
use detail::{DetailAction, DetailScreen};
use picker::{PickerAction, PickerScreen};

pub(super) struct Palette {
    pub(super) no_color: bool,
    pub(super) ascii: bool,
}

impl Palette {
    pub(super) fn fg(&self, fg: Color) -> Style {
        if self.no_color {
            Style::default()
        } else {
            Style::default().fg(fg)
        }
    }

    pub(super) fn bold(&self, fg: Color) -> Style {
        self.fg(fg).add_modifier(Modifier::BOLD)
    }

    pub(super) fn dim(&self) -> Style {
        if self.no_color {
            Style::default().add_modifier(Modifier::DIM)
        } else {
            Style::default().fg(Color::DarkGray)
        }
    }

    /// Threshold style for cache hit percentages (`>=80%` green, `50–79%` yellow, `<50%` red).
    pub(super) fn cache_hit_style(&self, pct: Option<f64>) -> Style {
        let Some(p) = pct.filter(|p| p.is_finite()) else {
            return self.dim();
        };
        if self.no_color {
            return Style::default();
        }
        if p >= 80.0 {
            Style::default().fg(Color::Green)
        } else if p >= 50.0 {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::Red)
        }
    }

    pub(super) fn pill(&self, active: bool) -> Style {
        if !active {
            return self.fg(Color::Gray);
        }
        if self.no_color {
            Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            Style::default()
                .bg(Color::Cyan)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD)
        }
    }

    pub(super) fn selected_row(&self, pane_active: bool) -> Style {
        if self.no_color {
            return Style::default().add_modifier(if pane_active {
                Modifier::REVERSED | Modifier::BOLD
            } else {
                Modifier::UNDERLINED | Modifier::BOLD
            });
        }
        let base = Style::default()
            .bg(Color::Indexed(if pane_active { 238 } else { 236 }))
            .fg(Color::White);
        if pane_active {
            base.add_modifier(Modifier::BOLD)
        } else {
            base
        }
    }

    pub(super) fn heatmap(&self, level: usize, selected: bool) -> Style {
        if selected {
            return self.pill(true);
        }
        if self.no_color {
            return match level {
                0 => Style::default().add_modifier(Modifier::DIM),
                4 => Style::default().add_modifier(Modifier::BOLD),
                _ => Style::default(),
            };
        }
        match level {
            0 => Style::default().fg(Color::DarkGray),
            1 => Style::default().fg(Color::Indexed(29)),
            2 => Style::default().fg(Color::Indexed(36)),
            3 => Style::default().fg(Color::Indexed(42)),
            _ => Style::default()
                .fg(Color::Indexed(78))
                .add_modifier(Modifier::BOLD),
        }
    }

    pub(super) fn sep(&self) -> &'static str {
        if self.ascii {
            "|"
        } else {
            "·"
        }
    }

    pub(super) fn pane_block(&self, title: String, active: bool) -> Block<'static> {
        let border_style = if self.no_color {
            Style::default().add_modifier(if active {
                Modifier::BOLD
            } else {
                Modifier::DIM
            })
        } else {
            Style::default().fg(if active { Color::Cyan } else { Color::DarkGray })
        };
        let title_style = if self.no_color {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(if active { Color::Cyan } else { Color::Gray })
                .add_modifier(Modifier::BOLD)
        };
        Block::default()
            .borders(Borders::ALL)
            .border_type(if self.ascii {
                BorderType::Plain
            } else {
                BorderType::Rounded
            })
            .border_style(border_style)
            .title(Span::styled(title, title_style))
    }
}

pub(super) fn right_cell(text: impl Into<String>, style: Style) -> Cell<'static> {
    Cell::from(Text::from(text.into()).alignment(Alignment::Right)).style(style)
}

/// Exact horizontal click zone for a single table header column.
#[derive(Debug, Clone, Copy)]
pub(super) struct ColumnHit<C> {
    pub(super) col: C,
    pub(super) x0: u16,
    pub(super) x1: u16,
}

/// Row and column bounds for clicking table headers to change sort order.
#[derive(Debug, Clone)]
pub(super) struct HeaderHits<C> {
    pub(super) y: u16,
    pub(super) cols: Vec<ColumnHit<C>>,
}

impl<C> Default for HeaderHits<C> {
    fn default() -> Self {
        Self {
            y: u16::MAX,
            cols: Vec::new(),
        }
    }
}

impl<C: Copy> HeaderHits<C> {
    pub(super) fn col_at(&self, pos: Position) -> Option<C> {
        if pos.y != self.y {
            return None;
        }
        self.cols
            .iter()
            .find(|c| pos.x >= c.x0 && pos.x < c.x1)
            .map(|c| c.col)
    }
}

/// Computes horizontal column header click zones matching `ratatui::widgets::Table` layout.
pub(super) fn compute_header_hits<C: Copy>(
    table_area: Rect,
    cols: &[C],
    widths: &[Constraint],
    chrome: TableChrome,
    highlight_symbol_width: u16,
) -> HeaderHits<C> {
    if !chrome.header || cols.is_empty() || cols.len() != widths.len() {
        return HeaderHits::default();
    }
    let border = u16::from(chrome.border);
    if table_area.height <= border * 2 {
        return HeaderHits::default();
    }
    let inner_x = table_area.x.saturating_add(border);
    let inner_w = table_area.width.saturating_sub(border * 2);
    let sym_w = if chrome.highlight {
        highlight_symbol_width.min(inner_w)
    } else {
        0
    };
    let header_y = table_area.y.saturating_add(border);
    let cols_area = Rect::new(
        inner_x.saturating_add(sym_w),
        header_y,
        inner_w.saturating_sub(sym_w),
        1,
    );
    let rects = Layout::horizontal(widths)
        .spacing(1)
        .flex(Flex::Start)
        .split(cols_area);
    let hits = cols
        .iter()
        .zip(rects.iter())
        .map(|(&col, r)| ColumnHit {
            col,
            x0: r.x,
            x1: r.right(),
        })
        .collect();
    HeaderHits {
        y: header_y,
        cols: hits,
    }
}

/// Shared hit-testing rectangle for any scrollable `ratatui::widgets::Table`.
#[derive(Default, Clone, Copy)]
pub(super) struct TableHit {
    pub(super) pane: Rect,
    pub(super) data_y0: u16,
    pub(super) data_rows: usize,
}

impl TableHit {
    pub(super) fn contains(&self, pos: Position) -> bool {
        self.pane.contains(pos)
    }

    /// Maps a terminal row inside the table body to an item index using `TableState::offset()`.
    pub(super) fn row_at(&self, pos: Position, offset: usize, len: usize) -> Option<usize> {
        if !self.pane.contains(pos) || pos.y < self.data_y0 {
            return None;
        }
        let rel = usize::from(pos.y - self.data_y0);
        if rel >= self.data_rows {
            return None;
        }
        let target = offset + rel;
        (target < len).then_some(target)
    }
}

/// Structural chrome flags for [`render_table_with_scrollbar`].
#[derive(Clone, Copy)]
pub(super) struct TableChrome {
    pub(super) highlight: bool,
    pub(super) header: bool,
    pub(super) footer: bool,
    pub(super) border: bool,
}

/// Renders a stateful table, clamps stale scroll offsets, attaches a right-border scrollbar
/// when `item_count > visible_rows`, and returns its mouse [`TableHit`] geometry.
pub(super) fn render_table_with_scrollbar(
    frame: &mut Frame<'_>,
    table: Table<'_>,
    areas: (Rect, Rect),
    table_state: &mut TableState,
    selected_idx: usize,
    item_count: usize,
    chrome: TableChrome,
) -> TableHit {
    let (table_area, scrollbar_area) = areas;
    *table_state.offset_mut() = table_state.offset().min(selected_idx);
    table_state.select(chrome.highlight.then_some(selected_idx));
    frame.render_stateful_widget(table, table_area, table_state);

    let border_top = u16::from(chrome.border);
    let chrome_v =
        usize::from(chrome.border) * 2 + usize::from(chrome.header) + usize::from(chrome.footer);
    let visible_rows = usize::from(table_area.height).saturating_sub(chrome_v);
    let data_y0 = table_area
        .y
        .saturating_add(border_top + u16::from(chrome.header));

    if item_count > visible_rows && scrollbar_area.height > border_top + u16::from(chrome.header) {
        let mut sb = ScrollbarState::new(item_count).position(selected_idx);
        let top_pad = border_top + u16::from(chrome.header);
        let bottom_pad = u16::from(chrome.border);
        let sb_rect = Rect::new(
            scrollbar_area.x,
            scrollbar_area.y.saturating_add(top_pad),
            scrollbar_area.width,
            scrollbar_area.height.saturating_sub(top_pad + bottom_pad),
        );
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None),
            sb_rect,
            &mut sb,
        );
    }

    TableHit {
        pane: table_area,
        data_y0,
        data_rows: visible_rows,
    }
}

/// Builds the proportional token composition bar + `Input`/`CacheR`/`CacheW`/`Output`/`Think` lines.
pub(super) fn token_breakdown_lines(
    s: &PeriodSummary,
    area: Rect,
    pal: &Palette,
) -> Vec<Line<'static>> {
    let bar_w = usize::from(area.width.saturating_sub(4)).clamp(10, 42);
    let bar = if s.total_tokens == 0 || bar_w == 0 {
        let empty = if pal.ascii { "." } else { "·" }.repeat(bar_w);
        Line::styled(format!(" {empty}"), pal.dim())
    } else {
        let total = s.total_tokens as f64;
        let parts = [
            (
                s.input_tokens,
                Color::Blue,
                if pal.ascii { "I" } else { "█" },
            ),
            (
                s.cache_read_tokens,
                Color::Green,
                if pal.ascii { "R" } else { "█" },
            ),
            (
                s.cache_write_tokens,
                Color::Yellow,
                if pal.ascii { "W" } else { "█" },
            ),
            (
                s.output_tokens,
                Color::Magenta,
                if pal.ascii { "O" } else { "█" },
            ),
        ];
        let non_zero: Vec<_> = parts.into_iter().filter(|(v, _, _)| *v > 0).collect();
        let mut spans = vec![Span::raw(" ")];
        let mut used = 0usize;
        for (idx, (val, color, glyph)) in non_zero.iter().enumerate() {
            let max_here = bar_w.saturating_sub(used + non_zero.len().saturating_sub(idx + 1));
            if max_here == 0 {
                continue;
            }
            let seg_w = if idx + 1 == non_zero.len() {
                bar_w.saturating_sub(used)
            } else {
                ((*val as f64 / total) * bar_w as f64).round().max(1.0) as usize
            }
            .min(max_here);
            used += seg_w;
            spans.push(Span::styled(glyph.repeat(seg_w), pal.fg(*color)));
        }
        Line::from(spans)
    };

    let pct_of = |part: u64, whole: u64| -> String {
        if whole == 0 {
            " 0.0%".to_string()
        } else {
            format!("{:5.1}%", (part as f64) * 100.0 / (whole as f64))
        }
    };
    let swatch = if pal.ascii { "*" } else { "■" };
    let branch = if pal.ascii { "`-" } else { "└" };
    let comp_row = |color: Color, label: &'static str, val: u64| -> Line<'static> {
        Line::from(vec![
            Span::styled(format!(" {swatch} "), pal.fg(color)),
            Span::styled(format!("{label:<8}"), pal.dim()),
            Span::styled(format!("{:>7}", fmt_tok(val)), pal.bold(Color::White)),
            Span::styled(format!(" ({})", pct_of(val, s.total_tokens)), pal.dim()),
        ])
    };

    let mut lines = vec![
        bar,
        comp_row(Color::Blue, "Input", s.input_tokens),
        comp_row(Color::Green, "CacheR", s.cache_read_tokens),
    ];
    if s.cache_write_tokens > 0 || area.height >= 10 {
        lines.push(comp_row(Color::Yellow, "CacheW", s.cache_write_tokens));
    }
    lines.push(comp_row(Color::Magenta, "Output", s.output_tokens));
    let think_w = if pal.ascii { 6 } else { 7 };
    lines.push(Line::from(vec![
        Span::styled(format!("   {branch} Think "), pal.dim()),
        Span::styled(
            format!("{:>think_w$}", fmt_tok(s.thinking_tokens)),
            pal.bold(Color::White),
        ),
        Span::styled(
            format!(
                " ({} out)",
                pct_of(s.thinking_tokens, s.output_tokens).trim()
            ),
            pal.dim(),
        ),
    ]));
    lines
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    Dashboard,
    Conversations,
    Detail,
}

/// Interval between lightweight metadata-only live watch checks on Conversations / Detail screens.
const LIVE_WATCH_INTERVAL: Duration = Duration::from_millis(500);

type PrefsChangeCallback =
    Box<dyn FnMut(Option<StatsMetric>, Option<TurnSort>, Option<ConversationSort>)>;

struct StatsApp {
    roots: Vec<(&'static str, PathBuf)>,
    cache_dir: Option<PathBuf>,
    tz: TimeZoneSpec,
    timezone: String,
    prefs: StatsTuiPrefs,
    on_prefs_change: Option<PrefsChangeCallback>,
    pricing: Option<PricingCatalog>,
    screen: Screen,
    origin: Screen,
    opened_from_picker: bool,
    dashboard: Option<DashboardScreen>,
    picker: Option<PickerScreen>,
    detail: Option<DetailScreen>,
    last_watch_check: Instant,
    detail_watch_fp: Option<ConversationWatchFingerprint>,
    picker_watch_fp: Option<ConversationsListFingerprint>,
}

impl StatsApp {
    fn active_metric(&self) -> StatsMetric {
        self.dashboard
            .as_ref()
            .map_or(self.prefs.metric, DashboardScreen::active_metric)
    }

    fn sync_prefs(&mut self) {
        let mut changed_metric = None;
        let mut changed_turn_sort = None;
        let mut changed_conv_sort = None;

        if let Some(d) = self.dashboard.as_ref() {
            let m = d.active_metric();
            if m != self.prefs.metric {
                self.prefs.metric = m;
                changed_metric = Some(m);
            }
        }
        if let Some(d) = self.detail.as_ref() {
            let ts = d.sort();
            if ts != self.prefs.turn_sort {
                self.prefs.turn_sort = ts;
                changed_turn_sort = Some(ts);
            }
        }
        if let Some(p) = self.picker.as_ref() {
            let cs = p.sort();
            if cs != self.prefs.conv_sort {
                self.prefs.conv_sort = cs;
                changed_conv_sort = Some(cs);
            }
        }
        if (changed_metric.is_some() || changed_turn_sort.is_some() || changed_conv_sort.is_some())
            && self.on_prefs_change.is_some()
        {
            if let Some(cb) = self.on_prefs_change.as_mut() {
                cb(changed_metric, changed_turn_sort, changed_conv_sort);
            }
        }
    }

    fn refresh_detail_watch_fp(&mut self) {
        self.detail_watch_fp = self.detail.as_ref().map(|d| {
            let subs = d.subagent_ids();
            conversation_watch_fingerprint(&self.roots, d.conversation_id(), &subs)
        });
    }

    fn refresh_picker_watch_fp(&mut self) {
        self.picker_watch_fp = Some(conversations_list_fingerprint(&self.roots));
    }

    fn poll_live_watch(&mut self) {
        if self.last_watch_check.elapsed() < LIVE_WATCH_INTERVAL {
            return;
        }
        self.last_watch_check = Instant::now();

        match self.screen {
            Screen::Detail => {
                let Some((conv_id, sub_ids)) = self
                    .detail
                    .as_ref()
                    .map(|d| (d.conversation_id().to_string(), d.subagent_ids()))
                else {
                    return;
                };
                let fp = conversation_watch_fingerprint(&self.roots, &conv_id, &sub_ids);
                if self.detail_watch_fp.as_ref() == Some(&fp) {
                    return;
                }
                if let Ok((updated, _)) = self.load_conversation_report(&conv_id) {
                    let mut subagents_changed = false;
                    if let Some(d) = self.detail.as_mut() {
                        d.apply_live_report(updated);
                        subagents_changed = d.subagent_ids() != sub_ids;
                    }
                    // Store pre-read `fp` when subagent set is unchanged so a mid-read commit is
                    // caught on the next tick; only re-fingerprint when new subagents appear.
                    if subagents_changed {
                        self.refresh_detail_watch_fp();
                    } else {
                        self.detail_watch_fp = Some(fp);
                    }
                }
            }
            Screen::Conversations => {
                let fp = conversations_list_fingerprint(&self.roots);
                if self.picker_watch_fp.as_ref() == Some(&fp) {
                    return;
                }
                let refreshed = list_conversations(&self.roots, &self.tz);
                if let Some(p) = self.picker.as_mut() {
                    p.refresh_items_live(refreshed);
                }
                self.picker_watch_fp = Some(fp);
            }
            Screen::Dashboard => {}
        }
    }

    fn back_screen(&self) -> Option<Screen> {
        match (self.origin, self.screen) {
            (Screen::Dashboard, Screen::Detail) => Some(Screen::Conversations),
            (Screen::Dashboard, Screen::Conversations) => Some(Screen::Dashboard),
            (Screen::Conversations, Screen::Detail) => Some(Screen::Conversations),
            (Screen::Detail, Screen::Detail) if self.opened_from_picker => {
                Some(Screen::Conversations)
            }
            (Screen::Detail, Screen::Conversations) if !self.opened_from_picker => {
                Some(Screen::Detail)
            }
            _ => None,
        }
    }

    fn load_conversation_report(
        &mut self,
        conv_id: &str,
    ) -> io::Result<(ConversationStatsReport, Duration)> {
        let start = Instant::now();
        let now = crate::sys::time::unix_now_secs();
        let metric = self.active_metric();
        let cache = self.cache_dir.as_deref();
        let pricing = self.pricing.get_or_insert_with(|| {
            PricingCatalog::load_effective_with(
                cache,
                SyncMode::BlockingIfStale,
                now,
                DEFAULT_PRICING_TTL_SECS,
                fetch_models_dev_via_curl,
            )
        });
        let report = collect_conversation_stats_report(
            &self.roots,
            conv_id,
            now,
            &self.tz,
            pricing,
            metric,
        )?;
        Ok((report, start.elapsed()))
    }

    fn open_conversations(&mut self) {
        let active_id = self
            .detail
            .as_ref()
            .map(|d| d.conversation_id().to_string());
        let items = list_conversations(&self.roots, &self.tz);
        match self.picker.as_mut() {
            Some(p) => p.replace_items(items, active_id.as_deref()),
            None => {
                self.picker = Some(PickerScreen::new(
                    items,
                    active_id.as_deref(),
                    None,
                    false,
                    self.prefs.conv_sort,
                ))
            }
        }
        self.refresh_picker_watch_fp();
        self.last_watch_check = Instant::now();
        self.screen = Screen::Conversations;
    }

    fn open_conversation(&mut self, conv_id: &str) {
        match self.load_conversation_report(conv_id) {
            Ok((report, elapsed)) => {
                let msg = format!(
                    "Loaded {} ({} turns) in {:.0}ms",
                    &report.conversation_id[..report.conversation_id.len().min(8)],
                    report.turns.len(),
                    elapsed.as_secs_f64() * 1_000.0
                );
                if let Some(p) = self.picker.as_mut() {
                    p.set_active_id(Some(&report.conversation_id));
                }
                let mut detail = DetailScreen::new(report, self.prefs.turn_sort);
                detail.set_status_msg(msg);
                self.detail = Some(detail);
                self.refresh_detail_watch_fp();
                self.last_watch_check = Instant::now();
                self.opened_from_picker = true;
                self.screen = Screen::Detail;
            }
            Err(err) => {
                if let Some(p) = self.picker.as_mut() {
                    p.set_status_msg(format!("Failed to load conversation: {err}"));
                }
            }
        }
    }

    fn pop_or_quit(&mut self) -> bool {
        let Some(prev) = self.back_screen() else {
            return true;
        };
        if prev == Screen::Conversations {
            if let (Some(p), Some(d)) = (self.picker.as_mut(), self.detail.as_ref()) {
                p.set_active_id(Some(d.conversation_id()));
            }
            self.refresh_picker_watch_fp();
        } else if prev == Screen::Detail {
            self.refresh_detail_watch_fp();
        }
        self.last_watch_check = Instant::now();
        self.screen = prev;
        false
    }

    fn run(mut self, pal: &Palette) -> io::Result<StatsTuiPrefs> {
        let (_guard, mut terminal) = enter_tui()?;

        loop {
            let scanning = self
                .dashboard
                .as_mut()
                .is_some_and(DashboardScreen::poll_scan);
            self.poll_live_watch();
            let has_back = self.back_screen().is_some();

            terminal.draw(|frame| match self.screen {
                Screen::Dashboard => {
                    if let Some(dash) = self.dashboard.as_mut() {
                        dash.draw(frame, pal);
                    }
                }
                Screen::Conversations => {
                    if let Some(picker) = self.picker.as_mut() {
                        picker.draw(frame, frame.area(), &self.timezone, has_back, pal);
                    }
                }
                Screen::Detail => {
                    if let Some(detail) = self.detail.as_mut() {
                        detail.draw(frame, has_back, pal);
                    }
                }
            })?;

            // Poll during background dashboard scans or active live-watch screens;
            // block at 0% CPU when Dashboard is idle.
            if scanning {
                if !event::poll(Duration::from_millis(50))? {
                    continue;
                }
            } else if matches!(self.screen, Screen::Conversations | Screen::Detail) {
                let wait = LIVE_WATCH_INTERVAL
                    .saturating_sub(self.last_watch_check.elapsed())
                    .max(Duration::from_millis(20));
                if !event::poll(wait)? {
                    continue;
                }
            }

            let evt = event::read()?;
            match self.screen {
                Screen::Dashboard => {
                    let term_h = terminal.size().map_or(30, |s| s.height);
                    let action = match evt {
                        Event::Key(key) if key.kind == KeyEventKind::Press => {
                            self.dashboard.as_mut().map_or(StatsAction::Quit, |d| {
                                d.handle_key(key.code, key.modifiers, term_h)
                            })
                        }
                        Event::Mouse(mouse) => {
                            if let Some(d) = self.dashboard.as_mut() {
                                d.handle_mouse(mouse);
                            }
                            StatsAction::Continue
                        }
                        _ => StatsAction::Continue,
                    };
                    self.sync_prefs();
                    match action {
                        StatsAction::Quit => break,
                        StatsAction::OpenConversations => self.open_conversations(),
                        StatsAction::Refresh => {
                            if let Some(d) = self.dashboard.as_mut() {
                                d.trigger_refresh(&self.roots, self.cache_dir.as_deref());
                            }
                        }
                        StatsAction::Continue => {}
                    }
                }
                Screen::Conversations => {
                    let action = match evt {
                        Event::Key(key) if key.kind == KeyEventKind::Press => self
                            .picker
                            .as_mut()
                            .map_or(PickerAction::Back, |p| p.handle_key(key)),
                        Event::Mouse(mouse) => self
                            .picker
                            .as_mut()
                            .map_or(PickerAction::Continue, |p| p.handle_mouse(mouse)),
                        _ => PickerAction::Continue,
                    };
                    self.sync_prefs();
                    match action {
                        PickerAction::Continue => {}
                        PickerAction::Quit => break,
                        PickerAction::Back => {
                            if self.pop_or_quit() {
                                break;
                            }
                        }
                        PickerAction::Reload => {
                            let start = Instant::now();
                            let refreshed = list_conversations(&self.roots, &self.tz);
                            let count = refreshed.len();
                            if let Some(p) = self.picker.as_mut() {
                                let active = p.active_id().map(str::to_string);
                                p.replace_items(refreshed, active.as_deref());
                                p.set_status_msg(format!(
                                    "Reloaded {count} conversations in {:.0}ms",
                                    start.elapsed().as_secs_f64() * 1_000.0
                                ));
                            }
                            self.refresh_picker_watch_fp();
                            self.last_watch_check = Instant::now();
                        }
                        PickerAction::Select(conv_id) => self.open_conversation(&conv_id),
                    }
                }
                Screen::Detail => {
                    let action = match evt {
                        Event::Key(key) if key.kind == KeyEventKind::Press => {
                            self.detail.as_mut().map_or(DetailAction::Quit, |d| {
                                d.handle_key(key.code, key.modifiers, has_back)
                            })
                        }
                        Event::Mouse(mouse) => {
                            if let Some(d) = self.detail.as_mut() {
                                d.handle_mouse(mouse);
                            }
                            DetailAction::Continue
                        }
                        _ => DetailAction::Continue,
                    };
                    self.sync_prefs();
                    match action {
                        DetailAction::Continue => {}
                        DetailAction::Quit => break,
                        DetailAction::Back => {
                            if self.pop_or_quit() {
                                break;
                            }
                        }
                        DetailAction::OpenConversations => self.open_conversations(),
                        DetailAction::Refresh => {
                            if let Some(conv_id) = self
                                .detail
                                .as_ref()
                                .map(|d| d.conversation_id().to_string())
                            {
                                match self.load_conversation_report(&conv_id) {
                                    Ok((updated, elapsed)) => {
                                        let turns = updated.turns.len();
                                        if let Some(d) = self.detail.as_mut() {
                                            d.apply_live_report(updated);
                                            d.set_status_msg(format!(
                                                "Refreshed {turns} turns in {:.0}ms",
                                                elapsed.as_secs_f64() * 1_000.0
                                            ));
                                        }
                                        self.refresh_detail_watch_fp();
                                        self.last_watch_check = Instant::now();
                                    }
                                    Err(err) => {
                                        if let Some(d) = self.detail.as_mut() {
                                            d.set_status_msg(format!("Refresh failed: {err}"));
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        drop(terminal);
        drop(_guard);
        self.prefs.metric = self.active_metric();
        Ok(self.prefs)
    }
}

fn require_tty(msg: &'static str) -> io::Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::new(io::ErrorKind::Unsupported, msg));
    }
    Ok(())
}

/// Launches the interactive `agy-statusline stats` TUI dashboard and returns the final preferences.
pub fn run_stats_tui<F>(
    roots: Vec<(&'static str, PathBuf)>,
    opts: &super::StatsOptions<'_>,
    prefs: StatsTuiPrefs,
    on_prefs_change: F,
) -> io::Result<StatsTuiPrefs>
where
    F: FnMut(Option<StatsMetric>, Option<TurnSort>, Option<ConversationSort>) + 'static,
{
    require_tty("interactive stats requires a TTY (use --plain or --json)")?;
    let tz = TimeZoneSpec::local();
    let timezone = tz.format_offset_at(opts.now_unix_secs as i64);
    let dashboard = DashboardScreen::new(
        &roots,
        opts.cache_dir,
        opts.days,
        opts.metric,
        opts.now_unix_secs,
    );
    let pal = Palette {
        no_color: opts.no_color,
        ascii: opts.ascii,
    };
    StatsApp {
        roots,
        cache_dir: opts.cache_dir.map(Path::to_path_buf),
        tz,
        timezone,
        prefs: StatsTuiPrefs {
            metric: opts.metric,
            ..prefs
        },
        on_prefs_change: Some(Box::new(on_prefs_change)),
        pricing: None,
        screen: Screen::Dashboard,
        origin: Screen::Dashboard,
        opened_from_picker: false,
        dashboard: Some(dashboard),
        picker: None,
        detail: None,
        last_watch_check: Instant::now(),
        detail_watch_fp: None,
        picker_watch_fp: None,
    }
    .run(&pal)
}

/// Launches the interactive single-conversation stats TUI (`agy-statusline stats <CONVERSATION_ID>`).
pub fn run_conversation_stats_tui<F>(
    roots: &[(&'static str, PathBuf)],
    conversation_query: &str,
    opts: &super::StatsOptions<'_>,
    prefs: StatsTuiPrefs,
    on_prefs_change: F,
) -> io::Result<StatsTuiPrefs>
where
    F: FnMut(Option<StatsMetric>, Option<TurnSort>, Option<ConversationSort>) + 'static,
{
    require_tty("interactive stats requires a TTY (use --plain, --all, or --json)")?;
    // Load synchronously before entering alternate screen so invalid/missing IDs
    // return a clean stderr error without flashing the terminal.
    let pricing = PricingCatalog::load_effective_with(
        opts.cache_dir,
        SyncMode::BlockingIfStale,
        opts.now_unix_secs,
        DEFAULT_PRICING_TTL_SECS,
        fetch_models_dev_via_curl,
    );
    let tz = TimeZoneSpec::local();
    let timezone = tz.format_offset_at(opts.now_unix_secs as i64);
    let report = collect_conversation_stats_report(
        roots,
        conversation_query,
        opts.now_unix_secs,
        &tz,
        &pricing,
        opts.metric,
    )?;
    let detail = DetailScreen::new(report, prefs.turn_sort);
    let sub_ids = detail.subagent_ids();
    let detail_fp = conversation_watch_fingerprint(roots, detail.conversation_id(), &sub_ids);
    let pal = Palette {
        no_color: opts.no_color,
        ascii: opts.ascii,
    };
    StatsApp {
        roots: roots.to_vec(),
        cache_dir: opts.cache_dir.map(Path::to_path_buf),
        tz,
        timezone,
        prefs: StatsTuiPrefs {
            metric: opts.metric,
            ..prefs
        },
        on_prefs_change: Some(Box::new(on_prefs_change)),
        pricing: Some(pricing),
        screen: Screen::Detail,
        origin: Screen::Detail,
        opened_from_picker: false,
        dashboard: None,
        picker: None,
        detail: Some(detail),
        last_watch_check: Instant::now(),
        detail_watch_fp: Some(detail_fp),
        picker_watch_fp: None,
    }
    .run(&pal)
}

/// Launches the full-screen historical conversation picker TUI (`agy-statusline conversations`).
pub fn run_conversations_tui<F>(
    roots: &[(&'static str, PathBuf)],
    initial_query: Option<&str>,
    show_subagents: bool,
    opts: &super::StatsOptions<'_>,
    prefs: StatsTuiPrefs,
    on_prefs_change: F,
) -> io::Result<StatsTuiPrefs>
where
    F: FnMut(Option<StatsMetric>, Option<TurnSort>, Option<ConversationSort>) + 'static,
{
    require_tty("interactive conversations requires a TTY (use --plain or --json)")?;
    let tz = TimeZoneSpec::local();
    let timezone = tz.format_offset_at(opts.now_unix_secs as i64);
    let items = list_conversations(roots, &tz);
    let picker_fp = conversations_list_fingerprint(roots);
    let pal = Palette {
        no_color: opts.no_color,
        ascii: opts.ascii,
    };
    StatsApp {
        roots: roots.to_vec(),
        cache_dir: opts.cache_dir.map(Path::to_path_buf),
        tz,
        timezone,
        prefs: StatsTuiPrefs {
            metric: opts.metric,
            ..prefs
        },
        on_prefs_change: Some(Box::new(on_prefs_change)),
        pricing: None,
        screen: Screen::Conversations,
        origin: Screen::Conversations,
        opened_from_picker: false,
        dashboard: None,
        picker: Some(PickerScreen::new(
            items,
            None,
            initial_query,
            show_subagents,
            prefs.conv_sort,
        )),
        detail: None,
        last_watch_check: Instant::now(),
        detail_watch_fp: None,
        picker_watch_fp: Some(picker_fp),
    }
    .run(&pal)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::fs;
    use std::rc::Rc;

    use crossterm::event::{KeyCode, KeyModifiers};
    use rusqlite::Connection;

    use super::*;
    use crate::agy::protobuf::encode_step_meta_blob;
    use crate::stats::metric::{ConversationSortField, SortDir, TurnSortField};
    use crate::stats::test_helpers::build_gen_blob;

    fn test_app(origin: Screen) -> StatsApp {
        StatsApp {
            roots: Vec::new(),
            cache_dir: None,
            tz: TimeZoneSpec::utc(),
            timezone: "UTC".to_string(),
            prefs: StatsTuiPrefs::default(),
            on_prefs_change: None,
            pricing: None,
            screen: origin,
            origin,
            opened_from_picker: false,
            dashboard: None,
            picker: None,
            detail: None,
            last_watch_check: Instant::now(),
            detail_watch_fp: None,
            picker_watch_fp: None,
        }
    }

    #[test]
    fn navigation_hierarchy_handles_all_three_entries_without_ping_pong() {
        // Entry 1: Dashboard -> Conversations -> Detail -> Conversations -> Dashboard.
        let mut app = test_app(Screen::Dashboard);
        assert_eq!(app.back_screen(), None);
        app.screen = Screen::Conversations;
        assert_eq!(app.back_screen(), Some(Screen::Dashboard));
        app.screen = Screen::Detail;
        app.opened_from_picker = true;
        assert_eq!(app.back_screen(), Some(Screen::Conversations));
        assert!(!app.pop_or_quit());
        assert_eq!(app.screen, Screen::Conversations);
        assert!(!app.pop_or_quit());
        assert_eq!(app.screen, Screen::Dashboard);

        // Entry 2: Conversations -> Detail -> Conversations -> quit.
        let mut conv = test_app(Screen::Conversations);
        assert_eq!(conv.back_screen(), None);
        conv.screen = Screen::Detail;
        conv.opened_from_picker = true;
        assert_eq!(conv.back_screen(), Some(Screen::Conversations));
        assert!(!conv.pop_or_quit());
        assert_eq!(conv.screen, Screen::Conversations);
        assert!(conv.pop_or_quit());

        // Entry 3: Detail -> Conversations (back returns to Detail) -> select new Detail -> back to Conversations -> quit.
        let mut det = test_app(Screen::Detail);
        assert_eq!(det.back_screen(), None);
        det.screen = Screen::Conversations;
        assert_eq!(det.back_screen(), Some(Screen::Detail));
        assert!(!det.pop_or_quit());
        assert_eq!(det.screen, Screen::Detail);
        det.screen = Screen::Conversations;
        det.screen = Screen::Detail;
        det.opened_from_picker = true;
        assert_eq!(det.back_screen(), Some(Screen::Conversations));
        assert!(!det.pop_or_quit());
        assert_eq!(det.screen, Screen::Conversations);
        assert!(det.pop_or_quit());
    }

    #[test]
    fn live_watch_reloads_new_turns_and_persists_remembered_sort_across_conversations() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("cli");
        let conv_dir = root.join("conversations");
        fs::create_dir_all(&conv_dir).unwrap();

        let id_a = "11111111-1111-4111-8111-111111111111";
        let id_b = "22222222-2222-4222-8222-222222222222";
        let db_a = conv_dir.join(format!("{id_a}.db"));
        let conn_a = Connection::open(&db_a).unwrap();
        conn_a
            .execute_batch(
                "PRAGMA journal_mode=WAL;
                 CREATE TABLE steps (idx INTEGER PRIMARY KEY, metadata BLOB);
                 CREATE TABLE gen_metadata (idx INTEGER PRIMARY KEY, data BLOB);",
            )
            .unwrap();
        conn_a
            .execute(
                "INSERT INTO steps (idx, metadata) VALUES (1, ?1)",
                [encode_step_meta_blob(1_795_500_000)],
            )
            .unwrap();
        conn_a
            .execute(
                "INSERT INTO gen_metadata (idx, data) VALUES (1, ?1)",
                [build_gen_blob(
                    Some(1),
                    1526,
                    "model-alpha",
                    "Model Alpha",
                    1_000,
                    4_000,
                    500,
                    100,
                    5,
                    800,
                    Some(0.10),
                )],
            )
            .unwrap();

        let saved = Rc::new(RefCell::new(Vec::new()));
        let saved_cb = Rc::clone(&saved);

        let mut app = test_app(Screen::Conversations);
        app.roots = vec![("cli", root.clone())];
        app.pricing = Some(PricingCatalog::load_effective_with(
            None,
            SyncMode::Offline,
            1_795_500_100,
            DEFAULT_PRICING_TTL_SECS,
            |_| unreachable!(),
        ));
        app.on_prefs_change = Some(Box::new(move |m, ts, cs| {
            saved_cb.borrow_mut().push((m, ts, cs));
        }));

        // Open Conversations list -> open conversation A -> change sort -> saved & remembered.
        app.open_conversations();
        app.open_conversation(id_a);
        assert_eq!(app.detail.as_ref().unwrap().sort(), TurnSort::default());

        app.detail
            .as_mut()
            .unwrap()
            .handle_key(KeyCode::Char('S'), KeyModifiers::NONE, true);
        app.sync_prefs();
        let expected_turn_sort = TurnSort {
            field: TurnSortField::Turn,
            dir: SortDir::Desc,
        };
        assert_eq!(app.prefs.turn_sort, expected_turn_sort);
        assert_eq!(
            saved.borrow().last(),
            Some(&(None, Some(expected_turn_sort), None))
        );

        // Write a second turn to conversation A (in WAL mode) and trigger live watch check.
        conn_a
            .execute(
                "INSERT INTO steps (idx, metadata) VALUES (2, ?1)",
                [encode_step_meta_blob(1_795_500_060)],
            )
            .unwrap();
        conn_a
            .execute(
                "INSERT INTO gen_metadata (idx, data) VALUES (2, ?1)",
                [build_gen_blob(
                    Some(2),
                    1526,
                    "model-alpha",
                    "Model Alpha",
                    2_000,
                    8_000,
                    600,
                    120,
                    6,
                    900,
                    Some(0.20),
                )],
            )
            .unwrap();
        app.last_watch_check = Instant::now() - LIVE_WATCH_INTERVAL - Duration::from_millis(10);
        app.poll_live_watch();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 32)).unwrap();
        terminal
            .draw(|frame| {
                app.detail.as_mut().unwrap().draw(
                    frame,
                    true,
                    &Palette {
                        no_color: true,
                        ascii: false,
                    },
                )
            })
            .unwrap();
        let detail_buf: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(detail_buf.contains("1. Turns (2 · # ↓)"), "{detail_buf}");

        // Back to Conversations, add conversation B on disk, live watch picks it up automatically.
        assert!(!app.pop_or_quit());
        assert_eq!(app.screen, Screen::Conversations);
        let conn_b = Connection::open(conv_dir.join(format!("{id_b}.db"))).unwrap();
        conn_b
            .execute_batch(
                "CREATE TABLE steps (idx INTEGER PRIMARY KEY, metadata BLOB);
                 CREATE TABLE gen_metadata (idx INTEGER PRIMARY KEY, data BLOB);",
            )
            .unwrap();
        drop(conn_b);
        app.last_watch_check = Instant::now() - LIVE_WATCH_INTERVAL - Duration::from_millis(10);
        app.poll_live_watch();

        // Opening conversation B inherits the remembered `turn_sort` (`# ↓`).
        app.open_conversation(id_b);
        assert_eq!(app.detail.as_ref().unwrap().sort(), expected_turn_sort);

        // Changing picker sort triggers persistence callback for `conv_sort`.
        assert!(!app.pop_or_quit());
        app.picker
            .as_mut()
            .unwrap()
            .handle_key(crossterm::event::KeyEvent::new(
                KeyCode::Char('S'),
                KeyModifiers::NONE,
            ));
        app.sync_prefs();
        assert_eq!(
            saved.borrow().last(),
            Some(&(
                None,
                None,
                Some(ConversationSort {
                    field: ConversationSortField::Modified,
                    dir: SortDir::Asc,
                }),
            ))
        );
    }
}
