//! Global usage dashboard screen (`agy-statusline stats`).

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, TryRecvError};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use jiff::civil::Date;
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table, TableState};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use super::dashboard_state::{StatsAction, StatsPane, StatsTuiState};
use super::{
    compute_header_hits, render_table_with_scrollbar, right_cell, token_breakdown_lines,
    ClickRegions, HeaderHits, Palette, SpanBar, TableChrome, TableHit,
};
use crate::stats::metric::{
    format_grouped_u64, format_pct, format_usd_short, heatmap_level_f64, StatsMetric, UsageCounters,
};
use crate::stats::query::{
    DailySort, HeatmapWeek, RangePreset, SourceFilter, StatsDataset, StatsView, WEEKDAY_NAMES,
};
use crate::stats::render::{
    build_month_header, format_diagnostics_compact, format_quota_source_age,
    format_quota_window_brief, ASCII_HEATMAP, UNICODE_HEATMAP,
};
use crate::stats::{QuotaWindowReport, ScanDiagnostics, StatsQuotaReport};
use crate::sys::fmt::{format_tokens_string as fmt_tok, sanitize_inline_text, truncate_to_width};

const UNICODE_BAR_EIGHTHS: [char; 9] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DashboardClick {
    Metric(StatsMetric),
    CycleMetric,
    Range(RangePreset),
    CycleRange,
    CycleSource,
    ClearModelFilter,
    OpenConversations,
    CycleSort,
    ToggleModelScope,
    ToggleFillEmpty,
    ToggleHeatmap,
    TogglePane,
    Refresh,
    Quit,
}

#[derive(Debug, Clone)]
struct HeatmapCellHit {
    rect: Rect,
    date: String,
    has_activity: bool,
}

#[derive(Default)]
struct DashboardTables {
    daily: TableState,
    models: TableState,
}

#[derive(Default)]
struct DashboardHitMap {
    controls: ClickRegions<DashboardClick>,
    heatmap: Vec<HeatmapCellHit>,
    daily: TableHit,
    daily_headers: HeaderHits<DailyCol>,
    models: TableHit,
    footer: ClickRegions<DashboardClick>,
}

pub(super) struct DashboardScreen {
    state: StatsTuiState,
    dataset: Option<StatsDataset>,
    view: Option<StatsView>,
    tables: DashboardTables,
    hit_map: DashboardHitMap,
    pending_scan: Option<mpsc::Receiver<(StatsDataset, Duration)>>,
}

fn spawn_dataset_scan(
    roots: Vec<(&'static str, PathBuf)>,
    daemon_dirs: Vec<PathBuf>,
    cache_dir: Option<PathBuf>,
    now_unix_secs: u64,
    force_quota_refresh: bool,
) -> mpsc::Receiver<(StatsDataset, Duration)> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let start = Instant::now();
        let ds = StatsDataset::collect(
            &roots,
            &daemon_dirs,
            cache_dir.as_deref(),
            now_unix_secs,
            force_quota_refresh,
        );
        let _ = tx.send((ds, start.elapsed()));
    });
    rx
}

impl DashboardScreen {
    pub(super) fn new(
        roots: &[(&'static str, PathBuf)],
        daemon_dirs: &[PathBuf],
        cache_dir: Option<&Path>,
        days: Option<u32>,
        metric: StatsMetric,
        now_unix_secs: u64,
    ) -> Self {
        Self {
            state: StatsTuiState::new(days, metric),
            dataset: None,
            view: None,
            tables: DashboardTables::default(),
            hit_map: DashboardHitMap::default(),
            pending_scan: Some(spawn_dataset_scan(
                roots.to_vec(),
                daemon_dirs.to_vec(),
                cache_dir.map(Path::to_path_buf),
                now_unix_secs,
                false,
            )),
        }
    }

    pub(super) fn active_metric(&self) -> StatsMetric {
        self.state.query().metric
    }

    /// Polls any in-flight background scan and recomputes the view if dirty.
    /// Returns `true` while a scan is still active.
    pub(super) fn poll_scan(&mut self) -> bool {
        if let Some(rx) = self.pending_scan.as_ref() {
            match rx.try_recv() {
                Ok((ds, elapsed)) => {
                    let is_refresh = self.dataset.is_some();
                    let (turns, dbs) = (ds.turn_count(), ds.sqlite.db_files);
                    self.dataset = Some(ds);
                    let status_msg = is_refresh.then(|| {
                        format!(
                            "Refreshed {turns} turns across {dbs} DBs in {:.2}s",
                            elapsed.as_secs_f64()
                        )
                    });
                    self.state.finish_scan(status_msg);
                    self.pending_scan = None;
                }
                Err(TryRecvError::Disconnected) => {
                    self.state.fail_scan("Background scan interrupted.");
                    self.pending_scan = None;
                }
                Err(TryRecvError::Empty) => {}
            }
        }

        if self.state.is_view_dirty() {
            if let Some(ds) = self.dataset.as_ref() {
                let computed = ds.compute_view(self.state.query(), self.state.selected_date());
                self.state.sync_with_view(&computed);
                self.view = Some(computed);
            }
        }
        self.pending_scan.is_some()
    }

    pub(super) fn trigger_refresh(
        &mut self,
        roots: &[(&'static str, PathBuf)],
        daemon_dirs: &[PathBuf],
        cache_dir: Option<&Path>,
    ) {
        if self.pending_scan.is_none() {
            self.state.begin_refresh("Refreshing local AGY usage...");
            let now = crate::sys::time::unix_now_secs();
            self.pending_scan = Some(spawn_dataset_scan(
                roots.to_vec(),
                daemon_dirs.to_vec(),
                cache_dir.map(Path::to_path_buf),
                now,
                true,
            ));
        }
    }

    pub(super) fn handle_key(
        &mut self,
        code: KeyCode,
        mods: KeyModifiers,
        term_h: u16,
    ) -> StatsAction {
        let page_size = self.hit_map.daily.data_rows.max(5);
        self.state
            .handle_key(code, mods, self.view.as_ref(), page_size, term_h)
    }

    fn apply_click(&mut self, click: DashboardClick, term_h: u16) -> StatsAction {
        match click {
            DashboardClick::Metric(m) => self.state.select_metric(m),
            DashboardClick::CycleMetric => {
                let next = self.state.query().metric.step(1);
                self.state.select_metric(next);
            }
            DashboardClick::Range(r) => self.state.select_range(r),
            DashboardClick::CycleRange => self.state.cycle_range(1),
            DashboardClick::CycleSource => self.state.cycle_source(),
            DashboardClick::ClearModelFilter => self.state.clear_model_filter(),
            DashboardClick::OpenConversations => return StatsAction::OpenConversations,
            DashboardClick::CycleSort => self.state.cycle_sort(),
            DashboardClick::ToggleModelScope => self.state.toggle_model_scope(),
            DashboardClick::ToggleFillEmpty => self.state.toggle_fill_empty_days(),
            DashboardClick::ToggleHeatmap => self.state.toggle_heatmap(term_h),
            DashboardClick::TogglePane => self.state.toggle_pane(),
            DashboardClick::Refresh => return StatsAction::Refresh,
            DashboardClick::Quit => return StatsAction::Quit,
        }
        StatsAction::Continue
    }

    fn click_daily_header(&mut self, col: DailyCol) {
        let cur = self.state.query().sort;
        let next = match col {
            DailyCol::Date => {
                if cur == DailySort::DateDesc {
                    DailySort::DateAsc
                } else {
                    DailySort::DateDesc
                }
            }
            DailyCol::CacheHit => {
                if cur == DailySort::CacheHitAsc {
                    DailySort::DateDesc
                } else {
                    DailySort::CacheHitAsc
                }
            }
            DailyCol::Metric => {
                if cur == DailySort::MetricDesc {
                    DailySort::DateDesc
                } else {
                    DailySort::MetricDesc
                }
            }
            _ => return,
        };
        self.state.select_sort(next);
    }

    pub(super) fn handle_mouse(&mut self, mouse: MouseEvent, term_h: u16) -> StatsAction {
        let pos = Position::new(mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.state.clear_status_msg();
                if let Some(v) = self.view.as_ref() {
                    if self.hit_map.models.contains(pos) {
                        self.state.move_model(-1, v);
                    } else {
                        self.state.move_daily(-3, v);
                    }
                }
            }
            MouseEventKind::ScrollDown => {
                self.state.clear_status_msg();
                if let Some(v) = self.view.as_ref() {
                    if self.hit_map.models.contains(pos) {
                        self.state.move_model(1, v);
                    } else {
                        self.state.move_daily(3, v);
                    }
                }
            }
            MouseEventKind::Down(MouseButton::Right) => {
                if self.hit_map.controls.hit(pos) == Some(DashboardClick::CycleRange) {
                    self.state.cycle_range(-1);
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.state.clear_status_msg();
                if let Some(click) = self
                    .hit_map
                    .controls
                    .hit(pos)
                    .or_else(|| self.hit_map.footer.hit(pos))
                {
                    return self.apply_click(click, term_h);
                }
                let Some(v) = self.view.as_ref() else {
                    return StatsAction::Continue;
                };
                if let Some(cell) = self.hit_map.heatmap.iter().find(|c| c.rect.contains(pos)) {
                    self.state
                        .select_heatmap_date(&cell.date, cell.has_activity, v);
                } else if self.hit_map.daily.contains(pos) {
                    self.state.focus_daily();
                    if mouse.row == self.hit_map.daily.pane.y {
                        self.state.cycle_sort();
                    } else if let Some(col) = self.hit_map.daily_headers.col_at(pos) {
                        self.click_daily_header(col);
                    } else if let Some(target) = self.hit_map.daily.row_at(
                        pos,
                        self.tables.daily.offset(),
                        v.daily_rows.len(),
                    ) {
                        self.state.select_daily_row(target, v);
                    }
                } else if self.hit_map.models.contains(pos) {
                    if mouse.row == self.hit_map.models.pane.y {
                        self.state.toggle_model_scope();
                    } else {
                        let len = self.state.active_models(v).len();
                        let target =
                            self.hit_map
                                .models
                                .row_at(pos, self.tables.models.offset(), len);
                        self.state.click_model_row(target, v);
                    }
                }
            }
            _ => {}
        }
        StatsAction::Continue
    }

    pub(super) fn draw(&mut self, frame: &mut Frame<'_>, pal: &Palette) {
        self.hit_map = draw_stats_ui(
            frame,
            &self.state,
            &mut self.tables,
            self.dataset.as_ref(),
            self.view.as_ref(),
            pal,
        );
    }
}

fn render_hbar(value: f64, max: f64, width: usize, ascii: bool) -> String {
    if width == 0 || !value.is_finite() || !max.is_finite() || value <= 0.0 || max <= 0.0 {
        return String::new();
    }
    let ratio = (value / max).clamp(0.0, 1.0);
    if ascii {
        return "#".repeat(((ratio * width as f64).round() as usize).clamp(1, width));
    }
    let eighths = ((ratio * width as f64 * 8.0).round() as usize).clamp(1, width * 8);
    let rem = eighths % 8;
    let mut s = "█".repeat(eighths / 8);
    if rem > 0 {
        s.push(UNICODE_BAR_EIGHTHS[rem]);
    }
    s
}

fn short_month_day(date: &str) -> &str {
    date.get(5..).unwrap_or(date)
}

fn draw_stats_ui(
    frame: &mut Frame<'_>,
    state: &StatsTuiState,
    tables: &mut DashboardTables,
    dataset: Option<&StatsDataset>,
    view: Option<&StatsView>,
    pal: &Palette,
) -> DashboardHitMap {
    let area = frame.area();
    let show_heatmap = state.heatmap_visible(area.height) && area.width >= 96;
    let quota_opt = dataset
        .and_then(|d| d.quota.as_ref())
        .filter(|q| q.has_visible_content());
    let quota_h: u16 = if quota_opt.is_some() && area.height >= 20 {
        if area.height >= 26 {
            3
        } else {
            1
        }
    } else {
        0
    };

    let (ctrl_area, quota_area, summary_area, main_area, footer_area) = if quota_h > 0 {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(quota_h),
                Constraint::Length(if show_heatmap { 11 } else { 3 }),
                Constraint::Min(8),
                Constraint::Length(1),
            ])
            .split(area);
        (chunks[0], Some(chunks[1]), chunks[2], chunks[3], chunks[4])
    } else {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(if show_heatmap { 11 } else { 3 }),
                Constraint::Min(8),
                Constraint::Length(1),
            ])
            .split(area);
        (chunks[0], None, chunks[1], chunks[2], chunks[3])
    };

