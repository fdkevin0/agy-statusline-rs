//! Pure 3-pane TUI editor state machine for `agy-statusline config`.

use crossterm::event::{KeyCode, KeyModifiers};

use crate::layout::{LayoutConfig, WidgetId, MAX_ROWS};

/// Currently focused pane inside the 3-pane configurator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivePane {
    /// Left pane: 3-line layout canvas with pill reordering and Move Mode.
    Canvas,
    /// Center pane: catalog of all 33 widgets toggled into the target line.
    Catalog,
    /// Right pane: presets, visual style, theme, separator, thresholds, and AGY mode.
    Settings,
}

impl ActivePane {
    /// Cycles forward (`Canvas` -> `Catalog` -> `Settings` -> `Canvas`).
    pub const fn next(self) -> Self {
        match self {
            Self::Canvas => Self::Catalog,
            Self::Catalog => Self::Settings,
            Self::Settings => Self::Canvas,
        }
    }

    /// Cycles backward (`Canvas` -> `Settings` -> `Catalog` -> `Canvas`).
    pub const fn prev(self) -> Self {
        match self {
            Self::Canvas => Self::Settings,
            Self::Catalog => Self::Canvas,
            Self::Settings => Self::Catalog,
        }
    }
}

/// Preview width mode for testing responsive truncation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewWidth {
    /// Use current terminal width inside the preview box.
    Auto,
    /// Simulate wide 120-column terminal.
    Wide120,
    /// Simulate narrow 72-column terminal.
    Narrow72,
}

impl PreviewWidth {
    /// Cycles preview width (`Auto` -> `Wide120` -> `Narrow72` -> `Auto`).
    pub const fn next(self) -> Self {
        match self {
            Self::Auto => Self::Wide120,
            Self::Wide120 => Self::Narrow72,
            Self::Narrow72 => Self::Auto,
        }
    }

    /// Label shown in the Live Preview title bar.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Wide120 => "120col",
            Self::Narrow72 => "72col",
        }
    }

    /// Effective column budget given the preview pane's inner width.
    pub fn resolve(self, box_inner: u16) -> usize {
        match self {
            Self::Auto => usize::from(box_inner).max(40),
            Self::Wide120 => 120,
            Self::Narrow72 => 72,
        }
    }
}

/// Action returned by [`TuiEditorState::handle_key`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitOutcome {
    /// Keep running the TUI event loop.
    Continue,
    /// Exit without saving changes.
    Quit,
    /// Save XDG `config.json` only and exit.
    SaveOnly,
    /// Save XDG `config.json` AND sync AGY `settings.json`, then exit.
    SaveAndApply,
}

/// State for the interactive 3-pane statusline configurator.
#[derive(Debug, Clone)]
pub struct TuiEditorState {
    /// Editable layout configuration.
    pub config: LayoutConfig,
    /// Active pane (`Canvas`, `Catalog`, `Settings`).
    pub active_pane: ActivePane,
    /// Active row in Canvas (`0..3`), also the target line when toggling in Catalog.
    pub canvas_row: usize,
    /// Active widget index within `config.rows[canvas_row]`.
    pub canvas_col: usize,
    /// Whether the active Canvas widget is currently grabbed for Move Mode.
    pub grabbed: bool,
    /// Selected index in `WidgetId::ALL` inside `Catalog`.
    pub catalog_idx: usize,
    /// Selected index (`0..13`) inside `Settings`.
    pub settings_idx: usize,
    /// Active preset cursor (`0=Full`, `1=Powerline`, `2=Minimal`, `3=Addon`).
    pub preset_idx: usize,
    /// Current Live Preview width simulation mode.
    pub preview_width: PreviewWidth,
}

impl TuiEditorState {
    /// Total rows in the Settings pane.
    pub const SETTINGS_COUNT: usize = 13;

    /// Initializes editor state from `config`.
    pub fn new(mut config: LayoutConfig) -> Self {
        config.normalize_rows();
        let preset_idx = config.matching_preset_index().unwrap_or(0);
        Self {
            config,
            active_pane: ActivePane::Canvas,
            canvas_row: 0,
            canvas_col: 0,
            grabbed: false,
            catalog_idx: 0,
            settings_idx: 0,
            preset_idx,
            preview_width: PreviewWidth::Auto,
        }
    }

