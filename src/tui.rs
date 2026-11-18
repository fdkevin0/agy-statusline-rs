//! 3-pane `ratatui` + `crossterm` interactive TUI configurator with true-color live preview.

use std::collections::BTreeMap;
use std::io::{self, IsTerminal};
use std::path::Path;

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind, MouseButton, MouseEvent,
    MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use unicode_width::UnicodeWidthStr;

use crate::ansi::ansi_to_ratatui_lines;
use crate::apply::{apply_statusline_config, resolve_invoked_binary_path, default_settings_path};
use crate::db::SessionMetadata;
use crate::editor::{ActivePane, ExitOutcome, TuiEditorState};
use crate::git::GitInfo;
use crate::layout::{load_layout_config, save_layout_config, WidgetId, MAX_ROWS};
use crate::render::write_statusline;
use crate::statusline::{
    ContextWindow, CostInfo, JsonUsage, ModelInfo, QuotaBucket, SandboxInfo, StatuslinePayload,
    VcsInfo, VimInfo, WorkspaceInfo,
};

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PillHit {
    row: usize,
    col: usize,
    y: u16,
    x0: u16,
    x1: u16,
}

#[derive(Default)]
struct UiHitMap {
    preview: Rect,
    canvas: Rect,
    catalog: Rect,
    catalog_offset: usize,
    settings: Rect,
    settings_offset: usize,
    canvas_line_y: [u16; MAX_ROWS],
    canvas_pills: Vec<PillHit>,
}

/// Launches the 3-pane interactive TUI layout configurator.
pub fn run_tui_config(config_path: &Path) -> io::Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "interactive config requires a TTY (use --show, --reset, or --preset)",
        ));
    }

    let initial = load_layout_config(config_path);
    let mut state = TuiEditorState::new(initial);

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let _guard = TerminalGuard;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let mut outcome = ExitOutcome::Continue;
    let mut hit_map = UiHitMap::default();

    while outcome == ExitOutcome::Continue {
        terminal.draw(|frame| hit_map = draw_ui(frame, &state))?;

        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                outcome = state.handle_key(key.code, key.modifiers);
            }
            Event::Mouse(mouse) => handle_mouse(&mut state, mouse, &hit_map),
            _ => {}
        }
    }

    drop(terminal);
    drop(_guard);

    match outcome {
        ExitOutcome::SaveOnly => {
            save_layout_config(config_path, &state.config)?;
            println!("Saved layout config to {}", config_path.display());
        }
        ExitOutcome::SaveAndApply => {
            save_layout_config(config_path, &state.config)?;
            let settings_path = default_settings_path()?;
            let cmd = resolve_invoked_binary_path()?;
            apply_statusline_config(&settings_path, &cmd, state.config.stack_with_default)?;
            println!("Saved layout config to {}", config_path.display());
            println!(
                "Applied statusLine (stack_with_default={}) to {}",
                state.config.stack_with_default,
                settings_path.display()
            );
        }
        ExitOutcome::Quit | ExitOutcome::Continue => {
            println!("Cancelled without saving.");
        }
    }
    Ok(())
}