    let mut hit_map = DashboardHitMap {
        controls: draw_control_bar(frame, ctrl_area, state, dataset, pal),
        ..Default::default()
    };
    if let (Some(q), Some(q_area)) = (quota_opt, quota_area) {
        draw_quota_strip(frame, q_area, q, pal);
    }

    let diag = dataset.map_or_else(ScanDiagnostics::default, |d| d.diagnostics);
    if let Some(v) = view {
        if show_heatmap {
            let top = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
                .split(summary_area);
            draw_summary_card(frame, top[0], state, v, &diag, pal);
            hit_map.heatmap = draw_heatmap_pane(frame, top[1], state, v, pal);
        } else {
            draw_compact_summary(frame, summary_area, state, v, &diag, pal);
        }

        if area.width >= 96 {
            let main = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
                .split(main_area);
            let (daily, daily_headers) =
                draw_daily_pane(frame, main[0], state, &mut tables.daily, v, pal);
            hit_map.daily = daily;
            hit_map.daily_headers = daily_headers;

            let right = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(if main[1].height >= 19 { 11 } else { 9 }),
                    Constraint::Min(5),
                ])
                .split(main[1]);
            draw_day_detail_pane(frame, right[0], v, pal);
            hit_map.models = draw_models_pane(frame, right[1], state, &mut tables.models, v, pal);
        } else {
            match state.active_pane() {
                StatsPane::Daily => {
                    let (daily, daily_headers) =
                        draw_daily_pane(frame, main_area, state, &mut tables.daily, v, pal);
                    hit_map.daily = daily;
                    hit_map.daily_headers = daily_headers;
                }
                StatsPane::Models => {
                    let stacked = Layout::default()
                        .direction(Direction::Vertical)
                        .constraints([
                            Constraint::Length(if main_area.height >= 16 { 9 } else { 7 }),
                            Constraint::Min(5),
                        ])
                        .split(main_area);
                    draw_day_detail_pane(frame, stacked[0], v, pal);
                    hit_map.models =
                        draw_models_pane(frame, stacked[1], state, &mut tables.models, v, pal);
                }
            }
        }
    } else {
        let msg = if state.is_loading() {
            "Scanning local AGY conversation databases & pricing cache..."
        } else {
            "No local AGY usage data found."
        };
        let loading = Paragraph::new(Line::styled(format!("  {msg}"), pal.bold(Color::Yellow)))
            .block(pal.pane_block(" Loading ".to_string(), true));
        frame.render_widget(loading, main_area);
    }

    hit_map.footer = draw_footer_bar(frame, footer_area, state, dataset, pal);
    hit_map
}

fn quota_win_style(win: &QuotaWindowReport, pal: &Palette) -> Style {
    if let Some(amt) = win.remaining_amount {
        if amt <= 0 {
            return pal.bold(Color::Red);
        } else if amt <= crate::statusline::metrics::LOW_CREDIT_WARN_THRESHOLD {
            return pal.fg(Color::Yellow);
        }
        return pal.fg(Color::Green);
    }
    if win.remaining_pct <= 10.0 {
        pal.bold(Color::Red)
    } else if win.will_exhaust_before_reset {
        pal.bold(Color::Yellow)
    } else if win.remaining_pct <= 30.0 {
        pal.fg(Color::Yellow)
    } else {
        pal.fg(Color::Green)
    }
}