    /// Returns true if `config` has been modified away from all built-in presets.
    pub fn is_custom_preset(&self) -> bool {
        self.config.matching_preset_index().is_none()
    }

    /// Applies one of the 4 built-in presets (`1..=4`).
    pub fn apply_preset(&mut self, one_based: usize) {
        self.config = match one_based {
            1 => LayoutConfig::preset_full(),
            2 => LayoutConfig::preset_powerline(),
            3 => LayoutConfig::preset_minimal(),
            4 => LayoutConfig::preset_addon(),
            _ => return,
        };
        self.preset_idx = one_based - 1;
        self.canvas_row = 0;
        self.canvas_col = 0;
        self.grabbed = false;
    }

    fn clamp_canvas_col(&mut self) {
        let len = self.config.rows[self.canvas_row].len();
        if len == 0 {
            self.canvas_col = 0;
        } else if self.canvas_col >= len {
            self.canvas_col = len - 1;
        }
    }

    fn swap_canvas_left(&mut self) {
        let row = &mut self.config.rows[self.canvas_row];
        if self.canvas_col > 0 && self.canvas_col < row.len() {
            row.swap(self.canvas_col, self.canvas_col - 1);
            self.canvas_col -= 1;
        }
    }

    fn swap_canvas_right(&mut self) {
        let row = &mut self.config.rows[self.canvas_row];
        if self.canvas_col + 1 < row.len() {
            row.swap(self.canvas_col, self.canvas_col + 1);
            self.canvas_col += 1;
        }
    }

    fn move_grabbed_row(&mut self, delta: isize) {
        let next = self.canvas_row as isize + delta;
        if (0..MAX_ROWS as isize).contains(&next)
            && self.canvas_col < self.config.rows[self.canvas_row].len()
        {
            let w = self.config.rows[self.canvas_row].remove(self.canvas_col);
            self.canvas_row = next as usize;
            self.config.rows[self.canvas_row].push(w);
            self.canvas_col = self.config.rows[self.canvas_row].len() - 1;
        }
    }

    /// Adjusts `setting_idx` by `dir` (`-1` or `+1`).
    pub fn adjust_setting_at(&mut self, idx: usize, dir: isize) {
        if idx < Self::SETTINGS_COUNT {
            self.settings_idx = idx;
            self.adjust_setting(dir);
        }
    }

    /// Selects `ActivePane::Canvas` row `r` and optional pill `col` (toggling Move Mode if re-clicked).
    pub fn click_canvas(&mut self, row: usize, col: Option<usize>) {
        if row >= MAX_ROWS {
            return;
        }
        self.active_pane = ActivePane::Canvas;
        let same_pill = self.canvas_row == row && col == Some(self.canvas_col);
        self.canvas_row = row;
        if let Some(c) = col.filter(|&c| c < self.config.rows[row].len()) {
            self.canvas_col = c;
            self.grabbed = if same_pill { !self.grabbed } else { false };
        } else {
            self.grabbed = false;
            self.clamp_canvas_col();
        }
    }

    /// Removes the pill at `(row, col)` if present.
    pub fn remove_canvas_pill(&mut self, row: usize, col: usize) {
        if row < MAX_ROWS && col < self.config.rows[row].len() {
            self.active_pane = ActivePane::Canvas;
            self.canvas_row = row;
            self.config.rows[row].remove(col);
            self.grabbed = false;
            self.clamp_canvas_col();
        }
    }

    /// Selects and toggles the widget at `idx` in `Catalog`.
    pub fn click_catalog(&mut self, idx: usize) {
        if idx < WidgetId::ALL.len() {
            self.active_pane = ActivePane::Catalog;
            self.grabbed = false;
            self.catalog_idx = idx;
            self.toggle_catalog_widget();
        }
    }