/// Builds a rich sample payload used for live preview and `doctor`.
pub fn sample_preview_payload(width: usize) -> StatuslinePayload {
    let mut quota = BTreeMap::new();
    quota.insert(
        "gemini-5h".to_string(),
        QuotaBucket {
            remaining_fraction: Some(0.84),
            reset_in_seconds: Some(11_400),
            ..Default::default()
        },
    );
    quota.insert(
        "gemini-weekly".to_string(),
        QuotaBucket {
            remaining_fraction: Some(0.92),
            reset_in_seconds: Some(432_000),
            ..Default::default()
        },
    );
    StatuslinePayload {
        agent_state: Some("thinking".to_string()),
        cycle_mode: Some("plan".to_string()),
        plan_tier: Some("Pro".to_string()),
        model: Some(ModelInfo {
            id: None,
            display_name: Some("Gemini 4".to_string()),
            effort: Some("high".to_string()),
        }),
        workspace: Some(WorkspaceInfo {
            current_dir: Some("/work/agy-statusline".to_string()),
            project_dir: Some("/work/agy-statusline".to_string()),
        }),
        vcs: Some(VcsInfo {
            vcs_type: Some("git".to_string()),
            branch: Some("main".to_string()),
            dirty: Some(true),
        }),
        sandbox: Some(SandboxInfo {
            enabled: Some(true),
            allow_network: None,
        }),
        vim: Some(VimInfo {
            mode: Some("NORMAL".to_string()),
        }),
        pending_input_count: Some(1),
        artifact_count: Some(2),
        subagents: Some(serde_json::json!([1, 2])),
        task_count: Some(3),
        context_window: Some(ContextWindow {
            used_percentage: Some(42.5),
            total_input_tokens: Some(48_000),
            total_output_tokens: Some(4_200),
            current_usage: Some(JsonUsage {
                input_tokens: Some(18_000),
                output_tokens: Some(50),
                cache_read_input_tokens: Some(200_000),
                cache_creation_input_tokens: Some(0),
            }),
            ..Default::default()
        }),
        cost: Some(CostInfo {
            total_usd: Some(0.14),
            subagent_usd: None,
            estimated: Some(true),
        }),
        quota: Some(quota),
        terminal_width: Some(width),
        ..Default::default()
    }
}

fn build_preview_lines(state: &TuiEditorState, box_inner: u16) -> (Vec<Line<'static>>, usize) {
    let width = state.preview_width.resolve(box_inner);
    let sample = sample_preview_payload(width);
    let meta = SessionMetadata {
        speed: Some(64.0),
        median_speed: Some(68.0),
        last_turn_speed: Some(72.0),
        e2e_speed: Some(46.0),
        ttft_secs: Some(1.4),
        thinking_tokens: Some(4_200),
        max_context_tokens: Some(512_000),
        ..Default::default()
    };
    let git = GitInfo {
        branch: Some("main".to_string()),
        worktree: Some("feat-wt".to_string()),
        added: 42,
        deleted: 11,
    };
    let mut buf = Vec::new();
    let _ = write_statusline(&sample, &meta, &git, &state.config, &mut buf);
    let raw = String::from_utf8_lossy(&buf);
    (ansi_to_ratatui_lines(&raw), width)
}

fn pane_block(title: String, active: bool, grabbed: bool) -> Block<'static> {
    let border_color = if active && grabbed {
        Color::Yellow
    } else if active {
        Color::Cyan
    } else {
        Color::DarkGray
    };
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .title(Span::styled(
            title,
            Style::default()
                .fg(if active { border_color } else { Color::Gray })
                .add_modifier(Modifier::BOLD),
        ))
}

fn in_rect(rect: Rect, col: u16, row: u16) -> bool {
    col >= rect.x && col < rect.x + rect.width && row >= rect.y && row < rect.y + rect.height
}

