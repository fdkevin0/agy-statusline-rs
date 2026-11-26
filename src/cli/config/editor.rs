//! Pure 3-pane TUI editor state machine for `agy-statusline config`.

use crossterm::event::{KeyCode, KeyModifiers};

use crate::config::layout::{
    LayoutConfig, Preset, WidgetId, MAX_BAR_LENGTH, MAX_ROWS, MAX_SPEED_WARN_THRESHOLD,
    MIN_BAR_LENGTH, MIN_SPEED_WARN_THRESHOLD, SPEED_WARN_STEP,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SettingField {
    Preset,
    Style,
    Theme,
    Separator,
    SpeedMode,
    CtxMode,
    SmartHide,
    GitDiff,
    BarWidth,
    AgyMode,
    Glyphs,
    Responsive,
    SpeedWarn,
}

impl SettingField {
    pub(super) const ALL: &'static [Self] = &[
        Self::Preset,
        Self::Style,
        Self::Theme,
        Self::Separator,
        Self::SpeedMode,
        Self::CtxMode,
        Self::SmartHide,
        Self::GitDiff,
        Self::BarWidth,
        Self::AgyMode,
        Self::Glyphs,
        Self::Responsive,
        Self::SpeedWarn,
    ];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ActivePane {
    Canvas,
    Catalog,
    Settings,
}

impl ActivePane {
    pub(super) const fn next(self) -> Self {
        match self {
            Self::Canvas => Self::Catalog,
            Self::Catalog => Self::Settings,
            Self::Settings => Self::Canvas,
        }
    }

    pub(super) const fn prev(self) -> Self {
        match self {
            Self::Canvas => Self::Settings,
            Self::Catalog => Self::Canvas,
            Self::Settings => Self::Catalog,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PreviewWidth {
    Auto,
    Wide120,
    Narrow72,
}

impl PreviewWidth {
    pub(super) const fn next(self) -> Self {
        match self {
            Self::Auto => Self::Wide120,
            Self::Wide120 => Self::Narrow72,
            Self::Narrow72 => Self::Auto,
        }
    }

    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Wide120 => "120col",
            Self::Narrow72 => "72col",
        }
    }

    pub(super) fn resolve(self, box_inner: u16) -> usize {
        match self {
            Self::Auto => usize::from(box_inner).max(40),
            Self::Wide120 => 120,
            Self::Narrow72 => 72,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ExitOutcome {
    Continue,
    Quit,
    SaveOnly,
    SaveAndApply,
}

#[derive(Debug, Clone)]
pub(super) struct TuiEditorState {
    pub(super) config: LayoutConfig,
    pub(super) active_pane: ActivePane,
    /// Active Canvas row (`0..MAX_ROWS`); also the target row when toggling in Catalog.
    pub(super) canvas_row: usize,
    pub(super) canvas_col: usize,
    pub(super) grabbed: bool,
    pub(super) catalog_idx: usize,
    pub(super) settings_idx: usize,
    pub(super) preset: Preset,
    pub(super) preview_width: PreviewWidth,
}

impl TuiEditorState {
    pub(super) fn new(mut config: LayoutConfig) -> Self {
        config.normalize_rows();
        let preset = config.matching_preset().unwrap_or(Preset::Full);
        Self {
            config,
            active_pane: ActivePane::Canvas,
            canvas_row: 0,
            canvas_col: 0,
            grabbed: false,
            catalog_idx: 0,
            settings_idx: 0,
            preset,
            preview_width: PreviewWidth::Auto,
        }
    }

    pub(super) fn apply_preset(&mut self, preset: Preset) {
        self.config = self.config.with_preset_preserving_prefs(preset);
        self.preset = preset;
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

    /// Switches focus to `pane` and clears Move Mode so `grabbed` is never stranded off-Canvas.
    pub(super) fn focus_pane(&mut self, pane: ActivePane) {
        self.active_pane = pane;
        self.grabbed = false;
    }

    /// Adjusts `setting_idx` by `dir` (`-1` or `+1`).
    pub(super) fn adjust_setting_at(&mut self, idx: usize, dir: isize) {
        self.focus_pane(ActivePane::Settings);
        if idx < SettingField::ALL.len() {
            self.settings_idx = idx;
            self.adjust_setting(dir);
        }
    }

    /// Selects `ActivePane::Canvas` row `row` and optional pill `col`.
    /// While a pill is grabbed in Move Mode, clicking another pill or row drops it there.
    pub(super) fn click_canvas(&mut self, row: usize, col: Option<usize>) {
        if row >= MAX_ROWS {
            return;
        }
        let was_canvas = self.active_pane == ActivePane::Canvas;
        self.active_pane = ActivePane::Canvas;
        let same_pill = was_canvas && self.canvas_row == row && col == Some(self.canvas_col);
        if self.grabbed {
            if same_pill || (self.canvas_row == row && col.is_none()) {
                self.grabbed = false;
                return;
            }
            if self.canvas_col < self.config.rows[self.canvas_row].len() {
                let w = self.config.rows[self.canvas_row].remove(self.canvas_col);
                let target_len = self.config.rows[row].len();
                let insert_idx = col.map_or(target_len, |c| c.min(target_len));
                self.config.rows[row].insert(insert_idx, w);
                self.canvas_row = row;
                self.canvas_col = insert_idx;
            }
            self.grabbed = false;
            return;
        }
        self.canvas_row = row;
        if let Some(c) = col.filter(|&c| c < self.config.rows[row].len()) {
            self.canvas_col = c;
            self.grabbed = same_pill;
        } else {
            self.clamp_canvas_col();
        }
    }

    /// Moves the Canvas row selection by `delta` (`-1` or `+1`) without moving a grabbed pill.
    pub(super) fn scroll_canvas(&mut self, delta: isize) {
        self.active_pane = ActivePane::Canvas;
        self.grabbed = false;
        let next = (self.canvas_row as isize + delta).clamp(0, (MAX_ROWS - 1) as isize);
        self.canvas_row = next as usize;
        self.clamp_canvas_col();
    }

    /// Removes the pill at `(row, col)` if present.
    pub(super) fn remove_canvas_pill(&mut self, row: usize, col: usize) {
        if row < MAX_ROWS && col < self.config.rows[row].len() {
            self.active_pane = ActivePane::Canvas;
            self.canvas_row = row;
            self.config.rows[row].remove(col);
            self.grabbed = false;
            self.clamp_canvas_col();
        }
    }

    /// Selects and toggles the widget at `idx` in `Catalog`.
    pub(super) fn click_catalog(&mut self, idx: usize) {
        self.focus_pane(ActivePane::Catalog);
        if idx < WidgetId::ALL.len() {
            self.catalog_idx = idx;
            self.toggle_catalog_widget();
        }
    }

    /// Moves `catalog_idx` by `delta` (`-1` or `+1`).
    pub(super) fn scroll_catalog(&mut self, delta: isize) {
        self.focus_pane(ActivePane::Catalog);
        let next = (self.catalog_idx as isize + delta).clamp(0, (WidgetId::ALL.len() - 1) as isize);
        self.catalog_idx = next as usize;
    }

    /// Moves `settings_idx` by `delta` (`-1` or `+1`).
    pub(super) fn scroll_settings(&mut self, delta: isize) {
        self.focus_pane(ActivePane::Settings);
        let next =
            (self.settings_idx as isize + delta).clamp(0, (SettingField::ALL.len() - 1) as isize);
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
        match SettingField::ALL[self.settings_idx] {
            SettingField::Preset => self.apply_preset(self.preset.step(dir)),
            SettingField::Style => self.config.style = self.config.style.step(dir),
            SettingField::Theme => self.config.theme = self.config.theme.step(dir),
            SettingField::Separator => self.config.separator = self.config.separator.step(dir),
            SettingField::SpeedMode => self.config.speed_mode = self.config.speed_mode.step(dir),
            SettingField::CtxMode => self.config.ctx_remaining = !self.config.ctx_remaining,
            SettingField::SmartHide => self.config.smart_hide = !self.config.smart_hide,
            SettingField::GitDiff => self.config.show_git_diff = !self.config.show_git_diff,
            SettingField::BarWidth => {
                self.config.bar_length = (self.config.bar_length as isize + dir)
                    .clamp(MIN_BAR_LENGTH as isize, MAX_BAR_LENGTH as isize)
                    as usize;
            }
            SettingField::AgyMode => {
                self.config.stack_with_default = !self.config.stack_with_default;
            }
            SettingField::Glyphs => self.config.glyph_mode = self.config.glyph_mode.step(dir),
            SettingField::Responsive => self.config.responsive = !self.config.responsive,
            SettingField::SpeedWarn => {
                self.config.speed_warn_threshold = (self.config.speed_warn_threshold
                    + (dir as f64) * SPEED_WARN_STEP)
                    .clamp(MIN_SPEED_WARN_THRESHOLD, MAX_SPEED_WARN_THRESHOLD);
            }
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
            KeyCode::Down | KeyCode::Char('j')
                if self.settings_idx + 1 < SettingField::ALL.len() =>
            {
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
    pub(super) fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> ExitOutcome {
        if code == KeyCode::Char('c') && mods.contains(KeyModifiers::CONTROL) {
            return ExitOutcome::Quit;
        }
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
                KeyCode::Char('s') if mods.contains(KeyModifiers::CONTROL) => {
                    return ExitOutcome::SaveOnly
                }
                KeyCode::Char('S') => return ExitOutcome::SaveOnly,
                KeyCode::Char('s') => return ExitOutcome::SaveAndApply,
                KeyCode::Char('q') | KeyCode::Esc => return ExitOutcome::Quit,
                KeyCode::Char('w') => {
                    self.preview_width = self.preview_width.next();
                    return ExitOutcome::Continue;
                }
                KeyCode::Char(c @ '1'..='4') => {
                    if let Some(&preset) = Preset::ALL.get((c as u8 - b'1') as usize) {
                        self.apply_preset(preset);
                    }
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
    use crate::config::layout::{GlyphMode, SpeedMode, ThemeStyle, VisualStyle};

    #[test]
    fn move_mode_swaps_within_and_across_rows() {
        let mut ed = TuiEditorState::new(LayoutConfig::preset_full());
        assert_eq!(
            ed.handle_key(KeyCode::Enter, KeyModifiers::NONE),
            ExitOutcome::Continue
        );
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
        assert_eq!(ed.preset, Preset::Minimal);
        assert!(ed.config.matching_preset().is_some());

        ed.handle_key(KeyCode::Down, KeyModifiers::NONE);
        ed.handle_key(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(ed.canvas_row, 2);
        ed.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(ed.active_pane, ActivePane::Catalog);

        ed.handle_key(KeyCode::Down, KeyModifiers::NONE);
        ed.handle_key(KeyCode::Down, KeyModifiers::NONE);
        ed.handle_key(KeyCode::Char(' '), KeyModifiers::NONE);
        assert_eq!(ed.config.rows[2], vec![WidgetId::Directory]);
        assert!(ed.config.matching_preset().is_none());

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
        assert!((ed.config.speed_warn_threshold - 40.0).abs() < 1e-6);
        ed.config.speed_warn_threshold = 5.0;
        ed.adjust_setting_at(12, -1);
        assert_eq!(ed.config.speed_warn_threshold, 0.0);
        ed.adjust_setting_at(12, -1);
        assert_eq!(ed.config.speed_warn_threshold, 0.0);

        assert_eq!(
            ed.handle_key(KeyCode::Char('s'), KeyModifiers::NONE),
            ExitOutcome::SaveAndApply
        );
        // Ctrl+C always exits even while a pill is grabbed in Move Mode.
        ed.grabbed = true;
        assert_eq!(
            ed.handle_key(KeyCode::Char('c'), KeyModifiers::CONTROL),
            ExitOutcome::Quit
        );
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
