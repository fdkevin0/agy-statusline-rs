//! Interactive `ratatui` + `crossterm` TUI for `agy-statusline stats`.
//!
//! Keyboard-first dashboard displaying daily usage with color-coded per-day `Cache%`,
//! selectable additive metrics, a 20-week activity heatmap, selected-day composition
//! inspector, and per-day / window model breakdown with cross-filtering.

use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, TryRecvError};
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{enable_raw_mode, EnterAlternateScreen};
use jiff::civil::Date;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Paragraph, Row, Scrollbar, ScrollbarOrientation,
    ScrollbarState, Table, TableState,
};
use ratatui::{Frame, Terminal};
use unicode_width::UnicodeWidthStr;

use super::metric::{
    format_grouped_u64, format_pct, format_usd_short, heatmap_level_f64, StatsMetric, UsageCounters,
};
use super::query::{HeatmapWeek, SourceFilter, StatsDataset, StatsView, WEEKDAY_NAMES};
use super::render::{build_month_header, ASCII_HEATMAP, UNICODE_HEATMAP};
use super::state::{StatsAction, StatsPane, StatsTuiState};
use crate::format::format_tokens_string as fmt_tok;
use crate::layout::save_stats_metric;
use crate::tui::TerminalGuard;

const UNICODE_BAR_EIGHTHS: [char; 9] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];

#[derive(Default)]
struct TableStates {
    daily: TableState,
    models: TableState,
}

struct Palette {
    no_color: bool,
    ascii: bool,
}

impl Palette {
    fn fg(&self, fg: Color) -> Style {
        if self.no_color {
            Style::default()
        } else {
            Style::default().fg(fg)
        }
    }

    fn bold(&self, fg: Color) -> Style {
        self.fg(fg).add_modifier(Modifier::BOLD)
    }

    fn dim(&self) -> Style {
        if self.no_color {
            Style::default().add_modifier(Modifier::DIM)
        } else {
            Style::default().fg(Color::DarkGray)
        }
    }

