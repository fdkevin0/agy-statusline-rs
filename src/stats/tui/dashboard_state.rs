//! Pure, zero-I/O state machine for the interactive `agy-statusline stats` TUI.

use crossterm::event::{KeyCode, KeyModifiers};

use crate::stats::metric::StatsMetric;
use crate::stats::query::{ModelScope, RangePreset, StatsQuery, StatsView};
use crate::stats::ModelStats;

/// Focused interactive pane inside the `stats` TUI.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum StatsPane {
    /// Scrollable daily usage table with per-day `Cache%` and proportional bar.
    #[default]
    Daily,
    /// Per-day or window model breakdown with `Enter` cross-filtering.
    Models,
}

impl StatsPane {
    pub(super) const fn toggle(self) -> Self {
        match self {
            Self::Daily => Self::Models,
            Self::Models => Self::Daily,
        }
    }
}

/// Action emitted by [`StatsTuiState::handle_key`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StatsAction {
    Continue,
    Refresh,
    OpenConversations,
    Quit,
}

/// Complete mutable UI state for the interactive stats dashboard.
#[derive(Debug, Clone)]
pub(super) struct StatsTuiState {
    query: StatsQuery,
    range_options: Vec<RangePreset>,
    active_pane: StatsPane,
    daily_idx: usize,
    selected_date: Option<String>,
    model_idx: usize,
    /// `None` = auto based on terminal height (`>= 32`), `Some(v)` = manual override via `g`.
    show_heatmap: Option<bool>,
    status_msg: Option<String>,
    loading: bool,
    view_dirty: bool,
}

impl StatsTuiState {
    /// Initializes state from CLI arguments and any persisted `stats_metric`.
    pub(super) fn new(initial_days: Option<u32>, initial_metric: StatsMetric) -> Self {
        let range_options = RangePreset::options_with_custom(initial_days);
        let range = initial_days
            .filter(|&d| d > 0)
            .map_or(RangePreset::All, RangePreset::Days);
        Self {
            query: StatsQuery {
                range,
                metric: initial_metric,
                ..Default::default()
            },
            range_options,
            active_pane: StatsPane::Daily,
            daily_idx: 0,
            selected_date: None,
            model_idx: 0,
            show_heatmap: None,
            status_msg: None,
            loading: true,
            view_dirty: true,
        }
    }

    pub(super) fn query(&self) -> &StatsQuery {
        &self.query
    }

    pub(super) fn range_options(&self) -> &[RangePreset] {
        &self.range_options
    }

    pub(super) fn active_pane(&self) -> StatsPane {
        self.active_pane
    }

    pub(super) fn daily_idx(&self) -> usize {
        self.daily_idx
    }

    pub(super) fn selected_date(&self) -> Option<&str> {
        self.selected_date.as_deref()
    }

    pub(super) fn model_idx(&self) -> usize {
        self.model_idx
    }

    pub(super) fn status_msg(&self) -> Option<&str> {
        self.status_msg.as_deref()
    }

    pub(super) fn is_loading(&self) -> bool {
        self.loading
    }

    pub(super) fn is_view_dirty(&self) -> bool {
        self.view_dirty
    }

    pub(super) fn begin_refresh(&mut self, msg: impl Into<String>) {
        self.loading = true;
        self.status_msg = Some(msg.into());
    }

    pub(super) fn finish_scan(&mut self, status_msg: Option<String>) {
        self.loading = false;
        self.view_dirty = true;
        self.status_msg = status_msg;
    }

    pub(super) fn fail_scan(&mut self, msg: impl Into<String>) {
        self.loading = false;
        self.status_msg = Some(msg.into());
    }

    pub(super) fn focus_daily(&mut self) {
        self.active_pane = StatsPane::Daily;
    }

    pub(super) fn click_model_row(&mut self, target: Option<usize>, view: &StatsView) {
        let was_active = self.active_pane == StatsPane::Models;
        self.active_pane = StatsPane::Models;
        if let Some(idx) = target {
            if was_active && self.model_idx == idx {
                self.toggle_model_filter(idx, view);
            } else if idx < self.active_models(view).len() {
                self.model_idx = idx;
            }
        }
    }

    /// Resolves whether the 20-week activity heatmap should be displayed at `terminal_height`.
    pub fn heatmap_visible(&self, terminal_height: u16) -> bool {
        self.show_heatmap.unwrap_or(terminal_height >= 32)
    }