    /// Moves `catalog_idx` by `delta` (`-1` or `+1`).
    pub fn scroll_catalog(&mut self, delta: isize) {
        self.active_pane = ActivePane::Catalog;
        let next = (self.catalog_idx as isize + delta).clamp(0, (WidgetId::ALL.len() - 1) as isize);
        self.catalog_idx = next as usize;
    }

    /// Moves `settings_idx` by `delta` (`-1` or `+1`).
    pub fn scroll_settings(&mut self, delta: isize) {
        self.active_pane = ActivePane::Settings;
        let next = (self.settings_idx as isize + delta).clamp(0, (Self::SETTINGS_COUNT - 1) as isize);
        self.settings_idx = next as usize;
    }

    fn toggle_catalog_widget(&mut self) {
        let widget = WidgetId::ALL[self.catalog_idx];
        if widget == WidgetId::Spacer {
            if let Some(c) = self.config.rows[self.canvas_row]
                .iter()
                .rposition(|&w| w == WidgetId::Spacer)
            {
                self.config.rows[self.canvas_row].remove(c);
                self.clamp_canvas_col();
            } else {
                self.config.rows[self.canvas_row].push(WidgetId::Spacer);
                self.canvas_col = self.config.rows[self.canvas_row].len() - 1;
            }
            return;
        }
        if let Some((r, c)) = self.config.locate_widget(widget) {
            self.config.rows[r].remove(c);
            self.clamp_canvas_col();
        } else {
            self.config.rows[self.canvas_row].push(widget);
            self.canvas_col = self.config.rows[self.canvas_row].len() - 1;
        }
    }

    fn adjust_setting(&mut self, dir: isize) {
        match self.settings_idx {
            0 => {
                let next = ((self.preset_idx as isize + dir).rem_euclid(4)) as usize;
                self.apply_preset(next + 1);
            }
            1 => self.config.style = self.config.style.step(dir),
            2 => self.config.theme = self.config.theme.step(dir),
            3 => self.config.separator = self.config.separator.step(dir),
            4 => self.config.speed_mode = self.config.speed_mode.step(dir),
            5 => self.config.ctx_remaining = !self.config.ctx_remaining,
            6 => self.config.smart_hide = !self.config.smart_hide,
            7 => self.config.show_git_diff = !self.config.show_git_diff,
            8 => self.config.bar_length = (self.config.bar_length as isize + dir).clamp(4, 40) as usize,
            9 => self.config.stack_with_default = !self.config.stack_with_default,
            10 => self.config.glyph_mode = self.config.glyph_mode.step(dir),
            11 => self.config.responsive = !self.config.responsive,
            12 => {
                self.config.speed_warn_threshold =
                    (self.config.speed_warn_threshold + (dir as f64) * 5.0).clamp(10.0, 200.0);
            }
            _ => {}
        }
    }

    fn handle_canvas_key(&mut self, code: KeyCode) {
        if self.grabbed {
            match code {
                KeyCode::Left | KeyCode::Char('h') => self.swap_canvas_left(),
                KeyCode::Right | KeyCode::Char('l') => self.swap_canvas_right(),
                KeyCode::Up | KeyCode::Char('k') => self.move_grabbed_row(-1),
                KeyCode::Down | KeyCode::Char('j') => self.move_grabbed_row(1),
                KeyCode::Enter | KeyCode::Esc | KeyCode::Char(' ') | KeyCode::Char('m') => {
                    self.grabbed = false;
                }
                _ => {}
            }
            return;
        }

        match code {
            KeyCode::Up | KeyCode::Char('k') if self.canvas_row > 0 => {
                self.canvas_row -= 1;
                self.clamp_canvas_col();
            }
            KeyCode::Down | KeyCode::Char('j') if self.canvas_row + 1 < MAX_ROWS => {
                self.canvas_row += 1;
                self.clamp_canvas_col();
            }
            KeyCode::Left | KeyCode::Char('h') if self.canvas_col > 0 => {
                self.canvas_col -= 1;
            }
            KeyCode::Right | KeyCode::Char('l') => {
                if self.canvas_col + 1 < self.config.rows[self.canvas_row].len() {
                    self.canvas_col += 1;
                }
            }
            KeyCode::Char('[') => self.swap_canvas_left(),
            KeyCode::Char(']') => self.swap_canvas_right(),
            KeyCode::Enter | KeyCode::Char('m') => {
                if self.config.rows[self.canvas_row].is_empty() {
                    self.active_pane = ActivePane::Catalog;
                } else {
                    self.grabbed = true;
                }
            }
            KeyCode::Char(' ') | KeyCode::Backspace | KeyCode::Delete | KeyCode::Char('x') => {
                if self.config.rows[self.canvas_row].is_empty() {
                    if matches!(code, KeyCode::Char(' ')) {
                        self.active_pane = ActivePane::Catalog;
                    }
                } else if self.canvas_col < self.config.rows[self.canvas_row].len() {
                    self.config.rows[self.canvas_row].remove(self.canvas_col);
                    self.clamp_canvas_col();
                }
            }
            _ => {}
        }
    }