    /// Threshold style for cache hit percentages (`>=80%` green, `50–79%` yellow, `<50%` red).
    fn cache_hit_style(&self, pct: Option<f64>) -> Style {
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

    fn pill(&self, active: bool) -> Style {
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

    fn selected_row(&self, pane_active: bool) -> Style {
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

    fn heatmap(&self, level: usize, selected: bool) -> Style {
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

    fn sep(&self) -> &'static str {
        if self.ascii { "|" } else { "·" }
    }

    fn pane_block(&self, title: String, active: bool) -> Block<'static> {
        let border_style = if self.no_color {
            Style::default().add_modifier(if active { Modifier::BOLD } else { Modifier::DIM })
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
            .border_type(if self.ascii { BorderType::Plain } else { BorderType::Rounded })
            .border_style(border_style)
            .title(Span::styled(title, title_style))
    }
}

fn spawn_dataset_scan(
    roots: Vec<(&'static str, PathBuf)>,
    cache_dir: Option<PathBuf>,
    now_unix_secs: u64,
) -> mpsc::Receiver<(StatsDataset, Duration)> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let start = Instant::now();
        let ds = StatsDataset::collect(&roots, cache_dir.as_deref(), now_unix_secs);
        let _ = tx.send((ds, start.elapsed()));
    });
    rx
}

/// Launches the interactive `agy-statusline stats` TUI dashboard.
pub fn run_stats_tui(
    roots: Vec<(&'static str, PathBuf)>,
    config_path: Option<&Path>,
    opts: &super::StatsOptions<'_>,
) -> io::Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "interactive stats requires a TTY (use --plain or --json)",
        ));
    }

    let mut state = StatsTuiState::new(opts.days, opts.metric);
    let mut tables = TableStates::default();
    let pal = Palette {
        no_color: opts.no_color,
        ascii: opts.ascii,
    };
    let cache_owned = opts.cache_dir.map(Path::to_path_buf);

    let _guard = TerminalGuard::enter();
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut dataset: Option<StatsDataset> = None;
    let mut view: Option<StatsView> = None;
    let mut daily_visible_rows: usize = 10;
    let mut pending_scan = Some(spawn_dataset_scan(
        roots.clone(),
        cache_owned.clone(),
        opts.now_unix_secs,
    ));

    loop {
        if let Some(rx) = pending_scan.as_ref() {
            match rx.try_recv() {
                Ok((ds, elapsed)) => {
                    let is_refresh = dataset.is_some();
                    let (turns, dbs) = (ds.events.len(), ds.sqlite.db_files);
                    dataset = Some(ds);
                    state.loading = false;
                    state.view_dirty = true;
                    if is_refresh {
                        state.status_msg = Some(format!(
                            "Refreshed {turns} turns across {dbs} DBs in {:.2}s",
                            elapsed.as_secs_f64()
                        ));
                    }
                    pending_scan = None;
                }
                Err(TryRecvError::Disconnected) => {
                    state.loading = false;
                    state.status_msg = Some("Background scan interrupted.".to_string());
                    pending_scan = None;
                }
                Err(TryRecvError::Empty) => {}
            }
        }

        if state.view_dirty {
            if let Some(ds) = dataset.as_ref() {
                let computed = ds.compute_view(&state.query, state.selected_date.as_deref());
                state.sync_with_view(&computed);
                view = Some(computed);
                state.view_dirty = false;
            }
        }

        terminal.draw(|frame| {
            daily_visible_rows =
                draw_stats_ui(frame, &state, &mut tables, dataset.as_ref(), view.as_ref(), &pal);
        })?;

        // Poll only while a background scan is active; block at 0% CPU when idle.
        if pending_scan.is_some() && !event::poll(Duration::from_millis(50))? {
            continue;
        }

        if let Event::Key(key) = event::read()? {
            if key.kind == KeyEventKind::Press {
                let term_h = terminal.size().map_or(30, |s| s.height);
                let page_size = daily_visible_rows.max(5);
                match state.handle_key(key.code, key.modifiers, view.as_ref(), page_size, term_h) {
                    StatsAction::Quit => break,
                    StatsAction::Refresh => {
                        if pending_scan.is_none() {
                            state.loading = true;
                            state.status_msg = Some("Refreshing local AGY usage...".to_string());
                            let now = jiff::Timestamp::now().as_second().max(0) as u64;
                            pending_scan =
                                Some(spawn_dataset_scan(roots.clone(), cache_owned.clone(), now));
                        }
                    }
                    StatsAction::Continue => {}
                }
            }
        }
    }

    drop(terminal);
    drop(_guard);

    if state.query.metric != opts.metric {
        if let Some(cfg_path) = config_path {
            let _ = save_stats_metric(cfg_path, state.query.metric);
        }
    }
    Ok(())
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
    tables: &mut TableStates,
    dataset: Option<&StatsDataset>,
    view: Option<&StatsView>,
    pal: &Palette,
) -> usize {
    let area = frame.area();
    let show_heatmap = state.heatmap_visible(area.height) && area.width >= 96;

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(if show_heatmap { 11 } else { 3 }),
            Constraint::Min(8),
            Constraint::Length(1),
        ])
        .split(area);

    draw_control_bar(frame, chunks[0], state, dataset, pal);

    let mut daily_visible_rows = 10;
    if let Some(v) = view {
        if show_heatmap {
            let top = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
                .split(chunks[1]);
            draw_summary_card(frame, top[0], state, v, pal);
            draw_heatmap_pane(frame, top[1], state, v, pal);
        } else {
            draw_compact_summary(frame, chunks[1], state, v, pal);
        }

        if area.width >= 96 {
            let main = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
                .split(chunks[2]);
            daily_visible_rows =
                draw_daily_pane(frame, main[0], state, &mut tables.daily, v, pal);

            let right = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(if main[1].height >= 19 { 11 } else { 9 }),
                    Constraint::Min(5),
                ])
                .split(main[1]);
            draw_day_detail_pane(frame, right[0], v, pal);
            draw_models_pane(frame, right[1], state, &mut tables.models, v, pal);
        } else {
            match state.active_pane {
                StatsPane::Daily => {
                    daily_visible_rows =
                        draw_daily_pane(frame, chunks[2], state, &mut tables.daily, v, pal);
                }
                StatsPane::Models => {
                    let stacked = Layout::default()
                        .direction(Direction::Vertical)
                        .constraints([
                            Constraint::Length(if chunks[2].height >= 16 { 9 } else { 7 }),
                            Constraint::Min(5),
                        ])
                        .split(chunks[2]);
                    draw_day_detail_pane(frame, stacked[0], v, pal);
                    draw_models_pane(frame, stacked[1], state, &mut tables.models, v, pal);
                }
            }
        }
    } else {
        let msg = if state.loading {
            "Scanning local AGY conversation databases & pricing cache..."
        } else {
            "No local AGY usage data found."
        };
        let loading = Paragraph::new(Line::styled(format!("  {msg}"), pal.bold(Color::Yellow)))
            .block(pal.pane_block(" Loading ".to_string(), true));
        frame.render_widget(loading, chunks[2]);
    }

    draw_footer_bar(frame, chunks[3], state, dataset, pal);
    daily_visible_rows
}