fn draw_quota_strip(frame: &mut Frame<'_>, area: Rect, q: &StatsQuotaReport, pal: &Palette) {
    let sep = pal.sep();
    let sep_spaced = format!(" {sep} ");
    let mut header_meta = Vec::new();
    if let Some(tier) = q.plan_tier.as_deref() {
        let clean = sanitize_inline_text(tier);
        if !clean.is_empty() {
            header_meta.push(clean);
        }
    }
    if let Some(credits) = q.ai_credits {
        header_meta.push(format!("AI credits {credits}"));
    }
    header_meta.push(format_quota_source_age(q));

    let bordered = area.height >= 3;
    let inner_w = if bordered {
        usize::from(area.width.saturating_sub(2))
    } else {
        usize::from(area.width)
    };
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;

    let mut try_push_group = |group: Vec<Span<'static>>| {
        let prefix = if spans.is_empty() {
            " "
        } else {
            sep_spaced.as_str()
        };
        let group_w: usize = group.iter().map(|s| s.content.width()).sum();
        let total_w = prefix.width() + group_w;
        if !spans.is_empty() && used + total_w > inner_w {
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
        spans.extend(group);
        used += total_w;
    };

    if !bordered {
        let mut head = vec![Span::styled("Quota ", pal.bold(Color::Cyan))];
        if let Some(tier) = q.plan_tier.as_deref() {
            let clean = sanitize_inline_text(tier);
            if !clean.is_empty() {
                head.push(Span::styled(format!("{clean} "), pal.bold(Color::White)));
            }
        }
        if let Some(credits) = q.ai_credits {
            let color = if credits <= 0 {
                Color::Red
            } else if credits <= crate::statusline::metrics::LOW_CREDIT_WARN_THRESHOLD {
                Color::Yellow
            } else {
                Color::Green
            };
            head.push(Span::styled(format!("cr:{credits} "), pal.bold(color)));
        }
        head.push(Span::styled(
            format!("[{}]", format_quota_source_age(q)),
            pal.dim(),
        ));
        try_push_group(head);
    }

    for pool in &q.pools {
        let mut group = vec![Span::styled(
            format!("{} ", pool.name),
            pal.bold(Color::White),
        )];
        let mut added_win = false;
        if let Some(ref short) = pool.short_window {
            group.push(Span::styled(
                format_quota_window_brief(&pool.name, short, false),
                quota_win_style(short, pal),
            ));
            added_win = true;
        }
        if let Some(ref long) = pool.long_window {
            if added_win {
                group.push(Span::styled(", ", pal.dim()));
            }
            group.push(Span::styled(
                format_quota_window_brief(&pool.name, long, true),
                quota_win_style(long, pal),
            ));
            added_win = true;
        }
        if added_win {
            try_push_group(group);
        }
    }

    for credit in &q.credits {
        if q.ai_credits.is_some()
            && (credit.key.eq_ignore_ascii_case("google_one_ai")
                || credit.key.eq_ignore_ascii_case("ai_credits"))
        {
            continue;
        }
        let style = match credit.severity.as_str() {
            "critical" => pal.bold(Color::Red),
            "low" => pal.fg(Color::Yellow),
            _ => pal.fg(Color::Green),
        };
        try_push_group(vec![
            Span::styled(format!("{} ", credit.label), pal.dim()),
            Span::styled(credit.remaining_amount.to_string(), style),
        ]);
    }

    if spans.is_empty() {
        spans.push(Span::styled(
            format!(" {}", header_meta.join(&sep_spaced)),
            pal.fg(Color::White),
        ));
    }

    if bordered {
        let title = format!(" Quota ({}) ", header_meta.join(&sep_spaced));
        frame.render_widget(
            Paragraph::new(Line::from(spans)).block(pal.pane_block(title, false)),
            area,
        );
    } else {
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }
}

fn draw_control_bar(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &StatsTuiState,
    dataset: Option<&StatsDataset>,
    pal: &Palette,
) -> ClickRegions<DashboardClick> {
    let sep = pal.sep();
    let title = match dataset {
        Some(ds) => format!(
            " agy-statusline stats {sep} Local {} ({}) {}",
            ds.today_local,
            ds.timezone,
            if state.is_loading() {
                "[refreshing...] "
            } else {
                ""
            }
        ),
        None => " agy-statusline stats ".to_string(),
    };

    let block = pal.pane_block(title, false);
    let inner = block.inner(area);
    let compact = area.width < 100;
    let mut bar = SpanBar::new(inner);

    bar.push(if compact { " " } else { " Metric: " }, pal.dim());
    for (idx, &preset) in StatsMetric::PRESETS.iter().enumerate() {
        let label = if compact {
            format!("{}:{}", idx + 1, preset.compact_label())
        } else {
            format!(" {}:{} ", idx + 1, preset.label())
        };
        if !bar.push_click(
            label,
            pal.pill(state.query().metric == preset),
            DashboardClick::Metric(preset),
        ) {
            break;
        }
        let _ = bar.push(" ", Style::default());
    }

    if bar.push(format!(" {sep} "), pal.dim()) {
        if compact {
            let _ = bar.push_click(
                format!("[{}]", state.query().range.label()),
                pal.pill(true),
                DashboardClick::CycleRange,
            );
        } else {
            for &range in state.range_options() {
                let active = state.query().range == range;
                let text = if active {
                    format!("[{}]", range.label())
                } else {
                    range.label()
                };
                if !bar.push_click(text, pal.pill(active), DashboardClick::Range(range)) {
                    break;
                }
                let _ = bar.push(" ", Style::default());
            }
        }
    }

    let show_src = dataset.is_some_and(|d| d.has_cli && d.has_desktop)
        || state.query().source_filter != SourceFilter::All;
    if show_src {
        let style = if state.query().source_filter == SourceFilter::All {
            pal.pill(false)
        } else {
            pal.bold(Color::Yellow)
        };
        let _ = bar.push_click(
            format!(" Src:{} ", state.query().source_filter.label()),
            style,
            DashboardClick::CycleSource,
        );
    }

    if let Some(model) = state.query().model_filter.as_deref() {
        let close = if pal.ascii { "x" } else { "×" };
        let style = if pal.no_color {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
                .bg(Color::Yellow)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD)
        };
        let label = truncate_to_width(model, 18, if pal.ascii { "..." } else { "…" });
        let _ = bar.push_click(
            format!(" {close} {label} "),
            style,
            DashboardClick::ClearModelFilter,
        );
    }

    let (line, hits) = bar.finish();
    frame.render_widget(Paragraph::new(line).block(block), area);
    hits
}

fn draw_summary_card(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &StatsTuiState,
    view: &StatsView,
    diag: &ScanDiagnostics,
    pal: &Palette,
) {
    let metric = state.query().metric;
    let sep = pal.sep();
    let title = format!(
        " Summary ({} {sep} {}) ",
        state.query().range.label(),
        metric.label()
    );

    let peak_str = match &view.peak_day {
        Some((date, wd, val)) => format!(
            "{} ({} {wd})",
            metric.format_display(*val, true),
            short_month_day(date)
        ),
        None => "-".to_string(),
    };
    let spd = view
        .median_tok_per_sec
        .map_or_else(|| "-".to_string(), |s| format!("{s:.1} tok/s"));
    let ttft = view
        .median_ttft_secs
        .map_or_else(|| "-".to_string(), |t| format!("{t:.2}s"));
    let cost = if view.summary.priced_requests > 0 {
        format_usd_short(view.summary.priced_usd)
    } else {
        "-".to_string()
    };
    let hit_pct = view.summary.cache_hit_pct();

    let kv = |k: &'static str, v: String, color: Color| -> Line<'static> {
        Line::from(vec![
            Span::styled(format!("  {k:<11} "), pal.dim()),
            Span::styled(v, pal.bold(color)),
        ])
    };

    let mut lines = vec![
        kv(
            "Total",
            metric.format_display(view.total_value, false),
            Color::Cyan,
        ),
        kv(
            "Daily Avg",
            format!(
                "{} / active day",
                metric.format_average(view.total_value, view.active_days)
            ),
            Color::Green,
        ),
        kv("Peak Day", peak_str, Color::Yellow),
        kv(
            "Active Days",
            format!("{} / {}d", view.active_days, view.span_days),
            Color::White,
        ),
        Line::from(vec![
            Span::styled(format!("  {:<11} ", "Turns / Hit"), pal.dim()),
            Span::styled(
                format!("{} turns {sep} ", format_grouped_u64(view.summary.requests)),
                pal.bold(Color::White),
            ),
            Span::styled(
                format!("{} cache", format_pct(hit_pct)),
                pal.cache_hit_style(hit_pct).add_modifier(Modifier::BOLD),
            ),
        ]),
        kv("Est. Cost", cost, Color::Green),
        kv("Speed/TTFT", format!("{spd} {sep} {ttft}"), Color::White),
    ];

    if let Some(model) = state.query().model_filter.as_deref() {
        lines.push(kv("Model", model.to_string(), Color::Yellow));
    } else if metric == StatsMetric::Cost && view.summary.unpriced_requests > 0 {
        lines.push(Line::styled(
            format!(
                "  Note: {} unpriced turns excluded",
                view.summary.unpriced_requests
            ),
            pal.dim(),
        ));
    }
    if let Some(diag_str) = format_diagnostics_compact(diag, pal.ascii) {
        let color = if diag.retries_429 > 0 || diag.error_steps > 0 {
            Color::Red
        } else {
            Color::Yellow
        };
        lines.push(kv("Diag", diag_str, color));
    }

    frame.render_widget(
        Paragraph::new(lines).block(pal.pane_block(title, false)),
        area,
    );
}

