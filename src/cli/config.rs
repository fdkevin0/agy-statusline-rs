//! 3-pane `ratatui` + `crossterm` interactive TUI configurator with true-color live preview.

mod editor;

use std::io::{self, IsTerminal};
use std::path::Path;

use crossterm::event::{self, Event, KeyEventKind, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::agy::paths::agy_settings_path;
use crate::agy::settings::{apply_statusline_config, resolve_invoked_binary_path};
use crate::config::layout::{load_layout_config, save_layout_config, WidgetId, MAX_ROWS};
use crate::statusline::render::render_statusline_lines;
use crate::statusline::{sample_preview_git, sample_preview_metadata, sample_preview_payload};
use crate::sys::terminal::enter_tui;
use editor::{ActivePane, ExitOutcome, PreviewWidth, SettingField, TuiEditorState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PillHit {
    row: usize,
    col: usize,
    y: u16,
    x0: u16,
    x1: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigFooterClick {
    NextPane,
    CyclePreset,
    CycleWidth,
    SaveAndApply,
    Quit,
    DropGrabbed,
}

#[derive(Default)]
struct UiHitMap {
    preview: Rect,
    canvas: Rect,
    catalog: Rect,
    catalog_inner: Rect,
    catalog_offset: usize,
    settings: Rect,
    settings_inner: Rect,
    settings_offset: usize,
    settings_dec_pos: Option<Position>,
    canvas_row_ys: Vec<(u16, usize)>,
    canvas_pills: Vec<PillHit>,
    footer_hits: Vec<(Rect, ConfigFooterClick)>,
}

/// Launches the 3-pane interactive TUI layout configurator.
pub fn run_tui_config(config_path: &Path) -> io::Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "interactive config requires a TTY (use --show, --reset, or --preset)",
        ));
    }

    // An upgrade can replace the running executable while the editor is open.
    let launcher = resolve_invoked_binary_path();
    let initial = load_layout_config(config_path);
    let mut state = TuiEditorState::new(initial);

    let (_guard, mut terminal) = enter_tui()?;
    let mut outcome = ExitOutcome::Continue;
    let mut hit_map = UiHitMap::default();

    while outcome == ExitOutcome::Continue {
        terminal.draw(|frame| hit_map = draw_ui(frame, &state))?;

        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                outcome = state.handle_key(key.code, key.modifiers);
            }
            Event::Mouse(mouse) => outcome = handle_mouse(&mut state, mouse, &hit_map),
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
            let settings_path = agy_settings_path()?;
            apply_statusline_config(&settings_path, &launcher?, state.config.stack_with_default)?;
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