fn handle_mouse(state: &mut TuiEditorState, mouse: MouseEvent, map: &UiHitMap) {
    let (col, row) = (mouse.column, mouse.row);
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            if in_rect(map.preview, col, row) {
                state.preview_width = state.preview_width.next();
            } else if in_rect(map.canvas, col, row) {
                if let Some(hit) = map
                    .canvas_pills
                    .iter()
                    .find(|p| row == p.y && col >= p.x0 && col < p.x1)
                {
                    state.click_canvas(hit.row, Some(hit.col));
                } else if let Some((r, _)) = map
                    .canvas_line_y
                    .iter()
                    .enumerate()
                    .find(|&(_, &y)| y == row)
                {
                    state.click_canvas(r, None);
                } else {
                    state.active_pane = ActivePane::Canvas;
                }
            } else if in_rect(map.catalog, col, row) {
                let inner_y = row.saturating_sub(map.catalog.y + 1) as usize;
                if row > map.catalog.y && row + 1 < map.catalog.y + map.catalog.height {
                    state.click_catalog(map.catalog_offset + inner_y);
                } else {
                    state.active_pane = ActivePane::Catalog;
                }
            } else if in_rect(map.settings, col, row) {
                let inner_y = row.saturating_sub(map.settings.y + 1) as usize;
                if row > map.settings.y && row + 1 < map.settings.y + map.settings.height {
                    state.adjust_setting_at(map.settings_offset + inner_y, 1);
                    state.active_pane = ActivePane::Settings;
                } else {
                    state.active_pane = ActivePane::Settings;
                }
            }
        }
        MouseEventKind::Down(MouseButton::Right) => {
            if in_rect(map.canvas, col, row) {
                if let Some(hit) = map
                    .canvas_pills
                    .iter()
                    .find(|p| row == p.y && col >= p.x0 && col < p.x1)
                {
                    state.remove_canvas_pill(hit.row, hit.col);
                }
            } else if in_rect(map.settings, col, row) {
                let inner_y = row.saturating_sub(map.settings.y + 1) as usize;
                if row > map.settings.y && row + 1 < map.settings.y + map.settings.height {
                    state.adjust_setting_at(map.settings_offset + inner_y, -1);
                    state.active_pane = ActivePane::Settings;
                }
            }
        }
        MouseEventKind::ScrollUp => {
            if in_rect(map.catalog, col, row) {
                state.scroll_catalog(-1);
            } else if in_rect(map.settings, col, row) {
                state.scroll_settings(-1);
            } else if in_rect(map.canvas, col, row) {
                state.click_canvas(state.canvas_row.saturating_sub(1), None);
            }
        }
        MouseEventKind::ScrollDown => {
            if in_rect(map.catalog, col, row) {
                state.scroll_catalog(1);
            } else if in_rect(map.settings, col, row) {
                state.scroll_settings(1);
            } else if in_rect(map.canvas, col, row) {
                state.click_canvas((state.canvas_row + 1).min(MAX_ROWS - 1), None);
            }
        }
        _ => {}
    }
}

fn draw_ui(frame: &mut Frame<'_>, state: &TuiEditorState) -> UiHitMap {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(6),
            Constraint::Min(10),
            Constraint::Length(3),
        ])
        .split(area);

    let (preview_lines, eff_w) = build_preview_lines(state, chunks[0].width.saturating_sub(4));
    let agy_mode = if state.config.stack_with_default {
        "Stack"
    } else {
        "Replace"
    };
    let preview_title = format!(
        " Live Preview [{} ({}col) · {} · {} · {} · AGY: {}] ",
        state.preview_width.label(),
        eff_w,
        state.config.style.name(),
        state.config.theme.name(),
        state.config.glyph_mode.name(),
        agy_mode
    );
    let preview = Paragraph::new(preview_lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(Color::DarkGray))
                .title(Span::styled(
                    preview_title,
                    Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                )),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(preview, chunks[0]);

    let mid = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(46),
            Constraint::Percentage(27),
            Constraint::Percentage(27),
        ])
        .split(chunks[1]);

    let help_text = if state.grabbed {
        "MOVE MODE:  ←/→ Swap on line  ·  ↑/↓ Move between Line 1–3  ·  Enter/Esc Drop"
    } else {
        "Tab/Click Pane  ·  ↑↓←→/Scroll Nav  ·  Enter/Click Grab/Toggle  ·  RightClick Remove  ·  1-4 Preset  ·  w Width  ·  s Save+Apply  ·  q Quit"
    };
    let help = Paragraph::new(help_text)
        .style(Style::default().fg(if state.grabbed { Color::Yellow } else { Color::Gray }))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(Color::DarkGray)),
        );
    frame.render_widget(help, chunks[2]);

    let (canvas_line_y, canvas_pills) = draw_canvas_pane(frame, mid[0], state);
    let catalog_offset = draw_catalog_pane(frame, mid[1], state);
    let settings_offset = draw_settings_pane(frame, mid[2], state);
    UiHitMap {
        preview: chunks[0],
        canvas: mid[0],
        catalog: mid[1],
        catalog_offset,
        settings: mid[2],
        settings_offset,
        canvas_line_y,
        canvas_pills,
    }
}

