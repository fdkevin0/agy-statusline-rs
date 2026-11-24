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

use crossterm::event::{self, EnableMouseCapture, Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{enable_raw_mode, EnterAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Scrollbar, ScrollbarOrientation, ScrollbarState, Table,
    TableState,
};
use ratatui::{Frame, Terminal};

use super::metric::StatsMetric;
use super::pricing::{
    fetch_models_dev_via_curl, PricingCatalog, SyncMode, DEFAULT_PRICING_TTL_SECS,
};
use super::scan::{collect_conversation_stats_report, list_conversations};
use super::tz::TimeZoneSpec;
use super::PeriodSummary;
use crate::config::layout::save_stats_metric;
use crate::statusline::format::format_tokens_string as fmt_tok;
use crate::sys::terminal::TerminalGuard;

use dashboard::{DashboardAction, DashboardScreen};
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

struct StatsApp {
    roots: Vec<(&'static str, PathBuf)>,
    cache_dir: Option<PathBuf>,
    tz: TimeZoneSpec,
    timezone: String,
    default_metric: StatsMetric,
    pricing: Option<PricingCatalog>,
    screen: Screen,
    prev_stack: Vec<Screen>,
    dashboard: Option<DashboardScreen>,
    picker: Option<PickerScreen>,
    detail: Option<DetailScreen>,
}

impl StatsApp {
    fn pricing_mut(&mut self) -> &PricingCatalog {
        let cache = self.cache_dir.as_deref();
        self.pricing.get_or_insert_with(|| {
            PricingCatalog::load_effective_with(
                cache,
                SyncMode::BlockingIfStale,
                crate::sys::time::unix_now_secs(),
                DEFAULT_PRICING_TTL_SECS,
                fetch_models_dev_via_curl,
            )
        })
    }

    fn active_metric(&self) -> StatsMetric {
        self.dashboard
            .as_ref()
            .map_or(self.default_metric, |d| d.state.query.metric)
    }

    fn open_conversations(&mut self) {
        let active_id = self
            .detail
            .as_ref()
            .map(|d| d.report.conversation_id.clone());
        if self.prev_stack.last() == Some(&Screen::Conversations) {
            self.prev_stack.pop();
            if let Some(p) = self.picker.as_mut() {
                p.set_active_id(active_id.as_deref());
            }
            self.screen = Screen::Conversations;
            return;
        }
        let items = list_conversations(&self.roots, &self.tz);
        match self.picker.as_mut() {
            Some(p) => p.replace_items(items, active_id.as_deref()),
            None => self.picker = Some(PickerScreen::new(items, active_id.as_deref(), None, false)),
        }
        self.prev_stack.push(self.screen);
        self.screen = Screen::Conversations;
    }

    fn open_conversation(&mut self, conv_id: &str) {
        let start = Instant::now();
        let now = crate::sys::time::unix_now_secs();
        let metric = self.active_metric();
        let roots = self.roots.clone();
        let tz = self.tz.clone();
        let pricing = self.pricing_mut();
        match collect_conversation_stats_report(&roots, conv_id, now, &tz, pricing, metric) {
            Ok(report) => {
                let short: String = report.conversation_id.chars().take(8).collect();
                let turns = report.turns.len();
                if let Some(p) = self.picker.as_mut() {
                    p.set_active_id(Some(&report.conversation_id));
                }
                let mut detail = DetailScreen::new(report);
                detail.status_msg = Some(format!(
                    "Loaded {short} ({turns} turns) in {:.0}ms",
                    start.elapsed().as_secs_f64() * 1_000.0
                ));
                self.detail = Some(detail);
                if self.prev_stack.last() == Some(&Screen::Detail) {
                    self.prev_stack.pop();
                } else if self.screen != Screen::Detail {
                    self.prev_stack.push(self.screen);
                }
                self.screen = Screen::Detail;
            }
            Err(err) => {
                if let Some(p) = self.picker.as_mut() {
                    p.status_msg = Some(format!("Failed to load conversation: {err}"));
                }
            }
        }
    }

    fn pop_or_quit(&mut self) -> bool {
        if let Some(prev) = self.prev_stack.pop() {
            if prev == Screen::Conversations {
                if let (Some(p), Some(d)) = (self.picker.as_mut(), self.detail.as_ref()) {
                    p.set_active_id(Some(&d.report.conversation_id));
                }
            }
            self.screen = prev;
            false
        } else {
            true
        }
    }

    fn run(mut self, config_path: Option<&Path>, pal: &Palette) -> io::Result<()> {
        let initial_metric = self.default_metric;
        let _guard = TerminalGuard::enter();
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;

        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        loop {
            let scanning = self
                .dashboard
                .as_mut()
                .is_some_and(DashboardScreen::poll_scan);
            let has_back = !self.prev_stack.is_empty();

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

            // Poll only while a background scan is active; block at 0% CPU when idle.
            if scanning && !event::poll(Duration::from_millis(50))? {
                continue;
            }

            let evt = event::read()?;
            match self.screen {
                Screen::Dashboard => {
                    let term_h = terminal.size().map_or(30, |s| s.height);
                    let action = match evt {
                        Event::Key(key) if key.kind == KeyEventKind::Press => {
                            self.dashboard.as_mut().map_or(DashboardAction::Quit, |d| {
                                d.handle_key(key.code, key.modifiers, term_h)
                            })
                        }
                        Event::Mouse(mouse) => {
                            if let Some(d) = self.dashboard.as_mut() {
                                d.handle_mouse(mouse);
                            }
                            DashboardAction::Continue
                        }
                        _ => DashboardAction::Continue,
                    };
                    match action {
                        DashboardAction::Quit => break,
                        DashboardAction::OpenConversations => self.open_conversations(),
                        DashboardAction::Refresh => {
                            if let Some(d) = self.dashboard.as_mut() {
                                d.trigger_refresh(&self.roots, self.cache_dir.as_deref());
                            }
                        }
                        DashboardAction::Continue => {}
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
                                p.status_msg = Some(format!(
                                    "Reloaded {count} conversations in {:.0}ms",
                                    start.elapsed().as_secs_f64() * 1_000.0
                                ));
                            }
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
                                .map(|d| d.report.conversation_id.clone())
                            {
                                let start = Instant::now();
                                let now = crate::sys::time::unix_now_secs();
                                let metric = self.active_metric();
                                let roots = self.roots.clone();
                                let tz = self.tz.clone();
                                let pricing = self.pricing_mut();
                                match collect_conversation_stats_report(
                                    &roots, &conv_id, now, &tz, pricing, metric,
                                ) {
                                    Ok(updated) => {
                                        let turns = updated.turns.len();
                                        if let Some(d) = self.detail.as_mut() {
                                            d.replace_report(updated);
                                            d.status_msg = Some(format!(
                                                "Refreshed {turns} turns in {:.0}ms",
                                                start.elapsed().as_secs_f64() * 1_000.0
                                            ));
                                        }
                                    }
                                    Err(err) => {
                                        if let Some(d) = self.detail.as_mut() {
                                            d.status_msg = Some(format!("Refresh failed: {err}"));
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

        let final_metric = self.active_metric();
        if final_metric != initial_metric {
            if let Some(cfg_path) = config_path {
                let _ = save_stats_metric(cfg_path, final_metric);
            }
        }
        Ok(())
    }
}

fn require_tty(msg: &'static str) -> io::Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::new(io::ErrorKind::Unsupported, msg));
    }
    Ok(())
}

/// Launches the interactive `agy-statusline stats` TUI dashboard.
pub fn run_stats_tui(
    roots: Vec<(&'static str, PathBuf)>,
    config_path: Option<&Path>,
    opts: &super::StatsOptions<'_>,
) -> io::Result<()> {
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
        default_metric: opts.metric,
        pricing: None,
        screen: Screen::Dashboard,
        prev_stack: Vec::new(),
        dashboard: Some(dashboard),
        picker: None,
        detail: None,
    }
    .run(config_path, &pal)
}

/// Launches the interactive single-conversation stats TUI (`agy-statusline stats <CONVERSATION_ID>`).
pub fn run_conversation_stats_tui(
    roots: &[(&'static str, PathBuf)],
    conversation_query: &str,
    opts: &super::StatsOptions<'_>,
) -> io::Result<()> {
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
    let pal = Palette {
        no_color: opts.no_color,
        ascii: opts.ascii,
    };
    StatsApp {
        roots: roots.to_vec(),
        cache_dir: opts.cache_dir.map(Path::to_path_buf),
        tz,
        timezone,
        default_metric: opts.metric,
        pricing: Some(pricing),
        screen: Screen::Detail,
        prev_stack: Vec::new(),
        dashboard: None,
        picker: None,
        detail: Some(DetailScreen::new(report)),
    }
    .run(None, &pal)
}

/// Launches the full-screen historical conversation picker TUI (`agy-statusline conversations`).
pub fn run_conversations_tui(
    roots: &[(&'static str, PathBuf)],
    initial_query: Option<&str>,
    show_subagents: bool,
    opts: &super::StatsOptions<'_>,
) -> io::Result<()> {
    require_tty("interactive conversations requires a TTY (use --plain or --json)")?;
    let tz = TimeZoneSpec::local();
    let timezone = tz.format_offset_at(opts.now_unix_secs as i64);
    let items = list_conversations(roots, &tz);
    let pal = Palette {
        no_color: opts.no_color,
        ascii: opts.ascii,
    };
    StatsApp {
        roots: roots.to_vec(),
        cache_dir: opts.cache_dir.map(Path::to_path_buf),
        tz,
        timezone,
        default_metric: opts.metric,
        pricing: None,
        screen: Screen::Conversations,
        prev_stack: Vec::new(),
        dashboard: None,
        picker: Some(PickerScreen::new(
            items,
            None,
            initial_query,
            show_subagents,
        )),
        detail: None,
    }
    .run(None, &pal)
}