    /// Returns the currently displayed models slice for the active [`ModelScope`].
    pub fn active_models<'a>(&self, view: &'a StatsView) -> &'a [ModelStats] {
        match self.query.model_scope {
            ModelScope::SelectedDay => view
                .selected_day
                .as_ref()
                .map_or(&[][..], |d| d.models.as_slice()),
            ModelScope::Window => view.window_models.as_slice(),
        }
    }

    /// Reconciles cursor indices and `selected_date` against a newly computed [`StatsView`].
    pub fn sync_with_view(&mut self, view: &StatsView) {
        if let Some(day) = view.selected_day.as_ref() {
            self.daily_idx = view
                .daily_rows
                .iter()
                .position(|r| r.date == day.date)
                .unwrap_or(0);
            self.selected_date = Some(day.date.clone());
        } else {
            self.daily_idx = 0;
            self.selected_date = None;
        }

        let models_len = self.active_models(view).len();
        if models_len == 0 {
            self.model_idx = 0;
        } else if self.model_idx >= models_len {
            self.model_idx = models_len - 1;
        }
        self.view_dirty = false;
    }

    pub fn select_metric(&mut self, metric: StatsMetric) {
        if self.query.metric != metric {
            self.query.metric = metric;
            self.view_dirty = true;
            self.status_msg = None;
        }
    }

    pub fn select_range(&mut self, range: RangePreset) {
        if self.query.range != range {
            self.query.range = range;
            self.daily_idx = 0;
            self.view_dirty = true;
            self.status_msg = None;
        }
    }

    pub fn cycle_range(&mut self, dir: isize) {
        if self.range_options.is_empty() {
            return;
        }
        let idx = self
            .range_options
            .iter()
            .position(|&r| r == self.query.range)
            .unwrap_or(0) as isize;
        let next = (idx + dir).rem_euclid(self.range_options.len() as isize) as usize;
        self.select_range(self.range_options[next]);
    }

    pub fn cycle_sort(&mut self) {
        self.query.sort = self.query.sort.next();
        self.view_dirty = true;
        self.status_msg = None;
    }

    pub fn cycle_source(&mut self) {
        self.query.source_filter = self.query.source_filter.next();
        self.view_dirty = true;
        self.status_msg = None;
    }

    pub fn toggle_model_scope(&mut self) {
        self.query.model_scope = self.query.model_scope.toggle();
        self.model_idx = 0;
        self.status_msg = None;
    }

    pub fn toggle_fill_empty_days(&mut self) {
        self.query.fill_empty_days = !self.query.fill_empty_days;
        self.view_dirty = true;
        self.status_msg = None;
    }

    pub fn toggle_heatmap(&mut self, terminal_height: u16) {
        let currently_shown = self.heatmap_visible(terminal_height);
        self.show_heatmap = Some(!currently_shown);
    }

    pub fn clear_model_filter(&mut self) {
        if self.query.model_filter.take().is_some() {
            self.view_dirty = true;
            self.status_msg = None;
        }
    }

    /// Selects daily row `idx` and updates `selected_date` so Day Detail refreshes.
    pub fn select_daily_row(&mut self, idx: usize, view: &StatsView) {
        if view.daily_rows.is_empty() {
            return;
        }
        let clamped = idx.min(view.daily_rows.len() - 1);
        self.active_pane = StatsPane::Daily;
        let changed = self.daily_idx != clamped
            || self.selected_date.as_deref() != Some(view.daily_rows[clamped].date.as_str());
        self.daily_idx = clamped;
        self.selected_date = Some(view.daily_rows[clamped].date.clone());
        if changed {
            self.model_idx = 0;
            self.view_dirty = true;
        }
    }

    pub fn move_daily(&mut self, delta: isize, view: &StatsView) {
        if view.daily_rows.is_empty() {
            return;
        }
        let max_idx = (view.daily_rows.len() - 1) as isize;
        let next = (self.daily_idx as isize + delta).clamp(0, max_idx) as usize;
        self.select_daily_row(next, view);
    }

    pub fn move_model(&mut self, delta: isize, view: &StatsView) {
        let len = self.active_models(view).len();
        if len == 0 {
            return;
        }
        self.active_pane = StatsPane::Models;
        let max_idx = (len - 1) as isize;
        self.model_idx = (self.model_idx as isize + delta).clamp(0, max_idx) as usize;
    }

    /// Toggles cross-filtering on the model at `idx`.
    pub fn toggle_model_filter(&mut self, idx: usize, view: &StatsView) {
        if let Some(m) = self.active_models(view).get(idx) {
            self.active_pane = StatsPane::Models;
            self.model_idx = idx;
            let name = m.model.clone();
            if self.query.model_filter.as_deref() == Some(name.as_str()) {
                self.query.model_filter = None;
            } else {
                self.query.model_filter = Some(name);
            }
            self.view_dirty = true;
            self.status_msg = None;
        }
    }

    /// Dispatches a key press and returns the resulting [`StatsAction`].
    pub fn handle_key(
        &mut self,
        code: KeyCode,
        mods: KeyModifiers,
        view: Option<&StatsView>,
        page_size: usize,
        terminal_height: u16,
    ) -> StatsAction {
        if matches!(code, KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL)) {
            return StatsAction::Quit;
        }
        match code {
            KeyCode::Char('q') => return StatsAction::Quit,
            KeyCode::Esc => {
                if self.query.model_filter.is_some() {
                    self.clear_model_filter();
                    return StatsAction::Continue;
                }
                return StatsAction::Quit;
            }
            KeyCode::Char('r') => return StatsAction::Refresh,
            KeyCode::Char('c') => return StatsAction::OpenConversations,
            _ => {}
        }

        match code {
            KeyCode::Char(c @ '1'..='5') => {
                let idx = (c as u8 - b'1') as usize;
                if let Some(&metric) = StatsMetric::PRESETS.get(idx) {
                    self.select_metric(metric);
                }
            }
            KeyCode::Char('[') => self.select_metric(self.query.metric.step(-1)),
            KeyCode::Char(']') => self.select_metric(self.query.metric.step(1)),
            KeyCode::Char('t') => self.cycle_range(1),
            KeyCode::Char('T') => self.cycle_range(-1),
            KeyCode::Char('s') => self.cycle_sort(),
            KeyCode::Char('v') => self.toggle_model_scope(),
            KeyCode::Char('f') => self.cycle_source(),
            KeyCode::Char('z') => self.toggle_fill_empty_days(),
            KeyCode::Char('g') => self.toggle_heatmap(terminal_height),
            KeyCode::Tab | KeyCode::BackTab => {
                self.active_pane = self.active_pane.toggle();
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(v) = view {
                    match self.active_pane {
                        StatsPane::Daily => self.move_daily(-1, v),
                        StatsPane::Models => self.move_model(-1, v),
                    }
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(v) = view {
                    match self.active_pane {
                        StatsPane::Daily => self.move_daily(1, v),
                        StatsPane::Models => self.move_model(1, v),
                    }
                }
            }
            KeyCode::PageUp => {
                if let Some(v) = view {
                    let step = -(page_size.max(1) as isize);
                    match self.active_pane {
                        StatsPane::Daily => self.move_daily(step, v),
                        StatsPane::Models => self.move_model(step, v),
                    }
                }
            }
            KeyCode::PageDown => {
                if let Some(v) = view {
                    let step = page_size.max(1) as isize;
                    match self.active_pane {
                        StatsPane::Daily => self.move_daily(step, v),
                        StatsPane::Models => self.move_model(step, v),
                    }
                }
            }
            KeyCode::Home => {
                if let Some(v) = view {
                    match self.active_pane {
                        StatsPane::Daily => self.select_daily_row(0, v),
                        StatsPane::Models => self.model_idx = 0,
                    }
                }
            }
            KeyCode::End => {
                if let Some(v) = view {
                    match self.active_pane {
                        StatsPane::Daily if !v.daily_rows.is_empty() => {
                            self.select_daily_row(v.daily_rows.len() - 1, v);
                        }
                        StatsPane::Models => {
                            let len = self.active_models(v).len();
                            if len > 0 {
                                self.model_idx = len - 1;
                            }
                        }
                        _ => {}
                    }
                }
            }
            KeyCode::Enter => {
                if let Some(v) = view {
                    match self.active_pane {
                        StatsPane::Models => self.toggle_model_filter(self.model_idx, v),
                        StatsPane::Daily => self.active_pane = StatsPane::Models,
                    }
                }
            }
            _ => {}
        }

        StatsAction::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::metric::MetricValue;
    use crate::stats::query::{DailyRow, DailySort, DayDetail};
    use crate::stats::PeriodSummary;

    fn dummy_view() -> StatsView {
        let model_a = ModelStats {
            model: "Model A".to_string(),
            requests: 2,
            total_tokens: 1_000,
            ..Default::default()
        };
        let model_b = ModelStats {
            model: "Model B".to_string(),
            requests: 1,
            total_tokens: 500,
            ..Default::default()
        };
        StatsView {
            summary: PeriodSummary::default(),
            total_value: MetricValue::Count(1_500),
            active_days: 2,
            span_days: 2,
            peak_day: None,
            median_tok_per_sec: None,
            median_ttft_secs: None,
            daily_rows: vec![
                DailyRow {
                    day_idx: 2,
                    date: "2026-11-20".to_string(),
                    weekday: "Fri",
                    summary: PeriodSummary::default(),
                    metric_value: MetricValue::Count(1_000),
                    median_tok_per_sec: None,
                    median_ttft_secs: None,
                },
                DailyRow {
                    day_idx: 1,
                    date: "2026-11-19".to_string(),
                    weekday: "Thu",
                    summary: PeriodSummary::default(),
                    metric_value: MetricValue::Count(500),
                    median_tok_per_sec: None,
                    median_ttft_secs: None,
                },
            ],
            daily_max: 1_000.0,
            heatmap_weeks: Vec::new(),
            heatmap_peak: None,
            window_models: vec![model_a.clone(), model_b.clone()],
            selected_day: Some(DayDetail {
                date: "2026-11-20".to_string(),
                weekday: "Fri",
                summary: PeriodSummary::default(),
                metric_value: MetricValue::Count(1_000),
                median_tok_per_sec: None,
                median_ttft_secs: None,
                models: vec![model_a, model_b],
            }),
        }
    }

    #[test]
    fn keyboard_controls_switch_metrics_range_sort_and_model_filter() {
        let view = dummy_view();
        let mut st = StatsTuiState::new(Some(10), StatsMetric::Total);
        st.loading = false;
        st.sync_with_view(&view);

        // Custom 10d inserted between 7d and 14d
        assert_eq!(
            st.range_options,
            vec![
                RangePreset::Days(7),
                RangePreset::Days(10),
                RangePreset::Days(14),
                RangePreset::Days(30),
                RangePreset::Days(90),
                RangePreset::All,
            ]
        );
        assert_eq!(st.query.range, RangePreset::Days(10));

        // 2 -> Cost ($), 4 -> Output, ] -> Turns
        st.handle_key(KeyCode::Char('2'), KeyModifiers::NONE, Some(&view), 10, 36);
        assert_eq!(st.query.metric, StatsMetric::Cost);
        st.handle_key(KeyCode::Char('4'), KeyModifiers::NONE, Some(&view), 10, 36);
        assert_eq!(st.query.metric, StatsMetric::Output);
        st.handle_key(KeyCode::Char(']'), KeyModifiers::NONE, Some(&view), 10, 36);
        assert_eq!(st.query.metric, StatsMetric::Turns);

        // 's' cycles DateDesc -> DateAsc -> MetricDesc -> CacheHitAsc -> DateDesc
        st.handle_key(KeyCode::Char('s'), KeyModifiers::NONE, Some(&view), 10, 36);
        assert_eq!(st.query.sort, DailySort::DateAsc);
        st.handle_key(KeyCode::Char('s'), KeyModifiers::NONE, Some(&view), 10, 36);
        assert_eq!(st.query.sort, DailySort::MetricDesc);
        st.handle_key(KeyCode::Char('s'), KeyModifiers::NONE, Some(&view), 10, 36);
        assert_eq!(st.query.sort, DailySort::CacheHitAsc);
        st.handle_key(KeyCode::Char('s'), KeyModifiers::NONE, Some(&view), 10, 36);
        assert_eq!(st.query.sort, DailySort::DateDesc);

        // Down selects 2026-11-19
        st.handle_key(KeyCode::Down, KeyModifiers::NONE, Some(&view), 10, 36);
        assert_eq!(st.daily_idx, 1);
        assert_eq!(st.selected_date.as_deref(), Some("2026-11-19"));

        // Tab -> Models -> Enter cross-filters Model A -> Esc clears filter instead of quitting
        st.handle_key(KeyCode::Tab, KeyModifiers::NONE, Some(&view), 10, 36);
        assert_eq!(st.active_pane, StatsPane::Models);
        st.handle_key(KeyCode::Enter, KeyModifiers::NONE, Some(&view), 10, 36);
        assert_eq!(st.query.model_filter.as_deref(), Some("Model A"));
        assert_eq!(
            st.handle_key(KeyCode::Esc, KeyModifiers::NONE, Some(&view), 10, 36),
            StatsAction::Continue
        );
        assert_eq!(st.query.model_filter, None);
        assert_eq!(
            st.handle_key(KeyCode::Esc, KeyModifiers::NONE, Some(&view), 10, 36),
            StatsAction::Quit
        );
    }
}