    fn handle_catalog_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Up | KeyCode::Char('k') if self.catalog_idx > 0 => {
                self.catalog_idx -= 1;
            }
            KeyCode::Down | KeyCode::Char('j') if self.catalog_idx + 1 < WidgetId::ALL.len() => {
                self.catalog_idx += 1;
            }
            KeyCode::Char(' ') | KeyCode::Enter => self.toggle_catalog_widget(),
            KeyCode::Char('+')
                if WidgetId::ALL[self.catalog_idx] == WidgetId::Spacer
                    && self.config.rows[self.canvas_row]
                        .iter()
                        .filter(|&&w| w == WidgetId::Spacer)
                        .count()
                        < 2 =>
            {
                self.config.rows[self.canvas_row].push(WidgetId::Spacer);
                self.canvas_col = self.config.rows[self.canvas_row].len() - 1;
            }
            _ => {}
        }
    }

    fn handle_settings_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Up | KeyCode::Char('k') if self.settings_idx > 0 => {
                self.settings_idx -= 1;
            }
            KeyCode::Down | KeyCode::Char('j') if self.settings_idx + 1 < Self::SETTINGS_COUNT => {
                self.settings_idx += 1;
            }
            KeyCode::Left | KeyCode::Char('h') => self.adjust_setting(-1),
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Char(' ') | KeyCode::Enter => {
                self.adjust_setting(1);
            }
            _ => {}
        }
    }

    /// Dispatches a key press and returns the resulting [`ExitOutcome`].
    pub fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> ExitOutcome {
        match code {
            KeyCode::Tab => {
                self.grabbed = false;
                self.active_pane = self.active_pane.next();
                return ExitOutcome::Continue;
            }
            KeyCode::BackTab => {
                self.grabbed = false;
                self.active_pane = self.active_pane.prev();
                return ExitOutcome::Continue;
            }
            _ => {}
        }

        if !self.grabbed {
            match code {
                KeyCode::Char('s') if mods.contains(KeyModifiers::CONTROL) => return ExitOutcome::SaveOnly,
                KeyCode::Char('S') => return ExitOutcome::SaveOnly,
                KeyCode::Char('s') => return ExitOutcome::SaveAndApply,
                KeyCode::Char('q') | KeyCode::Esc => return ExitOutcome::Quit,
                KeyCode::Char('w') => {
                    self.preview_width = self.preview_width.next();
                    return ExitOutcome::Continue;
                }
                KeyCode::Char(c @ '1'..='4') => {
                    self.apply_preset((c as u8 - b'0') as usize);
                    return ExitOutcome::Continue;
                }
                _ => {}
            }
        }

        match self.active_pane {
            ActivePane::Canvas => self.handle_canvas_key(code),
            ActivePane::Catalog => self.handle_catalog_key(code),
            ActivePane::Settings => self.handle_settings_key(code),
        }
        ExitOutcome::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{GlyphMode, SpeedMode, ThemeStyle, VisualStyle};

    #[test]
    fn move_mode_swaps_within_and_across_rows() {
        let mut ed = TuiEditorState::new(LayoutConfig::preset_full());
        assert_eq!(ed.handle_key(KeyCode::Enter, KeyModifiers::NONE), ExitOutcome::Continue);
        assert!(ed.grabbed);
        ed.handle_key(KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(ed.config.rows[0][1], WidgetId::State);

        ed.handle_key(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(ed.canvas_row, 1);
        assert_eq!(ed.config.rows[1].last().copied(), Some(WidgetId::State));
        ed.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(!ed.grabbed);
    }

    #[test]
    fn catalog_adds_to_selected_target_line_and_settings_cycle() {
        let mut ed = TuiEditorState::new(LayoutConfig::preset_minimal());
        assert_eq!(ed.preset_idx, 2);
        assert!(!ed.is_custom_preset());

        ed.handle_key(KeyCode::Down, KeyModifiers::NONE);
        ed.handle_key(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(ed.canvas_row, 2);
        ed.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(ed.active_pane, ActivePane::Catalog);

        ed.handle_key(KeyCode::Down, KeyModifiers::NONE);
        ed.handle_key(KeyCode::Down, KeyModifiers::NONE);
        ed.handle_key(KeyCode::Char(' '), KeyModifiers::NONE);
        assert_eq!(ed.config.rows[2], vec![WidgetId::Directory]);
        assert!(ed.is_custom_preset());

        ed.handle_key(KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(ed.active_pane, ActivePane::Settings);
        ed.handle_key(KeyCode::Down, KeyModifiers::NONE);
        ed.handle_key(KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(ed.config.style, VisualStyle::Pill);
        ed.handle_key(KeyCode::Down, KeyModifiers::NONE);
        ed.handle_key(KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(ed.config.theme, ThemeStyle::Nord);

        ed.adjust_setting_at(4, 1);
        assert_eq!(ed.config.speed_mode, SpeedMode::Median);
        ed.adjust_setting_at(4, 1);
        assert_eq!(ed.config.speed_mode, SpeedMode::Instant);
        ed.adjust_setting_at(4, 1);
        assert_eq!(ed.config.speed_mode, SpeedMode::E2e);
        ed.adjust_setting_at(4, 1);
        assert_eq!(ed.config.speed_mode, SpeedMode::Weighted);

        ed.adjust_setting_at(10, 1);
        assert_eq!(ed.config.glyph_mode, GlyphMode::Nerd);
        ed.adjust_setting_at(11, 1);
        assert!(!ed.config.responsive);
        ed.adjust_setting_at(12, 1);
        assert!((ed.config.speed_warn_threshold - 55.0).abs() < 1e-6);

        assert_eq!(ed.handle_key(KeyCode::Char('s'), KeyModifiers::NONE), ExitOutcome::SaveAndApply);
    }

    #[test]
    fn mouse_helpers_select_toggle_and_scroll() {
        let mut ed = TuiEditorState::new(LayoutConfig::preset_minimal());
        ed.click_canvas(0, Some(1));
        assert!(!ed.grabbed);
        ed.click_canvas(0, Some(1));
        assert!(ed.grabbed);
        ed.remove_canvas_pill(0, 1);
        assert!(!ed.grabbed);

        ed.click_catalog(2);
        assert_eq!(ed.active_pane, ActivePane::Catalog);
        assert!(ed.config.rows[0].contains(&WidgetId::Directory));

        ed.scroll_settings(1);
        assert_eq!(ed.settings_idx, 1);
        ed.adjust_setting_at(1, 1);
        assert_eq!(ed.config.style, VisualStyle::Pill);
    }

    #[test]
    fn spacer_toggles_independently_across_rows() {
        let mut ed = TuiEditorState::new(LayoutConfig::preset_minimal());
        let spacer_idx = WidgetId::ALL.len() - 1;
        ed.click_catalog(spacer_idx);
        assert!(ed.config.rows[0].contains(&WidgetId::Spacer));

        ed.click_canvas(1, None);
        ed.click_catalog(spacer_idx);
        assert!(ed.config.rows[0].contains(&WidgetId::Spacer));
        assert!(ed.config.rows[1].contains(&WidgetId::Spacer));

        ed.click_catalog(spacer_idx);
        assert!(ed.config.rows[0].contains(&WidgetId::Spacer));
        assert!(!ed.config.rows[1].contains(&WidgetId::Spacer));
    }
}
