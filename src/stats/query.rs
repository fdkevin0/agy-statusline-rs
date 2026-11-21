//! In-memory dataset and dynamic multi-metric query engine for `agy-statusline stats`.
//!
//! Scans local SQLite/transcript sources once into [`StatsDataset`], then serves
//! both backward-compatible [`StatsReport`] generation and sub-millisecond
//! interactive TUI queries across arbitrary metrics, time ranges, sort orders,
//! sources, and model filters.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use super::archive::scan_sqlite_conversations_with_archive;
use super::metric::{format_pct, MetricValue, StatsMetric};
use super::pricing::{fetch_models_dev_via_curl, round_usd_6, PricingCatalog, SyncMode, DEFAULT_PRICING_TTL_SECS};
use super::scan::{scan_transcripts, UsageEvent, UsageSource};
use super::tz::{format_day, unix_secs_to_utc_date, TimeZoneSpec};
use super::{
    accumulate_period, activity_week_mondays, build_weekly_usage, DailyUsage, ModelAccumulator,
    ModelStats, PeriodSummary, SqliteStats, StatsReport, WindowSummaries, DAYS_PER_WEEK,
};
use crate::db::compute_median;
use crate::paths::{resolve_agy_cli_dir, xdg_cache_dir, xdg_data_dir};
use crate::timeutil::civil_date_from_unix_day;

pub(super) const WEEKDAY_NAMES: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
/// Upper bound on calendar gap-fill span to prevent pathological allocations on huge `--days` or outlier timestamps.
const MAX_GAP_FILL_DAYS: i64 = 1096;

pub(super) fn weekday_name(day_idx: i64) -> &'static str {
    let idx = civil_date_from_unix_day(day_idx)
        .weekday()
        .to_monday_zero_offset() as usize;
    WEEKDAY_NAMES[idx]
}

/// Time window preset for interactive stats queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RangePreset {
    /// Rolling last `N` local calendar days including today (`N >= 1`).
    Days(u32),
    /// All recorded history.
    All,
}

impl RangePreset {
    /// Standard time window options in display order.
    pub const STANDARD: &'static [Self] = &[
        Self::Days(7),
        Self::Days(14),
        Self::Days(30),
        Self::Days(90),
        Self::All,
    ];

    /// Builds the ordered range options list, inserting a custom `--days N` in ascending order when needed.
    pub fn options_with_custom(custom_days: Option<u32>) -> Vec<Self> {
        let mut opts = Self::STANDARD.to_vec();
        if let Some(d) = custom_days.filter(|&d| d > 0) {
            let preset = Self::Days(d);
            if !opts.contains(&preset) {
                let pos = opts
                    .iter()
                    .position(|o| match o {
                        Self::Days(existing) => d < *existing,
                        Self::All => true,
                    })
                    .unwrap_or(opts.len());
                opts.insert(pos, preset);
            }
        }
        opts
    }

    /// Short pill label (e.g. `7d`, `30d`, `All`).
    pub fn label(self) -> String {
        match self {
            Self::Days(d) => format!("{d}d"),
            Self::All => "All".to_string(),
        }
    }

    /// Inclusive lower bound day index (`None` for `All`).
    pub fn cutoff_day(self, today_day_idx: i64) -> Option<i64> {
        match self {
            Self::Days(d) if d > 0 => Some(today_day_idx - i64::from(d - 1)),
            _ => None,
        }
    }
}

/// Sort order for the daily usage list.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DailySort {
    /// Newest date first (default).
    #[default]
    DateDesc,
    /// Chronological order (oldest first).
    DateAsc,
    /// Highest value under the active [`StatsMetric`] first.
    MetricDesc,
    /// Lowest value under the active [`StatsMetric`] first.
    MetricAsc,
}

impl DailySort {
    pub const fn next(self) -> Self {
        match self {
            Self::DateDesc => Self::DateAsc,
            Self::DateAsc => Self::MetricDesc,
            Self::MetricDesc => Self::MetricAsc,
            Self::MetricAsc => Self::DateDesc,
        }
    }

    pub const fn label(self, ascii: bool) -> &'static str {
        match (self, ascii) {
            (Self::DateDesc, false) => "Date ↓",
            (Self::DateAsc, false) => "Date ↑",
            (Self::MetricDesc, false) => "Value ↓",
            (Self::MetricAsc, false) => "Value ↑",
            (Self::DateDesc, true) => "Date desc",
            (Self::DateAsc, true) => "Date asc",
            (Self::MetricDesc, true) => "Value desc",
            (Self::MetricAsc, true) => "Value asc",
        }
    }
}

