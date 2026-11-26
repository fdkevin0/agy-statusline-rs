//! In-memory dataset and dynamic multi-metric query engine for `agy-statusline stats`.
//!
//! Scans local SQLite conversation DBs once into [`StatsDataset`], then serves
//! [`StatsReport`] generation and sub-millisecond interactive TUI queries across
//! arbitrary metrics, time ranges, sort orders, sources, and model filters.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

use super::metric::{MetricValue, StatsMetric, UsageCounters};
use super::pricing::{
    fetch_models_dev_via_curl, round_usd_6, PricingCatalog, SyncMode, DEFAULT_PRICING_TTL_SECS,
};
use super::scan::{
    scan_sqlite_conversations_with_cache, ScanDiagnostics, UsageEvent, UsageSource,
    STATS_SCAN_CACHE_FILE,
};
use super::{
    accumulate_period, activity_week_mondays, build_weekly_usage, finalize_models,
    ConversationListItem, DailyUsage, ModelAccumulator, ModelStats, PeriodSummary,
    QuotaCreditReport, QuotaPoolReport, QuotaWindowReport, SqliteStats, StatsQuotaReport,
    StatsReport, WindowSummaries, DAYS_PER_WEEK,
};
use crate::agy::db::compute_median;
use crate::agy::hub::{fetch_or_load_quota_snapshot, HubQuotaResult};
use crate::agy::quota::QuotaSource;
use crate::statusline::metrics::{summarize_quota_pools_with_history, CreditSeverity, QuotaView};
use crate::sys::time::{civil_date_from_unix_day, format_day, unix_secs_to_utc_date, TimeZoneSpec};

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
pub(super) enum RangePreset {
    /// Rolling last `N` local calendar days including today (`N >= 1`).
    Days(u32),
    /// All recorded history.
    All,
}

impl RangePreset {
    /// Standard time window options in display order.
    pub(super) const STANDARD: &'static [Self] = &[
        Self::Days(7),
        Self::Days(14),
        Self::Days(30),
        Self::Days(90),
        Self::All,
    ];

    /// Builds the ordered range options list, inserting a custom `--days N` in ascending order when needed.
    pub(super) fn options_with_custom(custom_days: Option<u32>) -> Vec<Self> {
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
    pub(super) fn label(self) -> String {
        match self {
            Self::Days(d) => format!("{d}d"),
            Self::All => "All".to_string(),
        }
    }

    /// Inclusive lower bound day index (`None` for `All`).
    pub(super) fn cutoff_day(self, today_day_idx: i64) -> Option<i64> {
        match self {
            Self::Days(d) if d > 0 => Some(today_day_idx - i64::from(d - 1)),
            _ => None,
        }
    }
}

/// Sort order for the daily usage list.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum DailySort {
    /// Newest date first (default).
    #[default]
    DateDesc,
    /// Chronological order (oldest first).
    DateAsc,
    /// Highest value under the active [`StatsMetric`] first.
    MetricDesc,
    /// Lowest prompt cache hit rate first (days without prompt tokens sink to the bottom).
    CacheHitAsc,
}

impl DailySort {
    pub(super) const fn next(self) -> Self {
        match self {
            Self::DateDesc => Self::DateAsc,
            Self::DateAsc => Self::MetricDesc,
            Self::MetricDesc => Self::CacheHitAsc,
            Self::CacheHitAsc => Self::DateDesc,
        }
    }

    pub(super) const fn label(self, ascii: bool) -> &'static str {
        match (self, ascii) {
            (Self::DateDesc, false) => "Date ↓",
            (Self::DateAsc, false) => "Date ↑",
            (Self::MetricDesc, false) => "Value ↓",
            (Self::CacheHitAsc, false) => "Cache% ↑",
            (Self::DateDesc, true) => "Date desc",
            (Self::DateAsc, true) => "Date asc",
            (Self::MetricDesc, true) => "Value desc",
            (Self::CacheHitAsc, true) => "Cache% asc",
        }
    }
}

/// Event origin filter (`All`, `CLI`, or `Desktop`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum SourceFilter {
    #[default]
    All,
    Cli,
    Desktop,
}

impl SourceFilter {
    pub(super) const fn next(self) -> Self {
        match self {
            Self::All => Self::Cli,
            Self::Cli => Self::Desktop,
            Self::Desktop => Self::All,
        }
    }