fn draw_compact_summary(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &StatsTuiState,
    view: &StatsView,
    diag: &ScanDiagnostics,
    pal: &Palette,
) {
    let metric = state.query().metric;
    let sep = format!(" {} ", pal.sep());
    let peak_str = match &view.peak_day {
        Some((date, _, val)) => {
            format!(
                "{} ({})",
                metric.format_display(*val, true),
                short_month_day(date)
            )
        }
        None => "-".to_string(),
    };
    let hit_pct = view.summary.cache_hit_pct();
    let inner_w = usize::from(area.width.saturating_sub(2));

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    let mut push_segment = |label: &str, value: String, value_style: Style| {
        let prefix = if spans.is_empty() { " " } else { sep.as_str() };
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

    push_segment(
        "Total ",
        metric.format_display(view.total_value, true),
        pal.bold(Color::Cyan),
    );
    push_segment(
        "Avg ",
        format!(
            "{}/d",
            metric.format_average(view.total_value, view.active_days)
        ),
        pal.bold(Color::Green),
    );
    push_segment("Peak ", peak_str, pal.bold(Color::Yellow));
    push_segment(
        "Hit ",
        format_pct(hit_pct),
        pal.cache_hit_style(hit_pct).add_modifier(Modifier::BOLD),
    );
    push_segment(
        "Active ",
        format!("{}/{}d", view.active_days, view.span_days),
        pal.bold(Color::White),
    );
    if let Some(diag_str) = format_diagnostics_compact(diag, pal.ascii) {
        let color = if diag.retries_429 > 0 || diag.error_steps > 0 {
            Color::Red
        } else {
            Color::Yellow
        };
        push_segment("Diag ", diag_str, pal.bold(color));
    }

    let title = format!(
        " Summary ({} {} {}) ",
        state.query().range.label(),
        pal.sep(),
        metric.label()
    );
    frame.render_widget(
        Paragraph::new(Line::from(spans)).block(pal.pane_block(title, false)),
        area,
    );
}

fn draw_heatmap_pane(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &StatsTuiState,
    view: &StatsView,
    pal: &Palette,
) -> Vec<HeatmapCellHit> {
    let metric = state.query().metric;
    let sep = pal.sep();
    let title = format!(" Activity (20w {sep} {}) ", metric.label());
    let block = pal.pane_block(title, false);
    let inner = block.inner(area);

    let inner_w = usize::from(inner.width);
    let max_weeks = (inner_w.saturating_sub(6) / 3).min(view.heatmap_weeks.len());
    let visible_weeks: &[HeatmapWeek] =
        &view.heatmap_weeks[view.heatmap_weeks.len().saturating_sub(max_weeks)..];
    let glyphs = if pal.ascii {
        &ASCII_HEATMAP
    } else {
        &UNICODE_HEATMAP
    };
    let peak_mag = view
        .heatmap_peak
        .as_ref()
        .map_or(0.0, |(_, _, v)| v.magnitude());
    let peak_note = match &view.heatmap_peak {
        Some((date, wd, val)) => format!(
            "peak {} on {date} ({wd})",
            metric.format_display(*val, true)
        ),
        None => "no activity".to_string(),
    };

    let mut hdr_spans = vec![
        Span::styled(format!("  {peak_note} {sep} "), pal.fg(Color::White)),
        Span::styled("Less ", pal.dim()),
    ];
    for (lvl, glyph) in glyphs.iter().enumerate() {
        hdr_spans.push(Span::styled(*glyph, pal.heatmap(lvl, false)));
        hdr_spans.push(Span::raw(" "));
    }
    hdr_spans.push(Span::styled("More", pal.dim()));

    let mut lines = Vec::with_capacity(9);
    lines.push(Line::from(hdr_spans));

    let week_dates: Vec<Option<Date>> = visible_weeks
        .iter()
        .map(|w| w.week_start.parse().ok())
        .collect();
    lines.push(Line::styled(
        format!("       {}", build_month_header(&week_dates)),
        pal.dim(),
    ));

    let mut cell_hits = Vec::new();
    for (wd_idx, wd_name) in WEEKDAY_NAMES.iter().enumerate() {
        let prefix = format!("  {wd_name}  ");
        let mut cur_x = inner.x.saturating_add(prefix.width() as u16);
        let row_y = inner.y.saturating_add(2 + wd_idx as u16);
        let mut row_spans = vec![Span::styled(prefix, pal.dim())];
        for (w_idx, week) in visible_weeks.iter().enumerate() {
            match &week.daily[wd_idx] {
                Some((date, val)) => {
                    let lvl = heatmap_level_f64(val.magnitude(), peak_mag);
                    let is_sel = state.selected_date() == Some(date.as_str());
                    let glyph = glyphs[lvl];
                    let gw = glyph.width() as u16;
                    if row_y < inner.bottom() && cur_x.saturating_add(gw) <= inner.right() {
                        cell_hits.push(HeatmapCellHit {
                            rect: Rect::new(cur_x, row_y, gw, 1),
                            date: date.clone(),
                            has_activity: val.is_positive(),
                        });
                    }
                    cur_x = cur_x.saturating_add(gw);
                    row_spans.push(Span::styled(glyph, pal.heatmap(lvl, is_sel)));
                }
                None => {
                    cur_x = cur_x.saturating_add(2);
                    row_spans.push(Span::raw("  "));
                }
            }
            if w_idx + 1 < visible_weeks.len() {
                cur_x = cur_x.saturating_add(1);
                row_spans.push(Span::raw(" "));
            }
        }
        lines.push(Line::from(row_spans));
    }

    frame.render_widget(Paragraph::new(lines).block(block), area);
    cell_hits
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DailyCol {
    Date,
    Wd,
    Turns,
    Input,
    Cache,
    Output,
    CacheHit,
    Metric,
    Share,
    Bar,
}

impl DailyCol {
    fn is_sorted(self, sort: DailySort) -> bool {
        matches!(
            (self, sort),
            (Self::Date, DailySort::DateDesc | DailySort::DateAsc)
                | (Self::CacheHit, DailySort::CacheHitAsc)
                | (Self::Metric, DailySort::MetricDesc)
        )
    }

    fn fixed_width(self, narrow: bool, sort: DailySort) -> Option<u16> {
        let active = self.is_sorted(sort);
        match self {
            Self::Date => Some(10),
            Self::Wd => Some(3),
            Self::CacheHit => Some(if active { 8 } else { 6 }),
            Self::Turns | Self::Input | Self::Cache | Self::Output | Self::Share => Some(6),
            Self::Metric => Some(if narrow && !active { 9 } else { 10 }),
            Self::Bar => None,
        }
    }

    fn constraint(self, narrow: bool, sort: DailySort) -> Constraint {
        match self.fixed_width(narrow, sort) {
            Some(w) => Constraint::Length(w),
            None => Constraint::Min(4),
        }
    }

    fn header(self, metric_hdr: &'static str, sort: DailySort, pal: &Palette) -> Cell<'static> {
        let active_style = if pal.no_color {
            Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            pal.bold(Color::Cyan)
        };
        let arrow = |asc: bool| match (asc, pal.ascii) {
            (true, false) => "↑",
            (true, true) => "^",
            (false, false) => "↓",
            (false, true) => "v",
        };
        match self {
            Self::Date => match sort {
                DailySort::DateDesc => {
                    Cell::from(format!("Date {}", arrow(false))).style(active_style)
                }
                DailySort::DateAsc => {
                    Cell::from(format!("Date {}", arrow(true))).style(active_style)
                }
                _ => Cell::from("Date"),
            },
            Self::Wd => Cell::from("Wd"),
            Self::Turns => right_cell("Turns", pal.dim()),
            Self::Input => right_cell("Input", pal.dim()),
            Self::Cache => right_cell("Cache", pal.dim()),
            Self::Output => right_cell("Output", pal.dim()),
            Self::CacheHit => {
                if sort == DailySort::CacheHitAsc {
                    right_cell(format!("Cache% {}", arrow(true)), active_style)
                } else {
                    right_cell("Cache%", pal.dim())
                }
            }
            Self::Metric => {
                if sort == DailySort::MetricDesc {
                    right_cell(format!("{metric_hdr} {}", arrow(false)), active_style)
                } else {
                    right_cell(metric_hdr, pal.dim())
                }
            }
            Self::Share => right_cell("Share", pal.dim()),
            Self::Bar => Cell::from("Bar"),
        }
    }
}

fn daily_columns(inner_w: usize) -> &'static [DailyCol] {
    use DailyCol::*;
    match inner_w {
        72.. => &[Date, Wd, Turns, Input, Cache, Output, CacheHit, Metric, Bar],
        48..72 => &[Date, Wd, CacheHit, Metric, Share, Bar],
        36..48 => &[Date, CacheHit, Metric, Bar],
        _ => &[Date, Metric, Bar],
    }
}

fn draw_daily_pane(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &StatsTuiState,
    table_state: &mut TableState,
    view: &StatsView,
    pal: &Palette,
) -> (TableHit, HeaderHits<DailyCol>) {
    let active = state.active_pane() == StatsPane::Daily;
    let metric = state.query().metric;
    let sort = state.query().sort;
    let sep = pal.sep();
    let title = format!(
        " 1. Daily Usage ({}d {sep} {} {sep} {}) ",
        view.daily_rows.len(),
        sort.label(pal.ascii),
        metric.label(),
    );

    if view.daily_rows.is_empty() {
        let empty = Paragraph::new(Line::styled(
            "  No usage recorded in this window.",
            pal.dim(),
        ))
        .block(pal.pane_block(title, active));
        frame.render_widget(empty, area);
        return (
            TableHit {
                pane: area,
                ..Default::default()
            },
            HeaderHits::default(),
        );
    }

    let inner_w = usize::from(area.width.saturating_sub(2));
    let inner_h = usize::from(area.height.saturating_sub(2));
    let has_footer = inner_h >= 5;
    let narrow = inner_w < 48;
    let cols = daily_columns(inner_w);
    let total_mag = view.total_value.magnitude();

    let fixed_cols_w: usize = cols
        .iter()
        .filter_map(|c| c.fixed_width(narrow, sort).map(usize::from))
        .sum();
    let bar_w = inner_w
        .saturating_sub(2 + fixed_cols_w + cols.len().saturating_sub(1))
        .max(4);

    let bar_style = pal.fg(Color::Indexed(42));
    let val_style = pal.bold(Color::Cyan);
    let summary_hit_pct = view.summary.cache_hit_pct();
    let summary_cache_tot = view
        .summary
        .cache_read_tokens
        .saturating_add(view.summary.cache_write_tokens);

    let header =
        Row::new(cols.iter().map(|c| c.header(metric.label(), sort, pal))).style(pal.dim());
    let widths: Vec<Constraint> = cols.iter().map(|c| c.constraint(narrow, sort)).collect();
    let chrome = TableChrome {
        highlight: true,
        header: true,
        footer: has_footer,
        border: true,
    };
    let header_hits = compute_header_hits(area, cols, &widths, chrome, 2);
    let footer_opt = has_footer.then(|| {
        let cells = cols.iter().map(|col| match col {
            DailyCol::Date => Cell::from("Total"),
            DailyCol::Wd | DailyCol::Bar => Cell::from(""),
            DailyCol::Turns => right_cell(view.summary.requests.to_string(), val_style),
            DailyCol::Input => right_cell(fmt_tok(view.summary.input_tokens), val_style),
            DailyCol::Cache => right_cell(fmt_tok(summary_cache_tot), val_style),
            DailyCol::Output => right_cell(fmt_tok(view.summary.output_tokens), val_style),
            DailyCol::CacheHit => right_cell(
                format_pct(summary_hit_pct),
                pal.cache_hit_style(summary_hit_pct)
                    .add_modifier(Modifier::BOLD),
            ),
            DailyCol::Metric => {
                right_cell(metric.format_display(view.total_value, true), val_style)
            }
            DailyCol::Share => right_cell(if total_mag > 0.0 { "100%" } else { "-" }, val_style),
        });
        Row::new(cells).style(val_style)
    });

    let rows: Vec<Row<'static>> = view
        .daily_rows
        .iter()
        .map(|row| {
            let zero = row.summary.requests == 0;
            let row_style = if zero { pal.dim() } else { Style::default() };
            let m_style = if zero { pal.dim() } else { val_style };
            let hit_pct = row.summary.cache_hit_pct();
            let cache_tot = row
                .summary
                .cache_read_tokens
                .saturating_add(row.summary.cache_write_tokens);
            let bar = render_hbar(
                row.metric_value.magnitude(),
                view.daily_max,
                bar_w,
                pal.ascii,
            );

            let cells = cols.iter().map(|col| match col {
                DailyCol::Date => Cell::from(row.date.clone()),
                DailyCol::Wd => Cell::from(row.weekday),
                DailyCol::Turns => right_cell(row.summary.requests.to_string(), row_style),
                DailyCol::Input => right_cell(fmt_tok(row.summary.input_tokens), row_style),
                DailyCol::Cache => right_cell(fmt_tok(cache_tot), row_style),
                DailyCol::Output => right_cell(fmt_tok(row.summary.output_tokens), row_style),
                DailyCol::CacheHit => right_cell(format_pct(hit_pct), pal.cache_hit_style(hit_pct)),
                DailyCol::Metric => {
                    right_cell(metric.format_display(row.metric_value, true), m_style)
                }
                DailyCol::Share => {
                    let share = if total_mag > 0.0 {
                        format!("{:.1}%", (row.metric_value.magnitude() / total_mag) * 100.0)
                    } else {
                        "-".to_string()
                    };
                    right_cell(share, row_style)
                }
                DailyCol::Bar => Cell::from(bar.clone()).style(bar_style),
            });
            Row::new(cells).style(row_style)
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

    let table_hit = render_table_with_scrollbar(
        frame,
        table,
        (area, area),
        table_state,
        state.daily_idx(),
        view.daily_rows.len(),
        chrome,
    );
    (table_hit, header_hits)
}

fn draw_day_detail_pane(frame: &mut Frame<'_>, area: Rect, view: &StatsView, pal: &Palette) {
    let sep = pal.sep();
    let Some(day) = view.selected_day.as_ref() else {
        let panel = Paragraph::new(Line::styled("  Select a day to inspect.", pal.dim()))
            .block(pal.pane_block(" 2. Day Detail ".to_string(), false));
        frame.render_widget(panel, area);
        return;
    };

    let title = format!(" 2. Day: {} ({}) ", day.date, day.weekday);
    let s = &day.summary;
    let cost_str = StatsMetric::Cost.format_display(StatsMetric::Cost.extract(s), false);
    let spd = day
        .median_tok_per_sec
        .map_or_else(|| "-".to_string(), |v| format!("{v:.0}t/s"));
    let ttft = day
        .median_ttft_secs
        .map_or_else(|| "-".to_string(), |v| format!("{v:.2}s"));

    let mut lines = vec![
        Line::from(vec![
            Span::styled(" Turns ", pal.dim()),
            Span::styled(format_grouped_u64(s.requests), pal.bold(Color::White)),
            Span::styled(format!(" {sep} Cost "), pal.dim()),
            Span::styled(cost_str, pal.bold(Color::Green)),
        ]),
        Line::from(vec![
            Span::styled(" Total ", pal.dim()),
            Span::styled(fmt_tok(s.total_tokens), pal.bold(Color::Cyan)),
            Span::styled(format!(" {sep} Hit "), pal.dim()),
            Span::styled(
                format_pct(s.cache_hit_pct()),
                pal.cache_hit_style(s.cache_hit_pct())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" {sep} {spd} / {ttft}"), pal.dim()),
        ]),
    ];
    lines.extend(token_breakdown_lines(s, area, pal));
    frame.render_widget(
        Paragraph::new(lines).block(pal.pane_block(title, false)),
        area,
    );
}

fn draw_models_pane(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &StatsTuiState,
    table_state: &mut TableState,
    view: &StatsView,
    pal: &Palette,
) -> TableHit {
    let active = state.active_pane() == StatsPane::Models;
    let metric = state.query().metric;
    let title = format!(" 3. Models [{}] (v) ", state.query().model_scope.label());

    let models = state.active_models(view);
    if models.is_empty() {
        let empty = Paragraph::new(Line::styled("  No models recorded.", pal.dim()))
            .block(pal.pane_block(title, active));
        frame.render_widget(empty, area);
        return TableHit {
            pane: area,
            ..Default::default()
        };
    }

    let show_hit = usize::from(area.width.saturating_sub(2)) >= 34;
    let filter_glyph = if pal.ascii { "* " } else { "● " };

    let rows: Vec<Row<'static>> = models
        .iter()
        .map(|m| {
            let is_filtered = state.query().model_filter.as_deref() == Some(m.model.as_str());
            let val = metric.extract(m);
            let name = if is_filtered {
                format!("{filter_glyph}{}", m.model)
            } else {
                m.model.clone()
            };
            let name_style = if is_filtered {
                pal.bold(Color::Yellow)
            } else {
                Style::default()
            };
            let val_style = pal.bold(if is_filtered {
                Color::Yellow
            } else {
                Color::Cyan
            });
            let mut cells = vec![
                Cell::from(name).style(name_style),
                right_cell(metric.format_display(val, true), val_style),
            ];
            if show_hit {
                let hit_pct = m.cache_hit_pct();
                cells.push(right_cell(
                    format_pct(hit_pct),
                    pal.cache_hit_style(hit_pct),
                ));
            }
            Row::new(cells)
        })
        .collect();

    let widths = if show_hit {
        vec![
            Constraint::Min(10),
            Constraint::Length(10),
            Constraint::Length(6),
        ]
    } else {
        vec![Constraint::Min(10), Constraint::Length(10)]
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
        table_state,
        state.model_idx(),
        models.len(),
        TableChrome {
            highlight: active,
            header: false,
            footer: false,
            border: true,
        },
    )
}

fn draw_footer_bar(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &StatsTuiState,
    dataset: Option<&StatsDataset>,
    pal: &Palette,
) -> ClickRegions<DashboardClick> {
    if let Some(msg) = state.status_msg() {
        frame.render_widget(
            Paragraph::new(Line::styled(format!(" {msg}"), pal.bold(Color::Yellow))),
            area,
        );
        return ClickRegions::default();
    }

    let inset = Rect::new(
        area.x.saturating_add(1),
        area.y,
        area.width.saturating_sub(1),
        area.height,
    );
    let sep = format!(" {} ", pal.sep());
    let dim = pal.dim();
    let mut bar = SpanBar::new(inset);

    if let Some(model) = state.query().model_filter.as_deref() {
        let _ = bar.push(format!("Filtered: {model}"), dim);
        let _ = bar.push_sep_click(
            &sep,
            dim,
            "Esc Clear",
            dim,
            DashboardClick::ClearModelFilter,
        );
        let _ = bar.push_sep_click(&sep, dim, "1-5 Metric", dim, DashboardClick::CycleMetric);
        let _ = bar.push_sep_click(&sep, dim, "t Range", dim, DashboardClick::CycleRange);
        let _ = bar.push_sep_click(&sep, dim, "c Convs", dim, DashboardClick::OpenConversations);
        let _ = bar.push_sep_click(&sep, dim, "s Sort", dim, DashboardClick::CycleSort);
        let _ = bar.push_sep_click(&sep, dim, "r Refresh", dim, DashboardClick::Refresh);
        let _ = bar.push_sep_click(&sep, dim, "q Quit", dim, DashboardClick::Quit);
    } else if area.width < 92 {
        let _ = bar.push_sep_click(&sep, dim, "1-5 Metric", dim, DashboardClick::CycleMetric);
        let _ = bar.push_sep_click(&sep, dim, "t Range", dim, DashboardClick::CycleRange);
        let _ = bar.push_sep_click(&sep, dim, "c Convs", dim, DashboardClick::OpenConversations);
        let _ = bar.push_sep_click(&sep, dim, "s Sort", dim, DashboardClick::CycleSort);
        let _ = bar.push_sep_click(&sep, dim, "Tab Pane", dim, DashboardClick::TogglePane);
        let _ = bar.push_sep_click(&sep, dim, "q Quit", dim, DashboardClick::Quit);
    } else {
        let _ = bar.push_sep_click(&sep, dim, "1-5 Metric", dim, DashboardClick::CycleMetric);
        let _ = bar.push_sep_click(&sep, dim, "t Range", dim, DashboardClick::CycleRange);
        let _ = bar.push_sep_click(&sep, dim, "c Convs", dim, DashboardClick::OpenConversations);
        let _ = bar.push_sep_click(&sep, dim, "s Sort", dim, DashboardClick::CycleSort);
        let _ = bar.push_sep_click(
            &sep,
            dim,
            "v Day/Win",
            dim,
            DashboardClick::ToggleModelScope,
        );
        let _ = bar.push_sep_click(&sep, dim, "z Gaps", dim, DashboardClick::ToggleFillEmpty);
        let _ = bar.push_sep_click(&sep, dim, "g Heatmap", dim, DashboardClick::ToggleHeatmap);
        if dataset.is_some_and(|d| d.has_cli && d.has_desktop) {
            let _ = bar.push_sep_click(&sep, dim, "f Src", dim, DashboardClick::CycleSource);
        }
        let _ = bar.push_sep_click(&sep, dim, "Tab Pane", dim, DashboardClick::TogglePane);
        let _ = bar.push_sep_click(&sep, dim, "r Refresh", dim, DashboardClick::Refresh);
        let _ = bar.push_sep_click(&sep, dim, "q Quit", dim, DashboardClick::Quit);
    }

    let (line, hits) = bar.finish();
    frame.render_widget(Paragraph::new(line), inset);
    hits
}

#[cfg(test)]
mod tests {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    use super::*;
    use crate::stats::query::ModelScope;
    use crate::stats::scan::{UsageEvent, UsageSource};
    use crate::stats::SqliteStats;
    use crate::sys::time::unix_day_from_civil_date;

    fn test_dataset() -> StatsDataset {
        let d1 = unix_day_from_civil_date("2026-11-19".parse().unwrap());
        let d2 = unix_day_from_civil_date("2026-11-20".parse().unwrap());
        StatsDataset::for_tests(
            d2,
            "2026-11-20",
            "UTC+08:00",
            "2026-11-20",
            Some("2026-11-19".to_string()),
            SqliteStats::default(),
            true,
            false,
            vec![
                UsageEvent {
                    source: UsageSource::Cli,
                    local_day: d1,
                    model_group: "Gemini 4 Neon".to_string(),
                    pricing_key: Some("gemini-4-neon".to_string()),
                    input_tokens: 20_000,
                    cache_read_tokens: 80_000,
                    cache_write_tokens: 5_000,
                    output_tokens: 4_000,
                    thinking_tokens: 2_000,
                    total_tokens: 109_000,
                    cost_usd: Some(1.50),
                    decode_speed: Some(180.0),
                    ttft_secs: Some(1.2),
                },
                UsageEvent {
                    source: UsageSource::Cli,
                    local_day: d2,
                    model_group: "Gemini 4 Hydrogen".to_string(),
                    pricing_key: Some("gemini-4-hydrogen".to_string()),
                    input_tokens: 5_000,
                    cache_read_tokens: 45_000,
                    cache_write_tokens: 0,
                    output_tokens: 1_000,
                    thinking_tokens: 400,
                    total_tokens: 51_000,
                    cost_usd: Some(0.25),
                    decode_speed: Some(320.0),
                    ttft_secs: Some(0.6),
                },
            ],
        )
    }

    fn render_buffer_text(width: u16, height: u16, metric: StatsMetric, ascii: bool) -> String {
        let ds = test_dataset();
        let mut st = StatsTuiState::new(None, metric);
        let mut tables = DashboardTables::default();
        st.finish_scan(None);
        let view = ds.compute_view(st.query(), None);
        st.sync_with_view(&view);
        let pal = Palette {
            no_color: true,
            ascii,
        };
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                draw_stats_ui(frame, &st, &mut tables, Some(&ds), Some(&view), &pal);
            })
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    #[test]
    fn tui_renders_wide_medium_and_compact_dashboards_with_cache_rates() {
        let wide = render_buffer_text(120, 36, StatsMetric::Cost, false);
        assert!(wide.contains("agy-statusline stats"));
        assert!(wide.contains("Activity (20w · Cost ($))"));
        assert!(wide.contains("1. Daily Usage"));
        assert!(wide.contains("90.0%"));
        assert!(wide.contains("76.2%"));
        assert!(wide.contains("80.6%"));
        assert!(wide.contains("~$1.50"));
        assert!(wide.contains("2. Day: 2026-11-20"));

        let med = render_buffer_text(96, 30, StatsMetric::Cost, true);
        assert!(med.contains("Cache%"));
        assert!(med.contains("80.6%"));

        let compact = render_buffer_text(80, 24, StatsMetric::InputOutput, true);
        assert!(compact.contains("Summary (All | In+Out)"));
        assert!(compact.contains("Hit 80.6%"));
        assert!(!compact.contains("Active|"));
    }

    #[test]
    fn dashboard_handles_mouse_wheel_pane_clicks_and_offset_recovery() {
        let mouse = |kind: MouseEventKind, column: u16, row: u16| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        let ds = test_dataset();
        let mut screen = DashboardScreen {
            state: StatsTuiState::new(None, StatsMetric::Total),
            dataset: Some(ds),
            view: None,
            tables: DashboardTables::default(),
            hit_map: DashboardHitMap::default(),
            pending_scan: None,
        };
        screen.state.finish_scan(None);
        screen.poll_scan();
        *screen.tables.models.offset_mut() = 10;

        let pal = Palette {
            no_color: true,
            ascii: false,
        };
        let mut terminal = Terminal::new(TestBackend::new(120, 36)).unwrap();
        terminal.draw(|frame| screen.draw(frame, &pal)).unwrap();
        assert_eq!(screen.tables.models.offset(), 0);

        let daily_x = screen.hit_map.daily.pane.x + 4;
        let daily_y = screen.hit_map.daily.data_y0;
        screen.handle_mouse(mouse(MouseEventKind::ScrollDown, daily_x, daily_y), 36);
        assert_eq!(screen.state.daily_idx(), 1);

        screen.handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), daily_x, daily_y),
            36,
        );
        assert_eq!(screen.state.daily_idx(), 0);

        let models_x = screen.hit_map.models.pane.x + 3;
        screen.handle_mouse(
            mouse(
                MouseEventKind::Down(MouseButton::Left),
                models_x,
                screen.hit_map.models.pane.y,
            ),
            36,
        );
        assert_eq!(screen.state.query().model_scope, ModelScope::Window);

        let model0_y = screen.hit_map.models.data_y0;
        screen.handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), models_x, model0_y),
            36,
        );
        assert_eq!(screen.state.active_pane(), StatsPane::Models);
        assert!(screen.state.query().model_filter.is_none());
        screen.handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), models_x, model0_y),
            36,
        );
        assert!(screen.state.query().model_filter.is_some());
    }

    #[test]
    fn dashboard_mouse_clicks_controls_heatmap_headers_and_footer() {
        let m = |kind: MouseEventKind, column: u16, row: u16| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        let rect_text = |buf: &ratatui::buffer::Buffer, r: Rect| -> String {
            (r.x..r.right()).map(|x| buf[(x, r.y)].symbol()).collect()
        };
        let d1 = unix_day_from_civil_date("2026-11-19".parse().unwrap());
        let d2 = unix_day_from_civil_date("2026-11-20".parse().unwrap());
        let ds = StatsDataset::for_tests(
            d2,
            "2026-11-20",
            "UTC+08:00",
            "2026-11-20",
            Some("2026-11-19".to_string()),
            SqliteStats::default(),
            true,
            true,
            vec![
                UsageEvent {
                    source: UsageSource::Cli,
                    local_day: d1,
                    model_group: "Gemini 4 Neon".to_string(),
                    pricing_key: Some("gemini-4-neon".to_string()),
                    input_tokens: 20_000,
                    cache_read_tokens: 80_000,
                    cache_write_tokens: 5_000,
                    output_tokens: 4_000,
                    thinking_tokens: 2_000,
                    total_tokens: 109_000,
                    cost_usd: Some(1.50),
                    decode_speed: Some(180.0),
                    ttft_secs: Some(1.2),
                },
                UsageEvent {
                    source: UsageSource::Desktop,
                    local_day: d2,
                    model_group: "Gemini 4 Hydrogen".to_string(),
                    pricing_key: Some("gemini-4-hydrogen".to_string()),
                    input_tokens: 5_000,
                    cache_read_tokens: 45_000,
                    cache_write_tokens: 0,
                    output_tokens: 1_000,
                    thinking_tokens: 400,
                    total_tokens: 51_000,
                    cost_usd: Some(0.25),
                    decode_speed: Some(320.0),
                    ttft_secs: Some(0.6),
                },
            ],
        );
        let mut screen = DashboardScreen {
            state: StatsTuiState::new(None, StatsMetric::Total),
            dataset: Some(ds),
            view: None,
            tables: DashboardTables::default(),
            hit_map: DashboardHitMap::default(),
            pending_scan: None,
        };
        screen.state.finish_scan(None);
        screen.poll_scan();
        let pal = Palette {
            no_color: true,
            ascii: false,
        };
        let mut terminal = Terminal::new(TestBackend::new(120, 36)).unwrap();
        terminal.draw(|frame| screen.draw(frame, &pal)).unwrap();

        // Top control bar: Metric pill, Range pill, Source filter pill.
        let cost_pill = screen
            .hit_map
            .controls
            .find(|c| *c == DashboardClick::Metric(StatsMetric::Cost))
            .unwrap();
        assert!(rect_text(terminal.backend().buffer(), cost_pill).contains("2:Cost ($)"));
        assert_eq!(
            screen.handle_mouse(
                m(
                    MouseEventKind::Down(MouseButton::Left),
                    cost_pill.x + 1,
                    cost_pill.y
                ),
                36,
            ),
            StatsAction::Continue
        );
        assert_eq!(screen.state.query().metric, StatsMetric::Cost);

        let range_7d = screen
            .hit_map
            .controls
            .find(|c| *c == DashboardClick::Range(RangePreset::Days(7)))
            .unwrap();
        assert_eq!(rect_text(terminal.backend().buffer(), range_7d), "7d");
        screen.handle_mouse(
            m(
                MouseEventKind::Down(MouseButton::Left),
                range_7d.x,
                range_7d.y,
            ),
            36,
        );
        assert_eq!(screen.state.query().range, RangePreset::Days(7));

        let src_pill = screen
            .hit_map
            .controls
            .find(|c| *c == DashboardClick::CycleSource)
            .unwrap();
        assert!(rect_text(terminal.backend().buffer(), src_pill).contains("Src:All"));
        screen.handle_mouse(
            m(
                MouseEventKind::Down(MouseButton::Left),
                src_pill.x + 1,
                src_pill.y,
            ),
            36,
        );
        assert_eq!(screen.state.query().source_filter, SourceFilter::Cli);
        screen.state.cycle_source();
        screen.state.cycle_source();
        assert_eq!(screen.state.query().source_filter, SourceFilter::All);
        screen.poll_scan();
        terminal.draw(|frame| screen.draw(frame, &pal)).unwrap();

        // Heatmap cell click selects exact date 2026-11-19.
        let hm_cell = screen
            .hit_map
            .heatmap
            .iter()
            .find(|c| c.date == "2026-11-19")
            .unwrap()
            .rect;
        screen.handle_mouse(
            m(
                MouseEventKind::Down(MouseButton::Left),
                hm_cell.x,
                hm_cell.y,
            ),
            36,
        );
        assert_eq!(screen.state.selected_date(), Some("2026-11-19"));
        assert_eq!(screen.state.daily_idx(), 1);

        // Daily table sortable column headers: Date, Cache%, Metric.
        let hdr_y = screen.hit_map.daily_headers.y;
        let date_hdr = screen
            .hit_map
            .daily_headers
            .cols
            .iter()
            .find(|c| c.col == DailyCol::Date)
            .copied()
            .unwrap();
        assert!(rect_text(
            terminal.backend().buffer(),
            Rect::new(date_hdr.x0, hdr_y, date_hdr.x1 - date_hdr.x0, 1)
        )
        .contains("Date"));
        screen.handle_mouse(
            m(
                MouseEventKind::Down(MouseButton::Left),
                date_hdr.x0 + 1,
                hdr_y,
            ),
            36,
        );
        assert_eq!(screen.state.query().sort, DailySort::DateAsc);

        let hit_hdr = screen
            .hit_map
            .daily_headers
            .cols
            .iter()
            .find(|c| c.col == DailyCol::CacheHit)
            .copied()
            .unwrap();
        screen.handle_mouse(
            m(
                MouseEventKind::Down(MouseButton::Left),
                hit_hdr.x0 + 1,
                hdr_y,
            ),
            36,
        );
        assert_eq!(screen.state.query().sort, DailySort::CacheHitAsc);

        let metric_hdr = screen
            .hit_map
            .daily_headers
            .cols
            .iter()
            .find(|c| c.col == DailyCol::Metric)
            .copied()
            .unwrap();
        screen.handle_mouse(
            m(
                MouseEventKind::Down(MouseButton::Left),
                metric_hdr.x0 + 1,
                hdr_y,
            ),
            36,
        );
        assert_eq!(screen.state.query().sort, DailySort::MetricDesc);

        // Cross-filter model and clear via top-bar yellow dismiss pill.
        let view_ref = screen.view.clone().unwrap();
        screen.state.toggle_model_filter(0, &view_ref);
        assert!(screen.state.query().model_filter.is_some());
        screen.poll_scan();
        terminal.draw(|frame| screen.draw(frame, &pal)).unwrap();
        let clear_pill = screen
            .hit_map
            .controls
            .find(|c| *c == DashboardClick::ClearModelFilter)
            .unwrap();
        assert!(rect_text(terminal.backend().buffer(), clear_pill).contains("×"));
        screen.handle_mouse(
            m(
                MouseEventKind::Down(MouseButton::Left),
                clear_pill.x + 1,
                clear_pill.y,
            ),
            36,
        );
        assert_eq!(screen.state.query().model_filter, None);

        // Footer actions: OpenConversations, ToggleHeatmap, Refresh, Quit.
        screen.poll_scan();
        terminal.draw(|frame| screen.draw(frame, &pal)).unwrap();
        let convs_btn = screen
            .hit_map
            .footer
            .find(|c| *c == DashboardClick::OpenConversations)
            .unwrap();
        assert_eq!(rect_text(terminal.backend().buffer(), convs_btn), "c Convs");
        assert_eq!(
            screen.handle_mouse(
                m(
                    MouseEventKind::Down(MouseButton::Left),
                    convs_btn.x + 1,
                    convs_btn.y
                ),
                36,
            ),
            StatsAction::OpenConversations
        );
        let hm_btn = screen
            .hit_map
            .footer
            .find(|c| *c == DashboardClick::ToggleHeatmap)
            .unwrap();
        screen.handle_mouse(
            m(
                MouseEventKind::Down(MouseButton::Left),
                hm_btn.x + 1,
                hm_btn.y,
            ),
            36,
        );
        assert!(!screen.state.heatmap_visible(36));
        let ref_btn = screen
            .hit_map
            .footer
            .find(|c| *c == DashboardClick::Refresh)
            .unwrap();
        assert_eq!(
            screen.handle_mouse(
                m(
                    MouseEventKind::Down(MouseButton::Left),
                    ref_btn.x + 1,
                    ref_btn.y
                ),
                36,
            ),
            StatsAction::Refresh
        );
        let quit_btn = screen
            .hit_map
            .footer
            .find(|c| *c == DashboardClick::Quit)
            .unwrap();
        assert_eq!(
            screen.handle_mouse(
                m(
                    MouseEventKind::Down(MouseButton::Left),
                    quit_btn.x + 1,
                    quit_btn.y
                ),
                36,
            ),
            StatsAction::Quit
        );

        // Compact control bar: left-click cycles range forward, right-click cycles backward.
        let mut narrow_term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        narrow_term.draw(|frame| screen.draw(frame, &pal)).unwrap();
        let compact_range = screen
            .hit_map
            .controls
            .find(|c| *c == DashboardClick::CycleRange)
            .unwrap();
        screen.handle_mouse(
            m(
                MouseEventKind::Down(MouseButton::Left),
                compact_range.x + 1,
                compact_range.y,
            ),
            24,
        );
        assert_eq!(screen.state.query().range, RangePreset::Days(14));
        screen.handle_mouse(
            m(
                MouseEventKind::Down(MouseButton::Right),
                compact_range.x + 1,
                compact_range.y,
            ),
            24,
        );
        assert_eq!(screen.state.query().range, RangePreset::Days(7));
    }

    #[test]
    fn dashboard_renders_quota_strip_diagnostics_and_preserves_mouse_hit_alignment() {
        use crate::stats::{QuotaPoolReport, QuotaWindowReport, ScanDiagnostics, StatsQuotaReport};

        let mut ds = test_dataset();
        ds.quota = Some(StatsQuotaReport {
            source: "hub_rpc".to_string(),
            from_cache: false,
            updated_at: 1_700_000_000,
            age_secs: 0,
            plan_tier: Some("Ultra 20x".to_string()),
            ai_credits: Some(1250),
            pools: vec![
                QuotaPoolReport {
                    name: "Gemini".to_string(),
                    short_window: Some(QuotaWindowReport {
                        label: "5h".to_string(),
                        remaining_pct: 65.0,
                        reset_compact: Some("2h10m".to_string()),
                        ..Default::default()
                    }),
                    long_window: Some(QuotaWindowReport {
                        label: "7d".to_string(),
                        remaining_pct: 90.0,
                        reset_compact: Some("4d".to_string()),
                        ..Default::default()
                    }),
                },
                QuotaPoolReport {
                    name: "Argon".to_string(),
                    short_window: None,
                    long_window: Some(QuotaWindowReport {
                        label: "argon".to_string(),
                        remaining_pct: 55.0,
                        reset_compact: Some("5d".to_string()),
                        ..Default::default()
                    }),
                },
                QuotaPoolReport {
                    name: "Claude/3P".to_string(),
                    short_window: Some(QuotaWindowReport {
                        label: "3p 5h".to_string(),
                        remaining_pct: 40.0,
                        reset_compact: Some("1h".to_string()),
                        ..Default::default()
                    }),
                    long_window: None,
                },
            ],
            credits: Vec::new(),
        });
        ds.diagnostics = ScanDiagnostics {
            retry_count: 3,
            retries_429: 2,
            retries_503: 1,
            total_retry_delay_secs: 12.0,
            compaction_steps: 2,
            ..Default::default()
        };

        let mut screen = DashboardScreen {
            state: StatsTuiState::new(None, StatsMetric::Total),
            dataset: Some(ds),
            view: None,
            tables: DashboardTables::default(),
            hit_map: DashboardHitMap::default(),
            pending_scan: None,
        };
        screen.state.finish_scan(None);
        screen.poll_scan();

        let pal = Palette {
            no_color: true,
            ascii: false,
        };
        let mut terminal = Terminal::new(TestBackend::new(120, 36)).unwrap();
        terminal.draw(|frame| screen.draw(frame, &pal)).unwrap();
        let wide: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            wide.contains("Quota (Ultra 20x · AI credits 1250 · live)"),
            "{wide}"
        );
        assert!(
            wide.contains("Gemini 5h 65% (2h10m), 7d 90% (4d)"),
            "{wide}"
        );
        assert!(wide.contains("Argon 7d 55% (5d)"), "{wide}");
        assert!(wide.contains("Claude/3P 5h 40% (1h)"), "{wide}");
        assert!(wide.contains("2×429 1×503 (12s) · 2 cmp"), "{wide}");

        // Verify mouse hit-testing remains aligned below the 3-row quota strip.
        let m = |kind, column, row| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        let daily_y = screen.hit_map.daily.data_y0 + 1;
        let daily_x = screen.hit_map.daily.pane.x + 3;
        screen.handle_mouse(
            m(MouseEventKind::Down(MouseButton::Left), daily_x, daily_y),
            36,
        );
        assert_eq!(screen.state.daily_idx(), 1);
        assert_eq!(screen.state.selected_date(), Some("2026-11-19"));

        // Verify compact 80x24 viewport renders the 1-line borderless quota strip cleanly.
        let mut narrow = Terminal::new(TestBackend::new(80, 24)).unwrap();
        narrow.draw(|frame| screen.draw(frame, &pal)).unwrap();
        let compact: String = narrow
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            compact.contains("Quota Ultra 20x cr:1250 [live]"),
            "{compact}"
        );
        assert!(compact.contains("Gemini 5h 65% (2h10m)"), "{compact}");
    }
}