/// Event origin filter (`All`, `CLI`, or `Desktop`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SourceFilter {
    #[default]
    All,
    Cli,
    Desktop,
}

impl SourceFilter {
    pub const fn next(self) -> Self {
        match self {
            Self::All => Self::Cli,
            Self::Cli => Self::Desktop,
            Self::Desktop => Self::All,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Cli => "CLI",
            Self::Desktop => "Desk",
        }
    }

    pub(super) const fn matches(self, source: UsageSource) -> bool {
        match self {
            Self::All => true,
            Self::Cli => matches!(source, UsageSource::Cli),
            Self::Desktop => matches!(source, UsageSource::Desktop),
        }
    }
}

/// Scope displayed in the TUI Models pane.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ModelScope {
    /// Models used on the currently highlighted date.
    #[default]
    SelectedDay,
    /// Models aggregated across the active time window.
    Window,
}

impl ModelScope {
    pub const fn toggle(self) -> Self {
        match self {
            Self::SelectedDay => Self::Window,
            Self::Window => Self::SelectedDay,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::SelectedDay => "Day",
            Self::Window => "Window",
        }
    }
}

/// Active interactive query parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatsQuery {
    pub range: RangePreset,
    pub metric: StatsMetric,
    pub sort: DailySort,
    pub model_filter: Option<String>,
    pub source_filter: SourceFilter,
    pub fill_empty_days: bool,
    pub model_scope: ModelScope,
}

impl Default for StatsQuery {
    fn default() -> Self {
        Self {
            range: RangePreset::All,
            metric: StatsMetric::Total,
            sort: DailySort::DateDesc,
            model_filter: None,
            source_filter: SourceFilter::All,
            fill_empty_days: false,
            model_scope: ModelScope::SelectedDay,
        }
    }
}

/// Single day row rendered in the Daily Usage table.
#[derive(Debug, Clone, PartialEq)]
pub struct DailyRow {
    pub day_idx: i64,
    pub date: String,
    pub weekday: &'static str,
    pub summary: PeriodSummary,
    pub metric_value: MetricValue,
    pub median_tok_per_sec: Option<f64>,
    pub median_ttft_secs: Option<f64>,
}

/// One Monday–Sunday week of metric-aware 20-week activity heatmap cells.
#[derive(Debug, Clone, PartialEq)]
pub struct HeatmapWeek {
    pub week_start: String,
    pub week_end: String,
    /// `(YYYY-MM-DD, MetricValue)` for Mon (`0`)..Sun (`6`); `None` for future days after `today_local`.
    pub daily: [Option<(String, MetricValue)>; 7],
}

/// Detailed breakdown for the currently selected date.
#[derive(Debug, Clone, PartialEq)]
pub struct DayDetail {
    pub date: String,
    pub weekday: &'static str,
    pub summary: PeriodSummary,
    pub metric_value: MetricValue,
    pub median_tok_per_sec: Option<f64>,
    pub median_ttft_secs: Option<f64>,
    pub models: Vec<ModelStats>,
}