    pub(super) const fn label(self) -> &'static str {
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
pub(super) enum ModelScope {
    /// Models used on the currently highlighted date.
    #[default]
    SelectedDay,
    /// Models aggregated across the active time window.
    Window,
}

impl ModelScope {
    pub(super) const fn toggle(self) -> Self {
        match self {
            Self::SelectedDay => Self::Window,
            Self::Window => Self::SelectedDay,
        }
    }

    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::SelectedDay => "Day",
            Self::Window => "Window",
        }
    }
}

/// Active interactive query parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StatsQuery {
    pub(super) range: RangePreset,
    pub(super) metric: StatsMetric,
    pub(super) sort: DailySort,
    pub(super) model_filter: Option<String>,
    pub(super) source_filter: SourceFilter,
    pub(super) fill_empty_days: bool,
    pub(super) model_scope: ModelScope,
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
pub(super) struct DailyRow {
    pub(super) day_idx: i64,
    pub(super) date: String,
    pub(super) weekday: &'static str,
    pub(super) summary: PeriodSummary,
    pub(super) metric_value: MetricValue,
    pub(super) median_tok_per_sec: Option<f64>,
    pub(super) median_ttft_secs: Option<f64>,
}

/// One Monday–Sunday week of metric-aware 20-week activity heatmap cells.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct HeatmapWeek {
    pub(super) week_start: String,
    pub(super) week_end: String,
    /// `(YYYY-MM-DD, MetricValue)` for Mon (`0`)..Sun (`6`); `None` for future days after `today_local`.
    pub(super) daily: [Option<(String, MetricValue)>; 7],
}

/// Detailed breakdown for the currently selected date.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct DayDetail {
    pub(super) date: String,
    pub(super) weekday: &'static str,
    pub(super) summary: PeriodSummary,
    pub(super) metric_value: MetricValue,
    pub(super) median_tok_per_sec: Option<f64>,
    pub(super) median_ttft_secs: Option<f64>,
    pub(super) models: Vec<ModelStats>,
}