fn draw_control_bar(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &StatsTuiState,
    dataset: Option<&StatsDataset>,
    pal: &Palette,
) {
    let sep = pal.sep();
    let title = match dataset {
        Some(ds) => format!(
            " agy-statusline stats {sep} Local {} ({}) {}",
            ds.today_local,
            ds.timezone,
            if state.loading { "[refreshing...] " } else { "" }
        ),
        None => " agy-statusline stats ".to_string(),
    };

    let compact = area.width < 100;
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

    push((if compact { " " } else { " Metric: " }).to_string(), pal.dim());
    for (idx, &preset) in StatsMetric::PRESETS.iter().enumerate() {
        let label = if compact {
            format!("{}:{}", idx + 1, preset.compact_label())
        } else {
            format!(" {}:{} ", idx + 1, preset.label())
        };
        if !push(label, pal.pill(state.query.metric == preset)) {
            break;
        }
        let _ = push(" ".to_string(), Style::default());
    }

    if push(format!(" {sep} "), pal.dim()) {
        if compact {
            let _ = push(format!("[{}]", state.query.range.label()), pal.pill(true));
        } else {
            for &range in &state.range_options {
                let active = state.query.range == range;
                let text = if active {
                    format!("[{}]", range.label())
                } else {
                    range.label()
                };
                if !push(text, pal.pill(active)) {
                    break;
                }
                let _ = push(" ".to_string(), Style::default());
            }
        }
    }

    let show_src = dataset.is_some_and(|d| d.has_cli && d.has_desktop)
        || state.query.source_filter != SourceFilter::All;
    if show_src {
        let style = if state.query.source_filter == SourceFilter::All {
            pal.pill(false)
        } else {
            pal.bold(Color::Yellow)
        };
        let _ = push(format!(" Src:{} ", state.query.source_filter.label()), style);
    }

    if let Some(model) = state.query.model_filter.as_deref() {
        let close = if pal.ascii { "x" } else { "×" };
        let style = if pal.no_color {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
                .bg(Color::Yellow)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD)
        };
        let label = crate::render::truncate_to_width(model, 18, if pal.ascii { "..." } else { "…" });
        let _ = push(format!(" {close} {label} "), style);
    }

    let bar = Paragraph::new(Line::from(spans)).block(pal.pane_block(title, false));
    frame.render_widget(bar, area);
}

fn draw_summary_card(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &StatsTuiState,
    view: &StatsView,
    pal: &Palette,
) {
    let metric = state.query.metric;
    let sep = pal.sep();
    let title = format!(" Summary ({} {sep} {}) ", state.query.range.label(), metric.label());

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
        kv("Total", metric.format_display(view.total_value, false), Color::Cyan),
        kv(
            "Daily Avg",
            format!("{} / active day", metric.format_average(view.total_value, view.active_days)),
            Color::Green,
        ),
        kv("Peak Day", peak_str, Color::Yellow),
        kv("Active Days", format!("{} / {}d", view.active_days, view.span_days), Color::White),
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

    if let Some(model) = &state.query.model_filter {
        lines.push(kv("Model", model.clone(), Color::Yellow));
    } else if metric == StatsMetric::Cost && view.summary.unpriced_requests > 0 {
        lines.push(Line::styled(
            format!("  Note: {} unpriced turns excluded", view.summary.unpriced_requests),
            pal.dim(),
        ));
    }

    frame.render_widget(Paragraph::new(lines).block(pal.pane_block(title, false)), area);
}