/// Computed view backing one frame of the interactive TUI (or metric-aware static heatmap).
#[derive(Debug, Clone, PartialEq)]
pub struct StatsView {
    pub summary: PeriodSummary,
    pub total_value: MetricValue,
    pub active_days: usize,
    pub span_days: usize,
    pub peak_day: Option<(String, &'static str, MetricValue)>,
    pub lowest_day: Option<(String, &'static str, MetricValue)>,
    pub median_tok_per_sec: Option<f64>,
    pub median_ttft_secs: Option<f64>,
    pub daily_rows: Vec<DailyRow>,
    pub daily_max: f64,
    pub heatmap_weeks: Vec<HeatmapWeek>,
    pub heatmap_peak: Option<(String, &'static str, MetricValue)>,
    pub window_models: Vec<ModelStats>,
    pub selected_day: Option<DayDetail>,
}

impl StatsView {
    /// Unweighted mean of daily rate percentages across days with a recorded sample.
    pub fn daily_avg_rate(&self) -> Option<f64> {
        let mut sum = 0.0_f64;
        let mut count = 0usize;
        for row in &self.daily_rows {
            if let Some(pct) = row.metric_value.ratio_pct() {
                sum += pct;
                count += 1;
            }
        }
        (count > 0).then(|| sum / (count as f64))
    }

    /// Formats the daily average for `metric` (unweighted daily mean for rates, `total / active_days` for additive metrics).
    pub fn daily_avg_display(&self, metric: StatsMetric) -> String {
        if metric.is_rate() {
            format_pct(self.daily_avg_rate())
        } else {
            metric.format_average(self.total_value, self.active_days)
        }
    }
}

/// In-memory snapshot of all locally scanned usage events and environment metadata.
#[derive(Debug, Clone)]
pub struct StatsDataset {
    pub today_day_idx: i64,
    pub today_local: String,
    pub timezone: String,
    pub today_utc: String,
    pub first_active_date: Option<String>,
    pub sqlite: SqliteStats,
    pub(super) events: Vec<UsageEvent>,
    pub has_cli: bool,
    pub has_desktop: bool,
}

#[derive(Default)]
struct DayAccumulator {
    date: String,
    summary: PeriodSummary,
    speeds: Vec<f64>,
    ttfts: Vec<f64>,
}

fn finalize_models(
    model_map: HashMap<String, ModelAccumulator>,
    sort_metric: StatsMetric,
) -> Vec<ModelStats> {
    let mut models: Vec<ModelStats> = model_map
        .into_iter()
        .map(|(model, acc)| ModelStats {
            model,
            pricing_key: acc.pricing_key,
            requests: acc.summary.requests,
            input_tokens: acc.summary.input_tokens,
            cache_read_tokens: acc.summary.cache_read_tokens,
            cache_write_tokens: acc.summary.cache_write_tokens,
            output_tokens: acc.summary.output_tokens,
            thinking_tokens: acc.summary.thinking_tokens,
            total_tokens: acc.summary.total_tokens,
            priced_usd: round_usd_6(acc.summary.priced_usd),
            unpriced_requests: acc.summary.unpriced_requests,
            speed_samples: acc.speeds.len(),
            median_tok_per_sec: compute_median(&acc.speeds),
            ttft_samples: acc.ttfts.len(),
            median_ttft_secs: compute_median(&acc.ttfts),
        })
        .collect();

    models.sort_by(|a, b| {
        sort_metric
            .extract(a)
            .cmp_desc(sort_metric.extract(b))
            .then_with(|| b.total_tokens.cmp(&a.total_tokens))
            .then_with(|| a.model.cmp(&b.model))
    });
    models
}

fn accumulate_model(model_map: &mut HashMap<String, ModelAccumulator>, ev: &UsageEvent) {
    let entry = model_map.entry(ev.model_group.clone()).or_default();
    if entry.pricing_key.is_none() {
        entry.pricing_key.clone_from(&ev.pricing_key);
    }
    accumulate_period(&mut entry.summary, ev);
    if let Some(speed) = ev.decode_speed {
        entry.speeds.push(speed);
    }
    if let Some(ttft) = ev.ttft_secs {
        entry.ttfts.push(ttft);
    }
}

fn finish_day_row(
    day_idx: i64,
    acc: Option<DayAccumulator>,
    metric: StatsMetric,
    peak_day: &mut Option<(String, &'static str, MetricValue)>,
    lowest_day: &mut Option<(String, &'static str, MetricValue)>,
) -> DailyRow {
    let wd = weekday_name(day_idx);
    match acc {
        Some(mut acc) => {
            acc.summary.priced_usd = round_usd_6(acc.summary.priced_usd);
            let val = metric.extract(&acc.summary);
            if val.has_sample() {
                if peak_day
                    .as_ref()
                    .is_none_or(|(_, _, prev)| val.cmp_desc(*prev) == Ordering::Less)
                {
                    *peak_day = Some((acc.date.clone(), wd, val));
                }
                if lowest_day
                    .as_ref()
                    .is_none_or(|(_, _, prev)| val.cmp_asc(*prev) == Ordering::Less)
                {
                    *lowest_day = Some((acc.date.clone(), wd, val));
                }
            }
            DailyRow {
                day_idx,
                date: acc.date,
                weekday: wd,
                summary: acc.summary,
                metric_value: val,
                median_tok_per_sec: compute_median(&acc.speeds),
                median_ttft_secs: compute_median(&acc.ttfts),
            }
        }
        None => {
            let zero = PeriodSummary::default();
            DailyRow {
                day_idx,
                date: format_day(day_idx),
                weekday: wd,
                metric_value: metric.extract(&zero),
                summary: zero,
                median_tok_per_sec: None,
                median_ttft_secs: None,
            }
        }
    }
}

pub(super) fn resolve_roots(home: Option<&Path>) -> Vec<(&'static str, PathBuf)> {
    let mut roots: Vec<(&'static str, PathBuf)> = Vec::new();
    if let Some(cli_dir) = resolve_agy_cli_dir()
        .or_else(|| home.map(|h| h.join(".gemini").join("antigravity-cli")))
    {
        roots.push(("cli", cli_dir));
    }
    if let Some(h) = home {
        roots.push(("desktop", h.join(".gemini").join("antigravity")));
    }
    roots
}

impl StatsDataset {
    /// Scans all AGY roots resolved from the ambient environment and `home`,
    /// incrementally syncing and loading persistent history from the XDG data archive.
    pub fn collect_from_env(home: Option<&Path>, now_unix_secs: u64) -> Self {
        let roots = resolve_roots(home);
        let cache_dir =
            xdg_cache_dir().or_else(|| home.map(|h| h.join(".cache").join("agy-statusline")));
        let data_dir = xdg_data_dir()
            .or_else(|| home.map(|h| h.join(".local").join("share").join("agy-statusline")));
        let pricing = PricingCatalog::load_effective_with(
            cache_dir.as_deref(),
            SyncMode::BlockingIfStale,
            now_unix_secs,
            DEFAULT_PRICING_TTL_SECS,
            fetch_models_dev_via_curl,
        );
        let tz = TimeZoneSpec::local();
        Self::collect_for_roots_with_archive(
            &roots,
            data_dir.as_deref(),
            cache_dir.as_deref(),
            now_unix_secs,
            &tz,
            &pricing,
        )
    }

    /// Scans explicit roots with optional persistent archive sync, timezone, and pricing catalog.
    pub(super) fn collect_for_roots_with_archive(
        roots: &[(&'static str, PathBuf)],
        data_dir: Option<&Path>,
        cache_dir: Option<&Path>,
        now_unix_secs: u64,
        tz: &TimeZoneSpec,
        pricing: &PricingCatalog,
    ) -> Self {
        let now_i64 = now_unix_secs as i64;
        let (today_day_idx, today_local) = tz.local_day_and_date(now_i64);
        let timezone = tz.format_offset_at(now_i64);
        let today_utc = unix_secs_to_utc_date(now_unix_secs);

        let (sqlite, mut events, covered) = scan_sqlite_conversations_with_archive(
            roots,
            data_dir,
            cache_dir,
            now_unix_secs,
            tz,
            pricing,
        );
        let fallback_events = scan_transcripts(roots, tz, pricing, &covered);
        events.extend(fallback_events);

        let mut first_day: Option<i64> = None;
        let mut has_cli = false;
        let mut has_desktop = false;
        for ev in &events {
            first_day = Some(match first_day {
                Some(prev) => prev.min(ev.local_day),
                None => ev.local_day,
            });
            match ev.source {
                UsageSource::Cli => has_cli = true,
                UsageSource::Desktop => has_desktop = true,
            }
        }

        Self {
            today_day_idx,
            today_local,
            timezone,
            today_utc,
            first_active_date: first_day.map(format_day),
            sqlite,
            events,
            has_cli,
            has_desktop,
        }
    }

    /// Builds a backward-compatible [`StatsReport`] for `--json` and static report rendering.
    pub fn to_report(&self, days: Option<u32>) -> StatsReport {
        self.to_report_with_metric(days, StatsMetric::Total)
    }

    /// Builds a [`StatsReport`] whose `models` leaderboard is sorted by `sort_metric`.
    pub fn to_report_with_metric(&self, days: Option<u32>, sort_metric: StatsMetric) -> StatsReport {
        let filter_days = days.filter(|&d| d > 0);
        let filter_cutoff = filter_days.map(|d| self.today_day_idx - i64::from(d - 1));

        let mut windows = WindowSummaries::default();
        let mut totals = PeriodSummary::default();
        let mut daily_map: BTreeMap<i64, (String, PeriodSummary)> = BTreeMap::new();
        let mut unfiltered_daily_tokens: BTreeMap<String, u64> = BTreeMap::new();
        let mut model_map: HashMap<String, ModelAccumulator> = HashMap::new();

        for ev in &self.events {
            let day_tok = unfiltered_daily_tokens
                .entry(ev.local_date.clone())
                .or_insert(0);
            *day_tok = day_tok.saturating_add(ev.total_tokens);

            accumulate_period(&mut windows.all_time, ev);
            if ev.local_day == self.today_day_idx {
                accumulate_period(&mut windows.today, ev);
            }
            if ev.local_day >= self.today_day_idx - 6 && ev.local_day <= self.today_day_idx {
                accumulate_period(&mut windows.last_7d, ev);
            }
            if ev.local_day >= self.today_day_idx - 29 && ev.local_day <= self.today_day_idx {
                accumulate_period(&mut windows.last_30d, ev);
            }

            let in_filter = filter_cutoff
                .is_none_or(|cutoff| ev.local_day >= cutoff && ev.local_day <= self.today_day_idx);
            if !in_filter {
                continue;
            }

            accumulate_period(&mut totals, ev);
            let (_, day_summary) = daily_map
                .entry(ev.local_day)
                .or_insert_with(|| (ev.local_date.clone(), PeriodSummary::default()));
            accumulate_period(day_summary, ev);
            accumulate_model(&mut model_map, ev);
        }

        for summary in [
            &mut windows.today,
            &mut windows.last_7d,
            &mut windows.last_30d,
            &mut windows.all_time,
            &mut totals,
        ] {
            summary.priced_usd = round_usd_6(summary.priced_usd);
        }

        let daily: Vec<DailyUsage> = daily_map
            .into_values()
            .map(|(date, s)| DailyUsage {
                date,
                requests: s.requests,
                input_tokens: s.input_tokens,
                cache_read_tokens: s.cache_read_tokens,
                cache_write_tokens: s.cache_write_tokens,
                output_tokens: s.output_tokens,
                thinking_tokens: s.thinking_tokens,
                total_tokens: s.total_tokens,
                priced_usd: round_usd_6(s.priced_usd),
                unpriced_requests: s.unpriced_requests,
            })
            .collect();

        let models = finalize_models(model_map, sort_metric);
        let unfiltered_lookup: HashMap<&str, u64> = unfiltered_daily_tokens
            .iter()
            .map(|(date, tokens)| (date.as_str(), *tokens))
            .collect();
        let weekly = build_weekly_usage(self.today_day_idx, &unfiltered_lookup);

        StatsReport {
            today_local: self.today_local.clone(),
            timezone: self.timezone.clone(),
            today_utc: self.today_utc.clone(),
            filter_days,
            first_active_date: self.first_active_date.clone(),
            windows,
            totals,
            daily,
            weekly,
            models,
            sqlite: self.sqlite.clone(),
        }
    }

    /// Computes a metric-aware 20-week heatmap for the active `source_filter` and `model_filter`.
    pub fn build_metric_heatmap(
        &self,
        metric: StatsMetric,
        source_filter: SourceFilter,
        model_filter: Option<&str>,
    ) -> (Vec<HeatmapWeek>, Option<(String, &'static str, MetricValue)>) {
        let mut by_day: HashMap<i64, PeriodSummary> = HashMap::new();
        for ev in &self.events {
            if !source_filter.matches(ev.source) {
                continue;
            }
            if let Some(target_model) = model_filter {
                if ev.model_group != target_model {
                    continue;
                }
            }
            let entry = by_day.entry(ev.local_day).or_default();
            accumulate_period(entry, ev);
        }
        for s in by_day.values_mut() {
            s.priced_usd = round_usd_6(s.priced_usd);
        }

        let zero_val = metric.extract(&PeriodSummary::default());
        let mut peak: Option<(String, &'static str, MetricValue)> = None;

        let weeks = activity_week_mondays(self.today_day_idx)
            .map(|week_mon| {
                let mut daily: [Option<(String, MetricValue)>; 7] = std::array::from_fn(|_| None);
                for (d, slot) in daily.iter_mut().enumerate() {
                    let day_idx = week_mon + (d as i64);
                    if day_idx <= self.today_day_idx {
                        let date = format_day(day_idx);
                        let val = by_day
                            .get(&day_idx)
                            .map_or(zero_val, |s| metric.extract(s));
                        if val.has_sample()
                            && peak
                                .as_ref()
                                .is_none_or(|(_, _, prev)| val.cmp_desc(*prev) == Ordering::Less)
                        {
                            peak = Some((date.clone(), WEEKDAY_NAMES[d], val));
                        }
                        *slot = Some((date, val));
                    }
                }
                HeatmapWeek {
                    week_start: format_day(week_mon),
                    week_end: format_day(week_mon + DAYS_PER_WEEK - 1),
                    daily,
                }
            })
            .collect();

        (weeks, peak)
    }

    /// Evaluates an interactive [`StatsQuery`] in memory without re-scanning disk.
    pub fn compute_view(&self, query: &StatsQuery, selected_date: Option<&str>) -> StatsView {
        let cutoff = query.range.cutoff_day(self.today_day_idx);

        let mut summary = PeriodSummary::default();
        let mut window_speeds = Vec::new();
        let mut window_ttfts = Vec::new();
        let mut day_map: BTreeMap<i64, DayAccumulator> = BTreeMap::new();
        let mut window_model_map: HashMap<String, ModelAccumulator> = HashMap::new();

        for ev in &self.events {
            if !query.source_filter.matches(ev.source) {
                continue;
            }
            let in_range =
                cutoff.is_none_or(|c| ev.local_day >= c && ev.local_day <= self.today_day_idx);
            if !in_range {
                continue;
            }

            // Keep full window model list across all models so users can switch/clear filters freely.
            accumulate_model(&mut window_model_map, ev);

            if let Some(target) = query.model_filter.as_deref() {
                if ev.model_group != target {
                    continue;
                }
            }

            accumulate_period(&mut summary, ev);
            if let Some(speed) = ev.decode_speed {
                window_speeds.push(speed);
            }
            if let Some(ttft) = ev.ttft_secs {
                window_ttfts.push(ttft);
            }

            let day_acc = day_map.entry(ev.local_day).or_insert_with(|| DayAccumulator {
                date: ev.local_date.clone(),
                ..Default::default()
            });
            accumulate_period(&mut day_acc.summary, ev);
            if let Some(speed) = ev.decode_speed {
                day_acc.speeds.push(speed);
            }
            if let Some(ttft) = ev.ttft_secs {
                day_acc.ttfts.push(ttft);
            }
        }

        summary.priced_usd = round_usd_6(summary.priced_usd);
        let total_value = query.metric.extract(&summary);
        let active_days = day_map.len();
        let span_days = match query.range {
            RangePreset::Days(n) => n as usize,
            RangePreset::All => day_map
                .keys()
                .next()
                .map(|&first| (self.today_day_idx - first + 1).max(1) as usize)
                .unwrap_or(0),
        };

        let mut peak_day: Option<(String, &'static str, MetricValue)> = None;
        let mut lowest_day: Option<(String, &'static str, MetricValue)> = None;
        let mut daily_rows: Vec<DailyRow> = if query.fill_empty_days {
            cutoff
                .or_else(|| day_map.keys().next().copied())
                .map(|raw_start| {
                    let end = self.today_day_idx.max(
                        day_map
                            .keys()
                            .next_back()
                            .copied()
                            .unwrap_or(self.today_day_idx),
                    );
                    let start = raw_start.max(end.saturating_sub(MAX_GAP_FILL_DAYS - 1));
                    (start..=end)
                        .map(|d| {
                            finish_day_row(
                                d,
                                day_map.remove(&d),
                                query.metric,
                                &mut peak_day,
                                &mut lowest_day,
                            )
                        })
                        .collect()
                })
                .unwrap_or_default()
        } else {
            day_map
                .into_iter()
                .map(|(d, acc)| {
                    finish_day_row(d, Some(acc), query.metric, &mut peak_day, &mut lowest_day)
                })
                .collect()
        };

        match query.sort {
            DailySort::DateDesc => daily_rows.sort_by_key(|a| std::cmp::Reverse(a.day_idx)),
            DailySort::DateAsc => daily_rows.sort_by_key(|a| a.day_idx),
            DailySort::MetricDesc => daily_rows.sort_by(|a, b| {
                a.metric_value
                    .cmp_desc(b.metric_value)
                    .then_with(|| b.day_idx.cmp(&a.day_idx))
            }),
            DailySort::MetricAsc => daily_rows.sort_by(|a, b| {
                // Sink empty gap-filled days below active days consistently across all metrics.
                (a.summary.requests == 0)
                    .cmp(&(b.summary.requests == 0))
                    .then_with(|| a.metric_value.cmp_asc(b.metric_value))
                    .then_with(|| b.day_idx.cmp(&a.day_idx))
            }),
        }

        let daily_max = daily_rows
            .iter()
            .map(|r| r.metric_value.magnitude())
            .fold(0.0_f64, f64::max);

        let window_models = finalize_models(window_model_map, query.metric);
        let (heatmap_weeks, heatmap_peak) = self.build_metric_heatmap(
            query.metric,
            query.source_filter,
            query.model_filter.as_deref(),
        );

        let selected_row = selected_date
            .and_then(|d| daily_rows.iter().find(|r| r.date == d))
            .or_else(|| daily_rows.first());

        let selected_day = selected_row.map(|row| {
            let mut day_models: HashMap<String, ModelAccumulator> = HashMap::new();
            for ev in &self.events {
                if ev.local_day == row.day_idx && query.source_filter.matches(ev.source) {
                    accumulate_model(&mut day_models, ev);
                }
            }
            DayDetail {
                date: row.date.clone(),
                weekday: row.weekday,
                summary: row.summary.clone(),
                metric_value: row.metric_value,
                median_tok_per_sec: row.median_tok_per_sec,
                median_ttft_secs: row.median_ttft_secs,
                models: finalize_models(day_models, query.metric),
            }
        });

        StatsView {
            summary,
            total_value,
            active_days,
            span_days,
            peak_day,
            lowest_day,
            median_tok_per_sec: compute_median(&window_speeds),
            median_ttft_secs: compute_median(&window_ttfts),
            daily_rows,
            daily_max,
            heatmap_weeks,
            heatmap_peak,
            window_models,
            selected_day,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::metric::TokenMask;
    use crate::timeutil::unix_day_from_civil_date;

    fn sample_dataset() -> StatsDataset {
        let d1 = unix_day_from_civil_date("2026-11-18".parse().unwrap());
        let d2 = unix_day_from_civil_date("2026-11-19".parse().unwrap());
        let d3 = unix_day_from_civil_date("2026-11-20".parse().unwrap());

        StatsDataset {
            today_day_idx: d3,
            today_local: "2026-11-20".to_string(),
            timezone: "UTC+08:00".to_string(),
            today_utc: "2026-11-20".to_string(),
            first_active_date: Some("2026-11-18".to_string()),
            sqlite: SqliteStats::default(),
            has_cli: true,
            has_desktop: true,
            events: vec![
                // Day 1 (CLI): huge cache read, low USD
                UsageEvent {
                    source: UsageSource::Cli,
                    local_day: d1,
                    local_date: "2026-11-18".to_string(),
                    model_group: "Model Fast".to_string(),
                    pricing_key: Some("model-fast".to_string()),
                    input_tokens: 1_000,
                    cache_read_tokens: 90_000,
                    cache_write_tokens: 500,
                    output_tokens: 500,
                    thinking_tokens: 100,
                    total_tokens: 92_000,
                    cost_usd: Some(0.10),
                    decode_speed: Some(200.0),
                    ttft_secs: Some(0.5),
                },
                // Day 2 (CLI): smaller total tokens, higher USD & output
                UsageEvent {
                    source: UsageSource::Cli,
                    local_day: d2,
                    local_date: "2026-11-19".to_string(),
                    model_group: "Model Pro".to_string(),
                    pricing_key: Some("model-pro".to_string()),
                    input_tokens: 5_000,
                    cache_read_tokens: 10_000,
                    cache_write_tokens: 2_000,
                    output_tokens: 4_000,
                    thinking_tokens: 3_000,
                    total_tokens: 21_000,
                    cost_usd: Some(2.50),
                    decode_speed: Some(80.0),
                    ttft_secs: Some(1.5),
                },
                // Day 3 (Desktop): small usage
                UsageEvent {
                    source: UsageSource::Desktop,
                    local_day: d3,
                    local_date: "2026-11-20".to_string(),
                    model_group: "Model Fast".to_string(),
                    pricing_key: Some("model-fast".to_string()),
                    input_tokens: 2_000,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                    output_tokens: 200,
                    thinking_tokens: 0,
                    total_tokens: 2_200,
                    cost_usd: Some(0.20),
                    decode_speed: Some(150.0),
                    ttft_secs: Some(0.8),
                },
            ],
        }
    }

    #[test]
    fn compute_view_switches_metrics_sorting_heatmap_and_filters() {
        let ds = sample_dataset();

        // Total tokens: 2026-11-18 (92K) is peak, Model Fast ranks #1.
        let q_total = StatsQuery {
            metric: StatsMetric::Total,
            sort: DailySort::MetricDesc,
            ..Default::default()
        };
        let v_total = ds.compute_view(&q_total, None);
        assert_eq!(v_total.daily_rows[0].date, "2026-11-18");
        assert_eq!(v_total.heatmap_peak.as_ref().unwrap().0, "2026-11-18");
        assert_eq!(v_total.window_models[0].model, "Model Fast");

        // Cost ($): 2026-11-19 ($2.50) flips to #1 in both daily list, heatmap, and model ranking.
        let q_cost = StatsQuery {
            metric: StatsMetric::Cost,
            sort: DailySort::MetricDesc,
            ..Default::default()
        };
        let v_cost = ds.compute_view(&q_cost, None);
        assert_eq!(v_cost.daily_rows[0].date, "2026-11-19");
        assert_eq!(v_cost.heatmap_peak.as_ref().unwrap().0, "2026-11-19");
        assert_eq!(v_cost.window_models[0].model, "Model Pro");

        // Custom token mask: Cache Write only -> 2026-11-19 (2,000) > 2026-11-18 (500) > 2026-11-20 (0)
        let q_cw = StatsQuery {
            metric: StatsMetric::Custom(TokenMask::CACHE_WRITE),
            sort: DailySort::MetricDesc,
            ..Default::default()
        };
        let v_cw = ds.compute_view(&q_cw, None);
        assert_eq!(v_cw.daily_rows[0].date, "2026-11-19");
        assert_eq!(v_cw.daily_rows[0].metric_value, MetricValue::Count(2_000));
        assert_eq!(v_cw.total_value, MetricValue::Count(2_500));

        // Source filter Desktop isolates 2026-11-20 only.
        let q_desk = StatsQuery {
            source_filter: SourceFilter::Desktop,
            ..Default::default()
        };
        let v_desk = ds.compute_view(&q_desk, None);
        assert_eq!(v_desk.daily_rows.len(), 1);
        assert_eq!(v_desk.daily_rows[0].date, "2026-11-20");

        // Model filter isolates Model Pro (2026-11-19) while keeping both models visible in window_models.
        let q_model = StatsQuery {
            model_filter: Some("Model Pro".to_string()),
            ..Default::default()
        };
        let v_model = ds.compute_view(&q_model, None);
        assert_eq!(v_model.daily_rows.len(), 1);
        assert_eq!(v_model.daily_rows[0].date, "2026-11-19");
        assert_eq!(v_model.window_models.len(), 2);

        // Fill empty days over 7d emits 7 consecutive days.
        let q_fill = StatsQuery {
            range: RangePreset::Days(7),
            fill_empty_days: true,
            ..Default::default()
        };
        let v_fill = ds.compute_view(&q_fill, None);
        assert_eq!(v_fill.daily_rows.len(), 7);
        assert_eq!(v_fill.active_days, 3);
        assert_eq!(v_fill.daily_rows[0].date, "2026-11-20");
        assert_eq!(v_fill.daily_rows[6].date, "2026-11-14");

        // Huge range with fill_empty_days is bounded to MAX_GAP_FILL_DAYS.
        let q_huge = StatsQuery {
            range: RangePreset::Days(1_000_000),
            fill_empty_days: true,
            ..Default::default()
        };
        let v_huge = ds.compute_view(&q_huge, None);
        assert_eq!(v_huge.daily_rows.len(), MAX_GAP_FILL_DAYS as usize);
        assert_eq!(v_huge.active_days, 3);

        // CacheHit metric: Day 1 = 90_000 / 91_500 = 98.4%, Day 2 = 10_000 / 17_000 = 58.8%, Day 3 = 0 / 2_000 = 0.0%
        let q_hit_asc = StatsQuery {
            range: RangePreset::Days(5),
            metric: StatsMetric::CacheHit,
            sort: DailySort::MetricAsc,
            fill_empty_days: true,
            ..Default::default()
        };
        let v_hit = ds.compute_view(&q_hit_asc, None);
        // Active days sort ascending (0.0% -> 58.8% -> 98.4%), while empty gap days (`-`) sink to the bottom.
        assert_eq!(v_hit.daily_rows[0].date, "2026-11-20");
        assert_eq!(v_hit.daily_rows[1].date, "2026-11-19");
        assert_eq!(v_hit.daily_rows[2].date, "2026-11-18");
        assert_eq!(v_hit.daily_rows[3].metric_value, MetricValue::Ratio(None));
        assert_eq!(v_hit.daily_rows[4].metric_value, MetricValue::Ratio(None));
        assert_eq!(v_hit.peak_day.as_ref().unwrap().0, "2026-11-18");
        assert_eq!(v_hit.lowest_day.as_ref().unwrap().0, "2026-11-20");
        // Weighted overall: 100_000 / 110_500 = 90.5%; unweighted daily avg: (98.36 + 58.82 + 0.0) / 3 = 52.4%
        assert_eq!(
            StatsMetric::CacheHit.format_display(v_hit.total_value, true),
            "90.5%"
        );
        assert_eq!(v_hit.daily_avg_display(StatsMetric::CacheHit), "52.4%");

        // MetricAsc also sinks empty gap-filled days below active days for additive token metrics.
        let q_tot_asc = StatsQuery {
            range: RangePreset::Days(5),
            metric: StatsMetric::Total,
            sort: DailySort::MetricAsc,
            fill_empty_days: true,
            ..Default::default()
        };
        let v_tot_asc = ds.compute_view(&q_tot_asc, None);
        assert_eq!(v_tot_asc.daily_rows[0].date, "2026-11-20");
        assert_eq!(v_tot_asc.daily_rows[1].date, "2026-11-19");
        assert_eq!(v_tot_asc.daily_rows[2].date, "2026-11-18");
        assert_eq!(v_tot_asc.daily_rows[3].summary.requests, 0);
        assert_eq!(v_tot_asc.daily_rows[4].summary.requests, 0);
    }
}