/// Computed view backing one frame of the interactive TUI (or metric-aware static heatmap).
#[derive(Debug, Clone, PartialEq)]
pub(super) struct StatsView {
    pub(super) summary: PeriodSummary,
    pub(super) total_value: MetricValue,
    pub(super) active_days: usize,
    pub(super) span_days: usize,
    pub(super) peak_day: Option<(String, &'static str, MetricValue)>,
    pub(super) median_tok_per_sec: Option<f64>,
    pub(super) median_ttft_secs: Option<f64>,
    pub(super) daily_rows: Vec<DailyRow>,
    pub(super) daily_max: f64,
    pub(super) heatmap_weeks: Vec<HeatmapWeek>,
    pub(super) heatmap_peak: Option<(String, &'static str, MetricValue)>,
    pub(super) window_models: Vec<ModelStats>,
    pub(super) selected_day: Option<DayDetail>,
}

/// In-memory snapshot of all locally scanned usage events and environment metadata.
#[derive(Debug, Clone)]
pub(super) struct StatsDataset {
    pub(super) today_day_idx: i64,
    pub(super) today_local: String,
    pub(super) timezone: String,
    pub(super) today_utc: String,
    pub(super) first_active_date: Option<String>,
    pub(super) sqlite: SqliteStats,
    pub(super) quota: Option<StatsQuotaReport>,
    pub(super) diagnostics: ScanDiagnostics,
    events: Vec<UsageEvent>,
    pub(super) has_cli: bool,
    pub(super) has_desktop: bool,
}

impl StatsDataset {
    pub(super) fn turn_count(&self) -> usize {
        self.events.len()
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(in crate::stats) fn for_tests(
        today_day_idx: i64,
        today_local: impl Into<String>,
        timezone: impl Into<String>,
        today_utc: impl Into<String>,
        first_active_date: Option<String>,
        sqlite: SqliteStats,
        has_cli: bool,
        has_desktop: bool,
        events: Vec<UsageEvent>,
    ) -> Self {
        Self {
            today_day_idx,
            today_local: today_local.into(),
            timezone: timezone.into(),
            today_utc: today_utc.into(),
            first_active_date,
            sqlite,
            quota: None,
            diagnostics: ScanDiagnostics::default(),
            events,
            has_cli,
            has_desktop,
        }
    }
}

fn quota_view_to_report(v: QuotaView) -> QuotaWindowReport {
    QuotaWindowReport {
        label: v.label,
        remaining_pct: v.remaining_pct,
        remaining_amount: v.remaining_amount,
        reset_compact: v.reset_compact,
        pace_behind: v.pace_behind,
        burn_rate_pct_per_hr: v.burn_rate_pct_per_hr,
        eta_secs: v.eta_secs,
        will_exhaust_before_reset: v.will_exhaust_before_reset,
    }
}

fn credit_severity_str(severity: CreditSeverity) -> &'static str {
    match severity {
        CreditSeverity::Ok => "ok",
        CreditSeverity::Low => "low",
        CreditSeverity::Critical => "critical",
    }
}

fn build_stats_quota_report(res: HubQuotaResult, now_unix_secs: u64) -> Option<StatsQuotaReport> {
    let snap = res.snapshot;
    let summary =
        summarize_quota_pools_with_history(&snap.quota, Some(&snap.history), now_unix_secs);
    let pools = summary
        .pools
        .into_iter()
        .map(|p| QuotaPoolReport {
            name: p.name,
            short_window: p.short_window.map(quota_view_to_report),
            long_window: p.long_window.map(quota_view_to_report),
        })
        .collect();
    let credits = summary
        .credits
        .into_iter()
        .map(|c| QuotaCreditReport {
            key: c.key,
            label: c.label,
            remaining_amount: c.remaining_amount,
            reset_compact: c.reset_compact,
            severity: credit_severity_str(c.severity).to_string(),
        })
        .collect();
    let source = match snap.source {
        QuotaSource::HubRpc => "hub_rpc",
        QuotaSource::Stdin => "stdin",
    }
    .to_string();
    let report = StatsQuotaReport {
        source,
        from_cache: res.from_cache,
        updated_at: snap.updated_at,
        age_secs: now_unix_secs.saturating_sub(snap.updated_at),
        plan_tier: snap.display_plan_tier(),
        ai_credits: snap.ai_credits,
        pools,
        credits,
    };
    report.has_visible_content().then_some(report)
}

#[derive(Default)]
struct DayAccumulator {
    summary: PeriodSummary,
    speeds: Vec<f64>,
    ttfts: Vec<f64>,
}

/// Filters and fuzzy-ranks historical conversations using `nucleo-matcher` (shared by TUI and CLI).
///
/// Preserves chronological newest-first order when `query` is empty; ranks by fuzzy relevance
/// score descending (tie-breaking by `last_modified_secs` descending) when `query` is non-empty.
pub(in crate::stats) fn filter_and_rank_conversations(
    items: &[ConversationListItem],
    query: &str,
    show_subagents: bool,
    source_filter: SourceFilter,
) -> Vec<usize> {
    let trimmed = query.trim();
    let candidates = items
        .iter()
        .enumerate()
        .filter(|(_, it)| show_subagents || it.parent_conversation_id.is_none())
        .filter(|(_, it)| match source_filter {
            SourceFilter::All => true,
            SourceFilter::Cli => it.source == "cli",
            SourceFilter::Desktop => it.source == "desktop",
        });

    if trimmed.is_empty() {
        return candidates.map(|(idx, _)| idx).collect();
    }

    let pattern = Pattern::parse(trimmed, CaseMatching::Smart, Normalization::Smart);
    let mut matcher = Matcher::new(Config::DEFAULT);
    let mut utf32_buf = Vec::new();
    let mut scored: Vec<(u32, i64, usize)> = Vec::new();

    for (idx, it) in candidates {
        let mut haystack = String::with_capacity(96);
        haystack.push_str(&it.conversation_id);
        if let Some(title) = it.title.as_deref() {
            haystack.push(' ');
            haystack.push_str(title);
        }
        if let Some(ws) = it.workspace.as_deref() {
            haystack.push(' ');
            haystack.push_str(ws);
        }
        if let Some(agent) = it.agent_name.as_deref() {
            haystack.push(' ');
            haystack.push_str(agent);
        }
        haystack.push(' ');
        haystack.push_str(&it.source);

        utf32_buf.clear();
        let needle_haystack = Utf32Str::new(&haystack, &mut utf32_buf);
        if let Some(score) = pattern.score(needle_haystack, &mut matcher) {
            scored.push((score, it.last_modified_secs, idx));
        }
    }

    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| b.1.cmp(&a.1))
            .then_with(|| a.2.cmp(&b.2))
    });
    scored.into_iter().map(|(_, _, idx)| idx).collect()
}