fn build_preview_lines(state: &TuiEditorState, box_inner: u16) -> (Vec<Line<'static>>, usize) {
    let width = state.preview_width.resolve(box_inner);
    let sample = sample_preview_payload(width);
    let meta = sample_preview_metadata();
    let git = sample_preview_git();
    let lines = render_statusline_lines(&sample, &meta, &git, &state.config);
    (lines, width)
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

fn handle_mouse(state: &mut TuiEditorState, mouse: MouseEvent, map: &UiHitMap) -> ExitOutcome {
    let (col, row) = (mouse.column, mouse.row);
    let pos = Position::new(col, row);
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            if let Some(&(_, action)) = map.footer_hits.iter().find(|(r, _)| r.contains(pos)) {
                match action {
                    ConfigFooterClick::NextPane => state.focus_pane(state.active_pane.next()),
                    ConfigFooterClick::CyclePreset => {
                        state.apply_preset(state.preset.step(1));
                    }
                    ConfigFooterClick::CycleWidth => {
                        state.preview_width = state.preview_width.next();
                    }
                    ConfigFooterClick::SaveAndApply => return ExitOutcome::SaveAndApply,
                    ConfigFooterClick::Quit => return ExitOutcome::Quit,
                    ConfigFooterClick::DropGrabbed => state.grabbed = false,
                }
                return ExitOutcome::Continue;
            }
            if map.preview.contains(pos) {
                state.preview_width = state.preview_width.next();
            } else if map.canvas.contains(pos) {
                if let Some(hit) = map
                    .canvas_pills
                    .iter()
                    .find(|p| row == p.y && col >= p.x0 && col < p.x1)
                {
                    state.click_canvas(hit.row, Some(hit.col));
                } else if let Some(&(_, r)) = map.canvas_row_ys.iter().find(|&&(y, _)| y == row) {
                    state.click_canvas(r, None);
                } else {
                    state.focus_pane(ActivePane::Canvas);
                }
            } else if map.catalog.contains(pos) {
                if map.catalog_inner.contains(pos) {
                    let inner_y = usize::from(row.saturating_sub(map.catalog_inner.y));
                    state.click_catalog(map.catalog_offset + inner_y);
                } else {
                    state.focus_pane(ActivePane::Catalog);
                }
            } else if map.settings.contains(pos) {
                if map.settings_inner.contains(pos) {
                    let inner_y = usize::from(row.saturating_sub(map.settings_inner.y));
                    let dir = if map.settings_dec_pos == Some(pos) {
                        -1
                    } else {
                        1
                    };
                    state.adjust_setting_at(map.settings_offset + inner_y, dir);
                } else {
                    state.focus_pane(ActivePane::Settings);
                }
            }
        }
        MouseEventKind::Down(MouseButton::Right) => {
            if map.canvas.contains(pos) {
                if let Some(hit) = map
                    .canvas_pills
                    .iter()
                    .find(|p| row == p.y && col >= p.x0 && col < p.x1)
                {
                    state.remove_canvas_pill(hit.row, hit.col);
                }
            } else if map.settings_inner.contains(pos) {
                let inner_y = usize::from(row.saturating_sub(map.settings_inner.y));
                state.adjust_setting_at(map.settings_offset + inner_y, -1);
            }
        }
        MouseEventKind::ScrollUp => {
            if map.catalog.contains(pos) {
                state.scroll_catalog(-1);
            } else if map.settings.contains(pos) {
                state.scroll_settings(-1);
            } else if map.canvas.contains(pos) {
                state.scroll_canvas(-1);
            }
        }
        MouseEventKind::ScrollDown => {
            if map.catalog.contains(pos) {
                state.scroll_catalog(1);
            } else if map.settings.contains(pos) {
                state.scroll_settings(1);
            } else if map.canvas.contains(pos) {
                state.scroll_canvas(1);
            }
        }
        _ => {}
    }
    ExitOutcome::Continue
}