fn layout_canvas_lines(
    area: Rect,
    state: &TuiEditorState,
) -> (Vec<Line<'static>>, [u16; MAX_ROWS], Vec<PillHit>) {
    let active = state.active_pane == ActivePane::Canvas;
    let mut lines = Vec::new();
    let mut line_y = [0u16; MAX_ROWS];
    let mut pills = Vec::new();
    let inner_x = area.x + 1;
    let max_x = area.x + area.width.saturating_sub(1);
    let max_y = area.y + area.height.saturating_sub(1);
    let mut cur_y = area.y + 1;

    for r in 0..MAX_ROWS {
        if cur_y < max_y {
            line_y[r] = cur_y;
        }
        let row_focused = active && state.canvas_row == r;
        let label_style = if row_focused {
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let mut spans = vec![Span::styled(format!("Line {}  ", r + 1), label_style)];
        let mut cur_x = inner_x + 8;
        let items = &state.config.rows[r];
        if items.is_empty() {
            spans.push(Span::styled(
                if row_focused {
                    "(empty — press Enter/Space to add)"
                } else {
                    "(empty)"
                },
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            ));
            lines.push(Line::from(spans));
            cur_y += 1;
        } else {
            let mut line_has_pill = false;
            for (c, w) in items.iter().enumerate() {
                let is_cur = row_focused && c == state.canvas_col;
                let (text, style) = if is_cur && state.grabbed {
                    (
                        format!(" ⇄ {} ", w.label()),
                        Style::default()
                            .bg(Color::Yellow)
                            .fg(Color::Black)
                            .add_modifier(Modifier::BOLD),
                    )
                } else if is_cur {
                    (
                        format!(" {} ", w.label()),
                        Style::default()
                            .bg(Color::Cyan)
                            .fg(Color::Black)
                            .add_modifier(Modifier::BOLD),
                    )
                } else {
                    (
                        format!(" {} ", w.label()),
                        Style::default().bg(Color::DarkGray).fg(Color::White),
                    )
                };
                let pw = text.width() as u16;
                if line_has_pill && cur_x + pw > max_x {
                    lines.push(Line::from(spans));
                    cur_y += 1;
                    spans = vec![Span::raw("        ")];
                    cur_x = inner_x + 8;
                }
                if cur_y < max_y {
                    pills.push(PillHit {
                        row: r,
                        col: c,
                        y: cur_y,
                        x0: cur_x,
                        x1: cur_x + pw,
                    });
                }
                cur_x += pw + 1;
                spans.push(Span::styled(text, style));
                spans.push(Span::raw(" "));
                line_has_pill = true;
            }
            lines.push(Line::from(spans));
            cur_y += 1;
        }
        lines.push(Line::raw(""));
        cur_y += 1;
    }

    let hint = if state.grabbed {
        "Moving widget: use arrows to place, Enter to drop."
    } else {
        "Tip: Click pill to select/grab, Right-click to remove."
    };
    lines.push(Line::styled(hint, Style::default().fg(Color::DarkGray)));

    (lines, line_y, pills)
}

fn draw_canvas_pane(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &TuiEditorState,
) -> ([u16; MAX_ROWS], Vec<PillHit>) {
    let active = state.active_pane == ActivePane::Canvas;
    let (lines, line_y, pills) = layout_canvas_lines(area, state);
    let panel = Paragraph::new(lines)
        .block(pane_block(" 1. Layout Lines ".to_string(), active, state.grabbed));
    frame.render_widget(panel, area);
    (line_y, pills)
}

fn draw_catalog_pane(frame: &mut Frame<'_>, area: Rect, state: &TuiEditorState) -> usize {
    let active = state.active_pane == ActivePane::Catalog;
    let inner_h = usize::from(area.height.saturating_sub(2)).max(1);
    let offset = state
        .catalog_idx
        .saturating_sub(inner_h.saturating_sub(1));
    let mut lines = Vec::new();
    for (idx, &w) in WidgetId::ALL.iter().enumerate().skip(offset).take(inner_h) {
        let placed = if w == WidgetId::Spacer {
            if state.config.rows[state.canvas_row].contains(&WidgetId::Spacer) {
                Some((state.canvas_row, 0))
            } else {
                state.config.locate_widget(w)
            }
        } else {
            state.config.locate_widget(w)
        };
        let badge = match placed {
            Some((r, _)) => format!("[✓ L{}]", r + 1),
            None => "[    ]".to_string(),
        };
        let is_cur = active && idx == state.catalog_idx;
        let line_style = if is_cur {
            Style::default()
                .bg(Color::Cyan)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD)
        } else if placed.is_some() {
            Style::default().fg(Color::White)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        lines.push(Line::styled(
            format!("{badge} {}", w.display_name()),
            line_style,
        ));
    }
    let title = format!(" 2. Catalog (→ L{}) ", state.canvas_row + 1);
    let panel = Paragraph::new(lines).block(pane_block(title, active, false));
    frame.render_widget(panel, area);
    offset
}

fn draw_settings_pane(frame: &mut Frame<'_>, area: Rect, state: &TuiEditorState) -> usize {
    let active = state.active_pane == ActivePane::Settings;
    let cfg = &state.config;
    let base_preset = match state.preset_idx {
        0 => "Full Dashboard",
        1 => "Powerline Pro",
        2 => "Minimal 1-Line",
        _ => "Metrics Addon",
    };
    let preset_name = if state.is_custom_preset() {
        format!("{base_preset}*")
    } else {
        base_preset.to_string()
    };
    let rows = [
        ("Preset", preset_name),
        ("Style", cfg.style.name().to_string()),
        ("Theme", cfg.theme.name().to_string()),
        ("Separator", cfg.separator.name().to_string()),
        ("Speed mode", cfg.speed_mode.name().to_string()),
        (
            "Ctx mode",
            if cfg.ctx_remaining { "Remaining %" } else { "Used %" }.to_string(),
        ),
        ("Smart hide", if cfg.smart_hide { "ON" } else { "OFF" }.to_string()),
        ("Git diff", if cfg.show_git_diff { "ON" } else { "OFF" }.to_string()),
        ("Bar width", format!("{} cols", cfg.bar_length)),
        (
            "AGY mode",
            if cfg.stack_with_default {
                "Stack Below"
            } else {
                "Replace Default"
            }
            .to_string(),
        ),
        ("Glyphs", cfg.glyph_mode.name().to_string()),
        ("Responsive", if cfg.responsive { "ON" } else { "OFF" }.to_string()),
        ("Speed warn", format!("{:.0} tok/s", cfg.speed_warn_threshold)),
    ];

    let inner_h = usize::from(area.height.saturating_sub(2)).max(1);
    let offset = state.settings_idx.saturating_sub(inner_h.saturating_sub(1));
    let lines: Vec<Line<'static>> = rows
        .iter()
        .enumerate()
        .skip(offset)
        .take(inner_h)
        .map(|(idx, (k, v))| {
            let is_cur = active && idx == state.settings_idx;
            if is_cur {
                Line::styled(
                    format!("▸ {k:<10} < {v} >"),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Line::from(vec![
                    Span::styled(format!("  {k:<10} "), Style::default().fg(Color::Gray)),
                    Span::styled(v.clone(), Style::default().fg(Color::Yellow)),
                ])
            }
        })
        .collect();

    let panel = Paragraph::new(lines).block(pane_block(" 3. Style & AGY ".to_string(), active, false));
    frame.render_widget(panel, area);
    offset
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::LayoutConfig;

    #[test]
    fn canvas_wrapping_updates_line_y_and_pill_hit_coordinates() {
        let mut cfg = LayoutConfig::preset_full();
        cfg.rows[1] = vec![WidgetId::Model];
        let state = TuiEditorState::new(cfg);
        let area = Rect::new(0, 0, 48, 18);
        let (_lines, line_y, pills) = layout_canvas_lines(area, &state);

        let last_row0 = pills.iter().rfind(|p| p.row == 0).unwrap();
        let row1_pill = pills.iter().find(|p| p.row == 1).unwrap();
        assert!(last_row0.y > line_y[0]);
        assert_eq!(line_y[1], last_row0.y + 2);
        assert_eq!(row1_pill.y, line_y[1]);
    }

    #[test]
    fn settings_pane_and_preview_reflect_median_speed_mode() {
        let mut cfg = LayoutConfig::preset_minimal();
        cfg.speed_mode = crate::layout::SpeedMode::Median;
        let state = TuiEditorState::new(cfg);

        let (preview_lines, _) = build_preview_lines(&state, 80);
        let preview_text: String = preview_lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(preview_text.contains("68 tok/s"));

        let backend = ratatui::backend::TestBackend::new(110, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                draw_settings_pane(frame, frame.area(), &state);
            })
            .unwrap();
        let buffer_text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(buffer_text.contains("Median"));
    }
}