fn draw_compact_summary(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &StatsTuiState,
    view: &StatsView,
    pal: &Palette,
) {
    let metric = state.query.metric;
    let sep = format!(" {} ", pal.sep());
    let peak_str = match &view.peak_day {
        Some((date, _, val)) => format!(
            "{} ({})",
            metric.format_display(*val, true),
            short_month_day(date)
        ),
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
            if spans.is_empty() { Style::default() } else { pal.dim() },
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
        format!("{}/d", metric.format_average(view.total_value, view.active_days)),
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

    let title = format!(" Summary ({} {} {}) ", state.query.range.label(), pal.sep(), metric.label());
    frame.render_widget(Paragraph::new(Line::from(spans)).block(pal.pane_block(title, false)), area);
}

fn draw_heatmap_pane(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &StatsTuiState,
    view: &StatsView,
    pal: &Palette,
) {
    let metric = state.query.metric;
    let sep = pal.sep();
    let title = format!(" Activity (20w {sep} {}) ", metric.label());

    let inner_w = usize::from(area.width.saturating_sub(2));
    // k weeks occupy 7 ("  Mon  ") + 3*k - 1 = 6 + 3*k columns.
    let max_weeks = (inner_w.saturating_sub(6) / 3).min(view.heatmap_weeks.len());
    let visible_weeks: &[HeatmapWeek] =
        &view.heatmap_weeks[view.heatmap_weeks.len().saturating_sub(max_weeks)..];
    let glyphs = if pal.ascii { &ASCII_HEATMAP } else { &UNICODE_HEATMAP };
    let peak_mag = view.heatmap_peak.as_ref().map_or(0.0, |(_, _, v)| v.magnitude());
    let peak_note = match &view.heatmap_peak {
        Some((date, wd, val)) => format!("peak {} on {date} ({wd})", metric.format_display(*val, true)),
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

    let week_dates: Vec<Option<Date>> = visible_weeks.iter().map(|w| w.week_start.parse().ok()).collect();
    lines.push(Line::styled(format!("       {}", build_month_header(&week_dates)), pal.dim()));

    for (wd_idx, wd_name) in WEEKDAY_NAMES.iter().enumerate() {
        let mut row_spans = vec![Span::styled(format!("  {wd_name}  "), pal.dim())];
        for (w_idx, week) in visible_weeks.iter().enumerate() {
            match &week.daily[wd_idx] {
                Some((date, val)) => {
                    let lvl = heatmap_level_f64(val.magnitude(), peak_mag);
                    let is_sel = state.selected_date.as_deref() == Some(date.as_str());
                    row_spans.push(Span::styled(glyphs[lvl], pal.heatmap(lvl, is_sel)));
                }
                None => row_spans.push(Span::raw("  ")),
            }
            if w_idx + 1 < visible_weeks.len() {
                row_spans.push(Span::raw(" "));
            }
        }
        lines.push(Line::from(row_spans));
    }

    frame.render_widget(Paragraph::new(lines).block(pal.pane_block(title, false)), area);
}

fn right_cell(text: impl Into<String>, style: Style) -> Cell<'static> {
    Cell::from(Text::from(text.into()).alignment(Alignment::Right)).style(style)
}

#[derive(Debug, Clone, Copy)]
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
    fn fixed_width(self, narrow: bool) -> Option<u16> {
        match self {
            Self::Date => Some(10),
            Self::Wd => Some(3),
            Self::Turns | Self::Input | Self::Cache | Self::Output | Self::CacheHit | Self::Share => {
                Some(6)
            }
            Self::Metric => Some(if narrow { 9 } else { 10 }),
            Self::Bar => None,
        }
    }

    fn constraint(self, narrow: bool) -> Constraint {
        match self.fixed_width(narrow) {
            Some(w) => Constraint::Length(w),
            None => Constraint::Min(4),
        }
    }

    fn header(self, metric_hdr: &'static str, pal: &Palette) -> Cell<'static> {
        match self {
            Self::Date => Cell::from("Date"),
            Self::Wd => Cell::from("Wd"),
            Self::Turns => right_cell("Turns", pal.dim()),
            Self::Input => right_cell("Input", pal.dim()),
            Self::Cache => right_cell("Cache", pal.dim()),
            Self::Output => right_cell("Output", pal.dim()),
            Self::CacheHit => right_cell("Cache%", pal.dim()),
            Self::Metric => right_cell(metric_hdr, pal.dim()),
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
) -> usize {
    let active = state.active_pane == StatsPane::Daily;
    let metric = state.query.metric;
    let sep = pal.sep();
    let title = format!(
        " 1. Daily Usage ({}d {sep} {} {sep} {}) ",
        view.daily_rows.len(),
        state.query.sort.label(pal.ascii),
        metric.label(),
    );

    if view.daily_rows.is_empty() {
        let empty = Paragraph::new(Line::styled("  No usage recorded in this window.", pal.dim()))
            .block(pal.pane_block(title, active));
        frame.render_widget(empty, area);
        return 0;
    }

    let inner_w = usize::from(area.width.saturating_sub(2));
    let inner_h = usize::from(area.height.saturating_sub(2));
    let has_footer = inner_h >= 5;
    let narrow = inner_w < 48;
    let cols = daily_columns(inner_w);

    let total_mag = view.total_value.magnitude();
    let metric_hdr = metric.label();

    // Exact non-bar footprint: 2-col highlight symbol + fixed columns + 1-col spacing between columns.
    let fixed_cols_w: usize = cols
        .iter()
        .filter_map(|c| c.fixed_width(narrow).map(usize::from))
        .sum();
    let non_bar_w = 2 + fixed_cols_w + cols.len().saturating_sub(1);
    let bar_w = inner_w.saturating_sub(non_bar_w).max(4);

    let bar_style = pal.fg(Color::Indexed(42));
    let val_style = pal.bold(Color::Cyan);
    let summary_hit_pct = view.summary.cache_hit_pct();
    let summary_hit_str = format_pct(summary_hit_pct);
    let summary_hit_style = pal.cache_hit_style(summary_hit_pct).add_modifier(Modifier::BOLD);
    let summary_cache_tot = view
        .summary
        .cache_read_tokens
        .saturating_add(view.summary.cache_write_tokens);
    let summary_metric_str = metric.format_display(view.total_value, true);

    let header = Row::new(cols.iter().map(|c| c.header(metric_hdr, pal))).style(pal.dim());
    let widths: Vec<Constraint> = cols.iter().map(|c| c.constraint(narrow)).collect();
    let footer_opt = has_footer.then(|| {
        let cells = cols.iter().map(|col| match col {
            DailyCol::Date => Cell::from("Total"),
            DailyCol::Wd | DailyCol::Bar => Cell::from(""),
            DailyCol::Turns => right_cell(view.summary.requests.to_string(), val_style),
            DailyCol::Input => right_cell(fmt_tok(view.summary.input_tokens), val_style),
            DailyCol::Cache => right_cell(fmt_tok(summary_cache_tot), val_style),
            DailyCol::Output => right_cell(fmt_tok(view.summary.output_tokens), val_style),
            DailyCol::CacheHit => right_cell(summary_hit_str.clone(), summary_hit_style),
            DailyCol::Metric => right_cell(summary_metric_str.clone(), val_style),
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
            let hit_str = format_pct(hit_pct);
            let hit_style = pal.cache_hit_style(hit_pct);
            let val_str = metric.format_display(row.metric_value, true);
            let bar = render_hbar(row.metric_value.magnitude(), view.daily_max, bar_w, pal.ascii);
            let cache_tot = row
                .summary
                .cache_read_tokens
                .saturating_add(row.summary.cache_write_tokens);

            let cells = cols.iter().map(|col| match col {
                DailyCol::Date => Cell::from(row.date.clone()),
                DailyCol::Wd => Cell::from(row.weekday),
                DailyCol::Turns => right_cell(row.summary.requests.to_string(), row_style),
                DailyCol::Input => right_cell(fmt_tok(row.summary.input_tokens), row_style),
                DailyCol::Cache => right_cell(fmt_tok(cache_tot), row_style),
                DailyCol::Output => right_cell(fmt_tok(row.summary.output_tokens), row_style),
                DailyCol::CacheHit => right_cell(hit_str.clone(), hit_style),
                DailyCol::Metric => right_cell(val_str.clone(), m_style),
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
    if let Some(foot) = footer_opt {
        table = table.footer(foot);
    }

    table_state.select(Some(state.daily_idx));
    frame.render_stateful_widget(table, area, table_state);

    let visible_rows = inner_h.saturating_sub(1 + usize::from(has_footer));
    if view.daily_rows.len() > visible_rows && area.height > 3 {
        let mut sb = ScrollbarState::new(view.daily_rows.len()).position(state.daily_idx);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None),
            Rect::new(area.x, area.y + 1, area.width, area.height.saturating_sub(2)),
            &mut sb,
        );
    }
    visible_rows
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
        build_composition_bar(s, usize::from(area.width.saturating_sub(4)).clamp(10, 42), pal),
    ];

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

    lines.push(comp_row(Color::Blue, "Input", s.input_tokens));
    lines.push(comp_row(Color::Green, "CacheR", s.cache_read_tokens));
    if s.cache_write_tokens > 0 || area.height >= 10 {
        lines.push(comp_row(Color::Yellow, "CacheW", s.cache_write_tokens));
    }
    lines.push(comp_row(Color::Magenta, "Output", s.output_tokens));
    lines.push(Line::from(vec![
        Span::styled(format!("   {branch} Think "), pal.dim()),
        Span::styled(format!("{:>6}", fmt_tok(s.thinking_tokens)), pal.bold(Color::White)),
        Span::styled(
            format!(" ({} out)", pct_of(s.thinking_tokens, s.output_tokens).trim()),
            pal.dim(),
        ),
    ]));

    frame.render_widget(Paragraph::new(lines).block(pal.pane_block(title, false)), area);
}

fn build_composition_bar(s: &super::PeriodSummary, width: usize, pal: &Palette) -> Line<'static> {
    if s.total_tokens == 0 || width == 0 {
        let empty = if pal.ascii { "." } else { "·" }.repeat(width);
        return Line::styled(format!(" {empty}"), pal.dim());
    }
    let total = s.total_tokens as f64;
    let parts = [
        (s.input_tokens, Color::Blue, if pal.ascii { "I" } else { "█" }),
        (s.cache_read_tokens, Color::Green, if pal.ascii { "R" } else { "█" }),
        (s.cache_write_tokens, Color::Yellow, if pal.ascii { "W" } else { "█" }),
        (s.output_tokens, Color::Magenta, if pal.ascii { "O" } else { "█" }),
    ];

    let non_zero: Vec<_> = parts.into_iter().filter(|(v, _, _)| *v > 0).collect();
    let mut spans = vec![Span::raw(" ")];
    let mut used = 0usize;
    for (idx, (val, color, glyph)) in non_zero.iter().enumerate() {
        let max_here = width.saturating_sub(used + non_zero.len().saturating_sub(idx + 1));
        if max_here == 0 {
            continue;
        }
        let seg_w = if idx + 1 == non_zero.len() {
            width.saturating_sub(used)
        } else {
            ((*val as f64 / total) * width as f64).round().max(1.0) as usize
        }
        .min(max_here);
        used += seg_w;
        spans.push(Span::styled(glyph.repeat(seg_w), pal.fg(*color)));
    }
    Line::from(spans)
}

fn draw_models_pane(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &StatsTuiState,
    table_state: &mut TableState,
    view: &StatsView,
    pal: &Palette,
) {
    let active = state.active_pane == StatsPane::Models;
    let metric = state.query.metric;
    let title = format!(" 3. Models [{}] (v) ", state.query.model_scope.label());

    let models = state.active_models(view);
    if models.is_empty() {
        let empty = Paragraph::new(Line::styled("  No models recorded.", pal.dim()))
            .block(pal.pane_block(title, active));
        frame.render_widget(empty, area);
        return;
    }

    let inner_w = usize::from(area.width.saturating_sub(2));
    let inner_h = usize::from(area.height.saturating_sub(2));
    let show_hit = inner_w >= 34;
    let filter_glyph = if pal.ascii { "* " } else { "● " };

    let rows: Vec<Row<'static>> = models
        .iter()
        .map(|m| {
            let is_filtered = state.query.model_filter.as_deref() == Some(m.model.as_str());
            let val = metric.extract(m);
            let name = if is_filtered {
                format!("{filter_glyph}{}", m.model)
            } else {
                m.model.clone()
            };
            let name_style = if is_filtered { pal.bold(Color::Yellow) } else { Style::default() };
            let val_style = pal.bold(if is_filtered { Color::Yellow } else { Color::Cyan });
            let mut cells = vec![
                Cell::from(name).style(name_style),
                right_cell(metric.format_display(val, true), val_style),
            ];
            if show_hit {
                let hit_pct = m.cache_hit_pct();
                cells.push(right_cell(format_pct(hit_pct), pal.cache_hit_style(hit_pct)));
            }
            Row::new(cells)
        })
        .collect();

    let widths = if show_hit {
        vec![Constraint::Min(10), Constraint::Length(10), Constraint::Length(6)]
    } else {
        vec![Constraint::Min(10), Constraint::Length(10)]
    };

    let table = Table::new(rows, widths)
        .block(pal.pane_block(title, active))
        .column_spacing(1)
        .row_highlight_style(pal.selected_row(active))
        .highlight_symbol(if pal.ascii { "> " } else { "▸ " });

    // Clamp stale scroll offset before rendering so shrinking model lists never strand the viewport when unfocused.
    *table_state.offset_mut() = table_state.offset().min(state.model_idx);
    table_state.select(active.then_some(state.model_idx));
    frame.render_stateful_widget(table, area, table_state);

    if models.len() > inner_h && area.height > 2 {
        let mut sb = ScrollbarState::new(models.len()).position(state.model_idx);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None),
            Rect::new(area.x, area.y + 1, area.width, area.height.saturating_sub(2)),
            &mut sb,
        );
    }
}

fn draw_footer_bar(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &StatsTuiState,
    dataset: Option<&StatsDataset>,
    pal: &Palette,
) {
    let sep = format!(" {} ", pal.sep());
    let text = if let Some(msg) = &state.status_msg {
        format!(" {msg}")
    } else if let Some(model) = &state.query.model_filter {
        format!(" Filtered: {model}{sep}Esc Clear{sep}1-5 Metric{sep}t Range{sep}s Sort{sep}r Refresh{sep}q Quit")
    } else if area.width < 92 {
        format!(" 1-5 Metric{sep}t Range{sep}s Sort{sep}Tab Pane{sep}Enter Filter{sep}q Quit")
    } else {
        let src_hint = if dataset.is_some_and(|d| d.has_cli && d.has_desktop) {
            format!("{sep}f Src")
        } else {
            String::new()
        };
        format!(
            " 1-5 Metric{sep}t Range{sep}s Sort{sep}v Day/Win{sep}z Gaps{sep}g Heatmap{src_hint}{sep}Tab Pane{sep}Enter Filter{sep}r Refresh{sep}q Quit"
        )
    };
    let style = if state.status_msg.is_some() {
        pal.bold(Color::Yellow)
    } else {
        pal.dim()
    };
    frame.render_widget(Paragraph::new(Line::styled(text, style)), area);
}

#[cfg(test)]
mod tests {
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::stats::query::StatsDataset;
    use crate::stats::scan::{UsageEvent, UsageSource};
    use crate::stats::SqliteStats;
    use crate::timeutil::unix_day_from_civil_date;

    fn test_dataset() -> StatsDataset {
        let d1 = unix_day_from_civil_date("2026-11-19".parse().unwrap());
        let d2 = unix_day_from_civil_date("2026-11-20".parse().unwrap());
        StatsDataset {
            today_day_idx: d2,
            today_local: "2026-11-20".to_string(),
            timezone: "UTC+08:00".to_string(),
            today_utc: "2026-11-20".to_string(),
            first_active_date: Some("2026-11-19".to_string()),
            sqlite: SqliteStats::default(),
            has_cli: true,
            has_desktop: false,
            events: vec![
                UsageEvent {
                    source: UsageSource::Cli,
                    local_day: d1,
                    local_date: "2026-11-19".to_string(),
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
                    local_date: "2026-11-20".to_string(),
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
        }
    }

    fn render_buffer_text(width: u16, height: u16, metric: StatsMetric, ascii: bool) -> String {
        let ds = test_dataset();
        let mut st = StatsTuiState::new(None, metric);
        let mut tables = TableStates::default();
        st.loading = false;
        let view = ds.compute_view(&st.query, None);
        st.sync_with_view(&view);
        let pal = Palette {
            no_color: true,
            ascii,
        };
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
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
    fn tui_renders_wide_dashboard_with_daily_and_model_cache_rates() {
        let text = render_buffer_text(120, 36, StatsMetric::Cost, false);
        assert!(text.contains("agy-statusline stats"));
        assert!(text.contains("5:Turns"));
        assert!(text.contains("Activity (20w · Cost ($))"));
        assert!(text.contains("1. Daily Usage"));
        assert!(text.contains("Cache%"));
        assert!(text.contains("2026-11-20"));
        assert!(text.contains("2026-11-19"));
        // Per-day cache hit rates: 45K/50K = 90.0% (11-20), 80K/105K = 76.2% (11-19), total 125K/155K = 80.6%
        assert!(text.contains("90.0%"));
        assert!(text.contains("76.2%"));
        assert!(text.contains("80.6%"));
        assert!(text.contains("~$1.50"));
        assert!(text.contains("~$0.25"));
        assert!(text.contains("2. Day: 2026-11-20"));
        assert!(text.contains("Gemini 4 Hydrogen"));
    }

    #[test]
    fn tui_renders_medium_and_compact_80x24_with_daily_cache_rate_and_no_clipping() {
        // 96x30 exercises medium daily layout (inner_w = 57) with per-day Cache% alongside Cost.
        let med = render_buffer_text(96, 30, StatsMetric::Cost, true);
        assert!(med.contains("Cache%"));
        assert!(med.contains("90.0%"));
        assert!(med.contains("76.2%"));
        assert!(med.contains("80.6%"));

        // 80x24 exercises compact summary (with width-budgeted Hit% + Active) and full daily table.
        let text = render_buffer_text(80, 24, StatsMetric::InputOutput, true);
        assert!(text.contains("Summary (All | In+Out)"));
        assert!(text.contains("Hit 80.6%"));
        assert!(!text.contains("Active|"));
        assert!(text.contains("1. Daily Usage"));
        assert!(text.contains("Cache%"));
        assert!(text.contains("90.0%"));
        assert!(text.contains("76.2%"));
        assert!(text.contains("2026-11-20"));
        assert!(text.contains("2026-11-19"));
        assert!(text.contains("24K"));
        assert!(text.contains("6K"));
    }

    #[test]
    fn unfocused_models_offset_recovers_on_short_terminal() {
        let ds = test_dataset();
        let mut st = StatsTuiState::new(None, StatsMetric::Cost);
        st.loading = false;
        st.show_heatmap = Some(true);
        let view = ds.compute_view(&st.query, None);
        st.sync_with_view(&view);

        let mut tables = TableStates::default();
        *tables.models.offset_mut() = 10;
        let pal = Palette {
            no_color: true,
            ascii: true,
        };
        let backend = TestBackend::new(96, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                draw_stats_ui(frame, &st, &mut tables, Some(&ds), Some(&view), &pal);
            })
            .unwrap();

        assert_eq!(tables.models.offset(), 0);
    }
}