fn draw_footer(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &TuiEditorState,
) -> Vec<(Rect, ConfigFooterClick)> {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    let style = Style::default().fg(if state.grabbed {
        Color::Yellow
    } else {
        Color::Gray
    });
    let segments: &[(&str, Option<ConfigFooterClick>)] = if state.grabbed {
        &[
            (
                "MOVE MODE:  ←/→ Swap on line  ·  ↑/↓ Move between Line 1–3  ·  ",
                None,
            ),
            ("Enter/Esc Drop", Some(ConfigFooterClick::DropGrabbed)),
        ]
    } else {
        &[
            ("Tab/Click Pane", Some(ConfigFooterClick::NextPane)),
            (
                "  ·  ↑↓←→/Scroll Nav  ·  Enter/Click Grab/Toggle  ·  RightClick Remove  ·  ",
                None,
            ),
            ("1-4 Preset", Some(ConfigFooterClick::CyclePreset)),
            ("  ·  ", None),
            ("w Width", Some(ConfigFooterClick::CycleWidth)),
            ("  ·  ", None),
            ("s Save+Apply", Some(ConfigFooterClick::SaveAndApply)),
            ("  ·  ", None),
            ("q Quit", Some(ConfigFooterClick::Quit)),
        ]
    };

    let mut spans = Vec::with_capacity(segments.len());
    let mut hits = Vec::new();
    let mut cur_x = inner.x;
    for &(text, action) in segments {
        let w = u16::try_from(text.width()).unwrap_or(u16::MAX);
        if let Some(act) = action {
            if inner.height > 0 && cur_x < inner.right() {
                let clipped_w = w.min(inner.right().saturating_sub(cur_x));
                if clipped_w > 0 {
                    hits.push((Rect::new(cur_x, inner.y, clipped_w, 1), act));
                }
            }
        }
        spans.push(Span::styled(text, style));
        cur_x = cur_x.saturating_add(w);
    }

    frame.render_widget(Paragraph::new(Line::from(spans)).block(block), area);
    hits
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
    let preview_title = match state.preview_width {
        PreviewWidth::Auto => format!(" Live Preview [Auto · {eff_w}col] "),
        _ => format!(" Live Preview [{}] ", state.preview_width.label()),
    };
    let preview = Paragraph::new(preview_lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(Color::DarkGray))
                .title(Span::styled(
                    preview_title,
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
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

    let footer_hits = draw_footer(frame, chunks[2], state);
    let (canvas_row_ys, canvas_pills) = draw_canvas_pane(frame, mid[0], state);
    let (catalog_inner, catalog_offset) = draw_catalog_pane(frame, mid[1], state);
    let (settings_inner, settings_offset, settings_dec_pos) =
        draw_settings_pane(frame, mid[2], state);
    UiHitMap {
        preview: chunks[0],
        canvas: mid[0],
        catalog: mid[1],
        catalog_inner,
        catalog_offset,
        settings: mid[2],
        settings_inner,
        settings_offset,
        settings_dec_pos,
        canvas_row_ys,
        canvas_pills,
        footer_hits,
    }
}

struct CanvasLayout {
    lines: Vec<Line<'static>>,
    row_ys: Vec<(u16, usize)>,
    pills: Vec<PillHit>,
}

fn layout_canvas_lines(area: Rect, state: &TuiEditorState) -> CanvasLayout {
    let active = state.active_pane == ActivePane::Canvas;
    let mut lines = Vec::new();
    let mut row_ys = Vec::new();
    let mut pills = Vec::new();
    let inner_x = area.x + 1;
    let max_x = area.x + area.width.saturating_sub(1);
    let max_y = area.y + area.height.saturating_sub(1);
    let mut cur_y = area.y + 1;

    for r in 0..MAX_ROWS {
        if cur_y < max_y {
            row_ys.push((cur_y, r));
        }
        let row_focused = active && state.canvas_row == r;
        let label_style = if row_focused {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let mut spans = vec![Span::styled(format!("Line {}  ", r + 1), label_style)];
        let mut cur_x = inner_x + 8;
        let items = &state.config.rows[r];
        if items.is_empty() {
            spans.push(Span::styled(
                "(empty)",
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
                    if cur_y < max_y {
                        row_ys.push((cur_y, r));
                    }
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

    CanvasLayout {
        lines,
        row_ys,
        pills,
    }
}

fn draw_canvas_pane(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &TuiEditorState,
) -> (Vec<(u16, usize)>, Vec<PillHit>) {
    let active = state.active_pane == ActivePane::Canvas;
    let layout = layout_canvas_lines(area, state);
    let panel = Paragraph::new(layout.lines).block(pane_block(
        " 1. Layout Lines ".to_string(),
        active,
        state.grabbed,
    ));
    frame.render_widget(panel, area);
    (layout.row_ys, layout.pills)
}

fn draw_catalog_pane(frame: &mut Frame<'_>, area: Rect, state: &TuiEditorState) -> (Rect, usize) {
    let active = state.active_pane == ActivePane::Catalog;
    let title = format!(" 2. Catalog (→ L{}) ", state.canvas_row + 1);
    let block = pane_block(title, active, false);
    let inner = block.inner(area);
    let inner_h = usize::from(inner.height).max(1);
    let offset = state.catalog_idx.saturating_sub(inner_h.saturating_sub(1));
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
    let panel = Paragraph::new(lines).block(block);
    frame.render_widget(panel, area);
    (inner, offset)
}

fn setting_row(state: &TuiEditorState, field: SettingField) -> (&'static str, String) {
    let cfg = &state.config;
    match field {
        SettingField::Preset => {
            let base = state.preset.display_name();
            let val = if state.config.matching_preset().is_none() {
                format!("{base}*")
            } else {
                base.to_string()
            };
            ("Preset", val)
        }
        SettingField::Style => ("Style", cfg.style.name().to_string()),
        SettingField::Theme => ("Theme", cfg.theme.name().to_string()),
        SettingField::Separator => ("Separator", cfg.separator.name().to_string()),
        SettingField::SpeedMode => ("Speed mode", cfg.speed_mode.name().to_string()),
        SettingField::CtxMode => (
            "Ctx mode",
            if cfg.ctx_remaining {
                "Remaining %"
            } else {
                "Used %"
            }
            .to_string(),
        ),
        SettingField::SmartHide => (
            "Smart hide",
            if cfg.smart_hide { "ON" } else { "OFF" }.to_string(),
        ),
        SettingField::GitDiff => (
            "Git diff",
            if cfg.show_git_diff { "ON" } else { "OFF" }.to_string(),
        ),
        SettingField::BarWidth => ("Bar width", format!("{} cols", cfg.bar_length)),
        SettingField::AgyMode => (
            "AGY mode",
            if cfg.stack_with_default {
                "Stack Below"
            } else {
                "Replace Default"
            }
            .to_string(),
        ),
        SettingField::Glyphs => ("Glyphs", cfg.glyph_mode.name().to_string()),
        SettingField::Responsive => (
            "Responsive",
            if cfg.responsive { "ON" } else { "OFF" }.to_string(),
        ),
        SettingField::SpeedWarn => (
            "Speed warn",
            if cfg.speed_warn_threshold <= 0.0 {
                "OFF".to_string()
            } else {
                format!("{:.0} tok/s", cfg.speed_warn_threshold)
            },
        ),
    }
}

fn draw_settings_pane(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &TuiEditorState,
) -> (Rect, usize, Option<Position>) {
    let active = state.active_pane == ActivePane::Settings;
    let block = pane_block(" 3. Style & AGY ".to_string(), active, false);
    let inner = block.inner(area);
    let inner_h = usize::from(inner.height).max(1);
    let offset = state.settings_idx.saturating_sub(inner_h.saturating_sub(1));
    let mut dec_pos = None;
    let lines: Vec<Line<'static>> = SettingField::ALL
        .iter()
        .enumerate()
        .skip(offset)
        .take(inner_h)
        .enumerate()
        .map(|(vis_row, (idx, &field))| {
            let (k, v) = setting_row(state, field);
            let is_cur = active && idx == state.settings_idx;
            if is_cur {
                let prefix = format!("▸ {k:<10} ");
                let dec_x = inner
                    .x
                    .saturating_add(u16::try_from(prefix.width()).unwrap_or(u16::MAX));
                let row_y = inner.y.saturating_add(vis_row as u16);
                if dec_x < inner.right() && row_y < inner.bottom() {
                    dec_pos = Some(Position::new(dec_x, row_y));
                }
                Line::styled(
                    format!("{prefix}< {v} >"),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Line::from(vec![
                    Span::styled(format!("  {k:<10} "), Style::default().fg(Color::Gray)),
                    Span::styled(v, Style::default().fg(Color::Yellow)),
                ])
            }
        })
        .collect();

    let panel = Paragraph::new(lines).block(block);
    frame.render_widget(panel, area);
    (inner, offset, dec_pos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::layout::LayoutConfig;

    #[test]
    fn canvas_wrapping_updates_line_y_and_pill_hit_coordinates() {
        let state = TuiEditorState::new(LayoutConfig::preset_full());
        let area = Rect::new(0, 0, 48, 18);
        let layout = layout_canvas_lines(area, &state);

        let line0_y = layout.row_ys.iter().find(|&&(_, r)| r == 0).unwrap().0;
        let line1_y = layout.row_ys.iter().find(|&&(_, r)| r == 1).unwrap().0;
        let last_row0 = layout.pills.iter().rfind(|p| p.row == 0).unwrap();
        let row1_pill = layout.pills.iter().find(|p| p.row == 1).unwrap();
        assert!(last_row0.y > line0_y);
        assert_eq!(line1_y, last_row0.y + 2);
        assert_eq!(row1_pill.y, line1_y);
        assert!(layout.row_ys.contains(&(last_row0.y, 0)));
    }

    #[test]
    fn config_mouse_clicks_move_mode_drop_setting_arrows_and_footer_actions() {
        use crate::config::layout::VisualStyle;

        let m = |kind, column, row| MouseEvent {
            kind,
            column,
            row,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        let mut state = TuiEditorState::new(LayoutConfig::preset_minimal());
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(150, 28)).unwrap();
        let mut map = UiHitMap::default();
        terminal.draw(|frame| map = draw_ui(frame, &state)).unwrap();

        // Click already-selected pill (0, 0) to grab, then click footer "Enter/Esc Drop" to drop.
        let p0 = map.canvas_pills[0];
        assert_eq!(
            handle_mouse(
                &mut state,
                m(MouseEventKind::Down(MouseButton::Left), p0.x0 + 1, p0.y),
                &map,
            ),
            ExitOutcome::Continue
        );
        assert!(state.grabbed);

        terminal.draw(|frame| map = draw_ui(frame, &state)).unwrap();
        let drop_rect = map
            .footer_hits
            .iter()
            .find(|(_, a)| *a == ConfigFooterClick::DropGrabbed)
            .unwrap()
            .0;
        handle_mouse(
            &mut state,
            m(
                MouseEventKind::Down(MouseButton::Left),
                drop_rect.x + 1,
                drop_rect.y,
            ),
            &map,
        );
        assert!(!state.grabbed);

        // Grab pill (0, 0) ("State") and click pill (0, 2) to reorder on the same line.
        terminal.draw(|frame| map = draw_ui(frame, &state)).unwrap();
        let p0 = map.canvas_pills[0];
        let p2 = map.canvas_pills[2];
        handle_mouse(
            &mut state,
            m(MouseEventKind::Down(MouseButton::Left), p0.x0 + 1, p0.y),
            &map,
        );
        assert!(state.grabbed);
        handle_mouse(
            &mut state,
            m(MouseEventKind::Down(MouseButton::Left), p2.x0 + 1, p2.y),
            &map,
        );
        assert!(!state.grabbed);
        assert_eq!(state.config.rows[0][2], WidgetId::State);

        // Grab pill (0, 2) ("State") and click Line 3 to move it across rows.
        terminal.draw(|frame| map = draw_ui(frame, &state)).unwrap();
        let moved = map.canvas_pills[2];
        let line3_y = map.canvas_row_ys.iter().find(|&&(_, r)| r == 2).unwrap().0;
        handle_mouse(
            &mut state,
            m(
                MouseEventKind::Down(MouseButton::Left),
                moved.x0 + 1,
                moved.y,
            ),
            &map,
        );
        assert!(state.grabbed);
        handle_mouse(
            &mut state,
            m(
                MouseEventKind::Down(MouseButton::Left),
                map.canvas.x + 2,
                line3_y,
            ),
            &map,
        );
        assert!(!state.grabbed);
        assert_eq!(state.canvas_row, 2);
        assert_eq!(state.config.rows[2], vec![WidgetId::State]);

        // Leaving Canvas while grabbed (via Settings click, Catalog/Settings scroll, or blank Canvas click) always drops Move Mode.
        terminal.draw(|frame| map = draw_ui(frame, &state)).unwrap();
        let line3_pill = map
            .canvas_pills
            .iter()
            .find(|p| p.row == 2)
            .copied()
            .unwrap();
        handle_mouse(
            &mut state,
            m(
                MouseEventKind::Down(MouseButton::Left),
                line3_pill.x0 + 1,
                line3_pill.y,
            ),
            &map,
        );
        assert!(state.grabbed);
        handle_mouse(
            &mut state,
            m(
                MouseEventKind::Down(MouseButton::Left),
                map.settings.x + 1,
                map.settings.y,
            ),
            &map,
        );
        assert_eq!(state.active_pane, ActivePane::Settings);
        assert!(!state.grabbed);

        state.click_canvas(2, Some(0));
        state.click_canvas(2, Some(0));
        assert!(state.grabbed);
        handle_mouse(
            &mut state,
            m(
                MouseEventKind::ScrollDown,
                map.catalog.x + 2,
                map.catalog.y + 2,
            ),
            &map,
        );
        assert_eq!(state.active_pane, ActivePane::Catalog);
        assert!(!state.grabbed);

        state.click_canvas(2, Some(0));
        state.click_canvas(2, Some(0));
        assert!(state.grabbed);
        handle_mouse(
            &mut state,
            m(
                MouseEventKind::ScrollDown,
                map.settings.x + 2,
                map.settings.y + 2,
            ),
            &map,
        );
        assert_eq!(state.active_pane, ActivePane::Settings);
        assert!(!state.grabbed);

        state.click_canvas(2, Some(0));
        state.click_canvas(2, Some(0));
        assert!(state.grabbed);
        handle_mouse(
            &mut state,
            m(
                MouseEventKind::Down(MouseButton::Left),
                map.canvas.x + 2,
                map.canvas.bottom() - 2,
            ),
            &map,
        );
        assert_eq!(state.active_pane, ActivePane::Canvas);
        assert!(!state.grabbed);

        // Settings pane: clicking '<' on active Style row steps backward; clicking right steps forward.
        state.active_pane = ActivePane::Settings;
        state.settings_idx = 1;
        terminal.draw(|frame| map = draw_ui(frame, &state)).unwrap();
        let dec = map.settings_dec_pos.unwrap();
        assert_eq!(terminal.backend().buffer()[(dec.x, dec.y)].symbol(), "<");
        handle_mouse(
            &mut state,
            m(MouseEventKind::Down(MouseButton::Left), dec.x, dec.y),
            &map,
        );
        assert_eq!(state.config.style, VisualStyle::Powerline);
        handle_mouse(
            &mut state,
            m(MouseEventKind::Down(MouseButton::Left), dec.x + 3, dec.y),
            &map,
        );
        assert_eq!(state.config.style, VisualStyle::Plain);

        // Footer actions: NextPane, CycleWidth, CyclePreset, SaveAndApply, Quit.
        let pane_rect = map
            .footer_hits
            .iter()
            .find(|(_, a)| *a == ConfigFooterClick::NextPane)
            .unwrap()
            .0;
        handle_mouse(
            &mut state,
            m(
                MouseEventKind::Down(MouseButton::Left),
                pane_rect.x + 1,
                pane_rect.y,
            ),
            &map,
        );
        assert_eq!(state.active_pane, ActivePane::Canvas);

        let width_rect = map
            .footer_hits
            .iter()
            .find(|(_, a)| *a == ConfigFooterClick::CycleWidth)
            .unwrap()
            .0;
        handle_mouse(
            &mut state,
            m(
                MouseEventKind::Down(MouseButton::Left),
                width_rect.x + 1,
                width_rect.y,
            ),
            &map,
        );
        assert_eq!(state.preview_width, PreviewWidth::Wide120);

        let preset_rect = map
            .footer_hits
            .iter()
            .find(|(_, a)| *a == ConfigFooterClick::CyclePreset)
            .unwrap()
            .0;
        handle_mouse(
            &mut state,
            m(
                MouseEventKind::Down(MouseButton::Left),
                preset_rect.x + 1,
                preset_rect.y,
            ),
            &map,
        );
        assert_eq!(state.preset, crate::config::layout::Preset::Addon);

        let save_rect = map
            .footer_hits
            .iter()
            .find(|(_, a)| *a == ConfigFooterClick::SaveAndApply)
            .unwrap()
            .0;
        assert_eq!(
            handle_mouse(
                &mut state,
                m(
                    MouseEventKind::Down(MouseButton::Left),
                    save_rect.x + 1,
                    save_rect.y,
                ),
                &map,
            ),
            ExitOutcome::SaveAndApply
        );

        let quit_rect = map
            .footer_hits
            .iter()
            .find(|(_, a)| *a == ConfigFooterClick::Quit)
            .unwrap()
            .0;
        assert_eq!(
            handle_mouse(
                &mut state,
                m(
                    MouseEventKind::Down(MouseButton::Left),
                    quit_rect.x + 1,
                    quit_rect.y,
                ),
                &map,
            ),
            ExitOutcome::Quit
        );
    }

    #[test]
    fn settings_pane_and_preview_reflect_median_speed_mode() {
        let mut cfg = LayoutConfig::preset_minimal();
        cfg.speed_mode = crate::config::layout::SpeedMode::Median;
        let state = TuiEditorState::new(cfg);

        let (preview_lines, _) = build_preview_lines(&state, 80);
        let preview_text: String = preview_lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(preview_text.contains("68 tok/s"));

        let backend = ratatui::backend::TestBackend::new(110, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
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