pub(super) fn accumulate_model(model_map: &mut HashMap<String, ModelAccumulator>, ev: &UsageEvent) {
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
) -> DailyRow {
    let wd = weekday_name(day_idx);
    let date = format_day(day_idx);
    match acc {
        Some(mut acc) => {
            acc.summary.priced_usd = round_usd_6(acc.summary.priced_usd);
            let val = metric.extract(&acc.summary);
            if val.is_positive()
                && peak_day
                    .as_ref()
                    .is_none_or(|(_, _, prev)| val.magnitude() > prev.magnitude())
            {
                *peak_day = Some((date.clone(), wd, val));
            }
            DailyRow {
                day_idx,
                date,
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
                date,
                weekday: wd,
                metric_value: metric.extract(&zero),
                summary: zero,
                median_tok_per_sec: None,
                median_ttft_secs: None,
            }
        }
    }
}

impl StatsDataset {
    /// Scans the given AGY SQLite roots using the local timezone, persistent per-DB scan cache,
    /// `models.dev` pricing cache, and opportunistic loopback Hub RPC / cached quota snapshot.
    pub(super) fn collect(
        roots: &[(&'static str, PathBuf)],
        daemon_dirs: &[PathBuf],
        cache_dir: Option<&Path>,
        now_unix_secs: u64,
        force_quota_refresh: bool,
    ) -> Self {
        let pricing = PricingCatalog::load_effective_with(
            cache_dir,
            SyncMode::BlockingIfStale,
            now_unix_secs,
            DEFAULT_PRICING_TTL_SECS,
            fetch_models_dev_via_curl,
        );
        let tz = TimeZoneSpec::local();
        let scan_cache_path = cache_dir.map(|dir| dir.join(STATS_SCAN_CACHE_FILE));
        let mut ds = Self::collect_for_roots_with_cache(
            roots,
            now_unix_secs,
            &tz,
            &pricing,
            scan_cache_path.as_deref(),
        );
        if !daemon_dirs.is_empty() || cache_dir.is_some() {
            ds.quota = fetch_or_load_quota_snapshot(
                daemon_dirs,
                cache_dir,
                now_unix_secs,
                force_quota_refresh,
            )
            .and_then(|res| build_stats_quota_report(res, now_unix_secs));
        }
        ds
    }

    /// Scans explicit SQLite roots with an explicit timezone and pricing catalog (no disk scan cache).
    #[cfg(test)]
    pub(super) fn collect_for_roots(
        roots: &[(&'static str, PathBuf)],
        now_unix_secs: u64,
        tz: &TimeZoneSpec,
        pricing: &PricingCatalog,
    ) -> Self {
        Self::collect_for_roots_with_cache(roots, now_unix_secs, tz, pricing, None)
    }

    /// Scans explicit SQLite roots with an explicit timezone, pricing catalog, and optional scan cache path.
    pub(super) fn collect_for_roots_with_cache(
        roots: &[(&'static str, PathBuf)],
        now_unix_secs: u64,
        tz: &TimeZoneSpec,
        pricing: &PricingCatalog,
        scan_cache_path: Option<&Path>,
    ) -> Self {
        let now_i64 = now_unix_secs as i64;
        let today_day_idx = tz.local_day(now_i64);
        let today_local = format_day(today_day_idx);
        let timezone = tz.format_offset_at(now_i64);
        let today_utc = unix_secs_to_utc_date(now_i64);

        let scan = scan_sqlite_conversations_with_cache(roots, tz, pricing, scan_cache_path);

        let mut first_day: Option<i64> = None;
        let mut has_cli = false;
        let mut has_desktop = false;
        for ev in &scan.events {
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
            sqlite: scan.sqlite,
            quota: None,
            diagnostics: scan.diagnostics,
            events: scan.events,
            has_cli,
            has_desktop,
        }
    }

    /// Builds a [`StatsReport`] whose `models` leaderboard is sorted by `sort_metric`.
    pub(super) fn to_report_with_metric(
        &self,
        days: Option<u32>,
        sort_metric: StatsMetric,
    ) -> StatsReport {
        let filter_days = days.filter(|&d| d > 0);
        let filter_cutoff = filter_days.map(|d| self.today_day_idx - i64::from(d - 1));

        let mut windows = WindowSummaries::default();
        let mut totals = PeriodSummary::default();
        let mut daily_map: BTreeMap<i64, PeriodSummary> = BTreeMap::new();
        let mut unfiltered_daily_tokens: HashMap<i64, u64> = HashMap::new();
        let mut model_map: HashMap<String, ModelAccumulator> = HashMap::new();

        for ev in &self.events {
            let day_tok = unfiltered_daily_tokens.entry(ev.local_day).or_insert(0);
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
            let day_summary = daily_map.entry(ev.local_day).or_default();
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
            .into_iter()
            .map(|(day_idx, s)| DailyUsage {
                date: format_day(day_idx),
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
        let weekly = build_weekly_usage(self.today_day_idx, &unfiltered_daily_tokens);

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
            quota: self.quota.clone(),
            diagnostics: self.diagnostics,
        }
    }

    /// Computes a metric-aware 20-week heatmap for the active `source_filter` and `model_filter`.
    pub(super) fn build_metric_heatmap(
        &self,
        metric: StatsMetric,
        source_filter: SourceFilter,
        model_filter: Option<&str>,
    ) -> (
        Vec<HeatmapWeek>,
        Option<(String, &'static str, MetricValue)>,
    ) {
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
                        let val = by_day.get(&day_idx).map_or(zero_val, |s| metric.extract(s));
                        if val.is_positive()
                            && peak
                                .as_ref()
                                .is_none_or(|(_, _, prev)| val.magnitude() > prev.magnitude())
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
    pub(super) fn compute_view(
        &self,
        query: &StatsQuery,
        selected_date: Option<&str>,
    ) -> StatsView {
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

            let day_acc = day_map.entry(ev.local_day).or_default();
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
                        .map(|d| finish_day_row(d, day_map.remove(&d), query.metric, &mut peak_day))
                        .collect()
                })
                .unwrap_or_default()
        } else {
            day_map
                .into_iter()
                .map(|(d, acc)| finish_day_row(d, Some(acc), query.metric, &mut peak_day))
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
            DailySort::CacheHitAsc => daily_rows.sort_by(|a, b| {
                match (a.summary.cache_hit_pct(), b.summary.cache_hit_pct()) {
                    (Some(x), Some(y)) => x.total_cmp(&y),
                    // Days without prompt tokens (including empty gap rows) sink to the bottom.
                    (Some(_), None) => Ordering::Less,
                    (None, Some(_)) => Ordering::Greater,
                    (None, None) => Ordering::Equal,
                }
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
    use crate::sys::time::unix_day_from_civil_date;

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
            quota: None,
            diagnostics: ScanDiagnostics::default(),
            has_cli: true,
            has_desktop: true,
            events: vec![
                // Day 1 (CLI): huge cache read, low USD
                UsageEvent {
                    source: UsageSource::Cli,
                    local_day: d1,
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

        // Output metric: 2026-11-19 (4,000) > 2026-11-18 (500) > 2026-11-20 (200)
        let q_out = StatsQuery {
            metric: StatsMetric::Output,
            sort: DailySort::MetricDesc,
            ..Default::default()
        };
        let v_out = ds.compute_view(&q_out, None);
        assert_eq!(v_out.daily_rows[0].date, "2026-11-19");
        assert_eq!(v_out.daily_rows[0].metric_value, MetricValue::Count(4_000));
        assert_eq!(v_out.total_value, MetricValue::Count(4_700));

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

        // CacheHitAsc: Day 3 (0.0%) -> Day 2 (58.8%) -> Day 1 (98.4%), while empty gap days sink to the bottom.
        let q_hit_asc = StatsQuery {
            range: RangePreset::Days(5),
            sort: DailySort::CacheHitAsc,
            fill_empty_days: true,
            ..Default::default()
        };
        let v_hit = ds.compute_view(&q_hit_asc, None);
        assert_eq!(v_hit.daily_rows[0].date, "2026-11-20");
        assert_eq!(v_hit.daily_rows[1].date, "2026-11-19");
        assert_eq!(v_hit.daily_rows[2].date, "2026-11-18");
        assert_eq!(v_hit.daily_rows[3].summary.requests, 0);
        assert_eq!(v_hit.daily_rows[4].summary.requests, 0);
    }
}
