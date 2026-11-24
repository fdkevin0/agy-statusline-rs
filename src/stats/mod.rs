//! Local usage, estimated cost, and speed statistics (`agy-statusline stats`).
//!
//! Scans AGY local directories (`conversations/<uuid>.db`: `gen_metadata` joined with
//! `steps.metadata`) in read-only mode as the single authoritative source for turns,
//! tokens, thinking tokens, model names, timestamps, decode speed, and TTFT.

mod metric;
mod pricing;
mod query;
mod render;
mod scan;
mod state;
mod tui;
mod tz;

use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub use metric::{MetricValue, StatsMetric};
pub(crate) use pricing::{
    is_pricing_fingerprint_fresh, maybe_spawn_pricing_sync_if_stale, pricing_cache_fingerprint,
    DEFAULT_PRICING_TTL_SECS, MODELS_DEV_CACHE_FILE,
};
pub use pricing::{run_pricing_command, PricingCatalog, SyncMode};
pub(crate) use scan::{
    estimate_conversation_cost_usd_with_parent_conn, lookup_cached_conversation_cost,
};
pub use tui::run_stats_tui;
pub use tz::unix_secs_to_utc_date;

use crate::timeutil::civil_date_from_unix_day;
use query::{SourceFilter, StatsDataset};
use render::render_stats_report_with_metric;
#[cfg(test)]
use render::render_stats_report;
use scan::UsageEvent;
use tz::format_day;
#[cfg(test)]
use tz::TimeZoneSpec;

const DAYS_PER_WEEK: i64 = 7;
const ACTIVITY_WEEKS: usize = 20;

/// Aggregated token and optional cost totals for a time window.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PeriodSummary {
    /// Total deduplicated turns/requests with non-zero token usage.
    pub requests: u64,
    /// Uncached input tokens.
    pub input_tokens: u64,
    /// Prompt cache read tokens.
    pub cache_read_tokens: u64,
    /// Prompt cache write tokens.
    pub cache_write_tokens: u64,
    /// Output tokens (includes thinking tokens when reported by AGY).
    pub output_tokens: u64,
    /// Thinking/reasoning output tokens (subset of `output_tokens`).
    pub thinking_tokens: u64,
    /// Total tokens (`input_tokens + cache_read_tokens + cache_write_tokens + output_tokens`).
    pub total_tokens: u64,
    /// Estimated cost in USD across priced turns (from native `model_cost` or `models.dev` rates).
    pub priced_usd: f64,
    /// Count of turns with a price entry or native cost.
    pub priced_requests: u64,
    /// Count of turns without a price entry or native cost.
    pub unpriced_requests: u64,
}

/// Standard fixed-window summaries (`today`, `last_7d`, `last_30d`, `all_time`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WindowSummaries {
    /// Local today (`now_unix_secs` date in the active timezone).
    pub today: PeriodSummary,
    /// Last 7 local calendar days including today.
    pub last_7d: PeriodSummary,
    /// Last 30 local calendar days including today.
    pub last_30d: PeriodSummary,
    /// All recorded turns regardless of date.
    pub all_time: PeriodSummary,
}

/// Daily local-calendar token and optional cost aggregate.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DailyUsage {
    /// Local calendar date in `YYYY-MM-DD` format.
    pub date: String,
    /// Turn/request count on this date.
    pub requests: u64,
    /// Uncached input tokens.
    pub input_tokens: u64,
    /// Cache read tokens.
    pub cache_read_tokens: u64,
    /// Cache write tokens.
    pub cache_write_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Thinking tokens.
    pub thinking_tokens: u64,
    /// Total tokens (`input + cache_read + cache_write + output`).
    pub total_tokens: u64,
    /// Estimated cost in USD for priced turns.
    pub priced_usd: f64,
    /// Count of unpriced turns on this date.
    pub unpriced_requests: u64,
}

/// Monday-anchored local calendar week aggregate for the 20-week activity tile grid.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WeeklyUsage {
    /// Monday start date in `YYYY-MM-DD` format.
    pub week_start: String,
    /// Sunday end date in `YYYY-MM-DD` format.
    pub week_end: String,
    /// Per-day token totals from Monday (`0`) through Sunday (`6`), `None` for future days after `today_local`.
    pub daily_tokens: [Option<u64>; 7],
    /// Total tokens across non-future days in this week.
    pub total_tokens: u64,
}

/// Per-model usage, optional cost, and speed/TTFT leaderboard entry.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelStats {
    /// Display model name derived dynamically from AGY metadata.
    pub model: String,
    /// Matched catalog model key (or `"native"`) when priced.
    pub pricing_key: Option<String>,
    /// Total turns attributed to this model.
    pub requests: u64,
    /// Uncached input tokens.
    pub input_tokens: u64,
    /// Cache read tokens.
    pub cache_read_tokens: u64,
    /// Cache write tokens.
    pub cache_write_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Thinking tokens.
    pub thinking_tokens: u64,
    /// Total tokens (`input + cache_read + cache_write + output`).
    pub total_tokens: u64,
    /// Estimated cost in USD.
    pub priced_usd: f64,
    /// Count of unpriced turns in this model bucket.
    pub unpriced_requests: u64,
    /// Number of valid decode speed samples.
    pub speed_samples: usize,
    /// Median decode throughput in tokens/second, if speed samples exist.
    pub median_tok_per_sec: Option<f64>,
    /// Number of valid TTFT samples.
    pub ttft_samples: usize,
    /// Median time-to-first-token in seconds, if TTFT samples exist.
    pub median_ttft_secs: Option<f64>,
}

/// Aggregate speed and TTFT statistics from `conversations/*.db`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SqliteStats {
    /// Total valid UUID `.db` conversations scanned.
    pub db_files: u64,
    /// Completed turns decoded from `gen_metadata` passing speed sanity bounds.
    pub completed_turns: u64,
    /// Sum of output tokens across completed speed turns.
    pub total_output_tokens: u64,
    /// Median decode speed across completed turns (tokens/second).
    pub median_tok_per_sec: Option<f64>,
    /// Count of turns with valid TTFT (`chat.11`).
    pub ttft_samples: usize,
    /// Median time-to-first-token across decoded turns (seconds).
    pub median_ttft_secs: Option<f64>,
}

/// Complete usage report computed by `agy-statusline stats` and serialized by `--json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StatsReport {
    /// Reference local calendar date (`YYYY-MM-DD`) derived from `now_unix_secs` and `timezone`.
    pub today_local: String,
    /// Active timezone label (e.g., `"UTC+08:00"` or `"UTC"`).
    pub timezone: String,
    /// Reference UTC date (`YYYY-MM-DD`) derived from `now_unix_secs`.
    pub today_utc: String,
    /// Active `--days` filter (`None` for all-time).
    pub filter_days: Option<u32>,
    /// Earliest local calendar date observed across active usage events.
    pub first_active_date: Option<String>,
    /// Fixed comparison windows (`today`, `last_7d`, `last_30d`, `all_time`).
    pub windows: WindowSummaries,
    /// Active summary totals (filtered by `filter_days` when set).
    pub totals: PeriodSummary,
    /// Daily usage entries sorted chronologically (filtered by `filter_days` when set).
    pub daily: Vec<DailyUsage>,
    /// Unfiltered 20-week Monday-anchored calendar activity (oldest week first, ending with current week).
    pub weekly: Vec<WeeklyUsage>,
    /// Per-model breakdown sorted by `total_tokens` descending.
    pub models: Vec<ModelStats>,
    /// Aggregate statistics from `conversations/*.db` (`gen_metadata`).
    pub sqlite: SqliteStats,
}

fn accumulate_period(summary: &mut PeriodSummary, ev: &UsageEvent) {
    summary.requests += 1;
    summary.input_tokens = summary.input_tokens.saturating_add(ev.input_tokens);
    summary.cache_read_tokens = summary
        .cache_read_tokens
        .saturating_add(ev.cache_read_tokens);
    summary.cache_write_tokens = summary
        .cache_write_tokens
        .saturating_add(ev.cache_write_tokens);
    summary.output_tokens = summary.output_tokens.saturating_add(ev.output_tokens);
    summary.thinking_tokens = summary.thinking_tokens.saturating_add(ev.thinking_tokens);
    summary.total_tokens = summary.total_tokens.saturating_add(ev.total_tokens);
    match ev.cost_usd {
        Some(usd) => {
            summary.priced_usd += usd;
            summary.priced_requests += 1;
        }
        None => {
            summary.unpriced_requests += 1;
        }
    }
}

#[derive(Default)]
struct ModelAccumulator {
    pricing_key: Option<String>,
    summary: PeriodSummary,
    speeds: Vec<f64>,
    ttfts: Vec<f64>,
}

/// Yields the 20 Monday Unix day indices ending with the calendar week containing `today_idx`.
pub(super) fn activity_week_mondays(today_idx: i64) -> impl Iterator<Item = i64> {
    let today_wd = i64::from(
        civil_date_from_unix_day(today_idx)
            .weekday()
            .to_monday_zero_offset(),
    );
    let first_monday = (today_idx - today_wd) - ((ACTIVITY_WEEKS as i64 - 1) * DAYS_PER_WEEK);
    (0..ACTIVITY_WEEKS).map(move |w| first_monday + (w as i64) * DAYS_PER_WEEK)
}

fn build_weekly_usage(
    today_idx: i64,
    daily_by_date: &HashMap<&str, u64>,
) -> Vec<WeeklyUsage> {
    activity_week_mondays(today_idx)
        .map(|week_mon| {
            let mut daily_tokens = [None; 7];
            let mut total_tokens = 0u64;
            for (d, slot) in daily_tokens.iter_mut().enumerate() {
                let day_idx = week_mon + (d as i64);
                if day_idx <= today_idx {
                    let date = format_day(day_idx);
                    let tok = daily_by_date.get(date.as_str()).copied().unwrap_or(0);
                    *slot = Some(tok);
                    total_tokens = total_tokens.saturating_add(tok);
                }
            }
            WeeklyUsage {
                week_start: format_day(week_mon),
                week_end: format_day(week_mon + DAYS_PER_WEEK - 1),
                daily_tokens,
                total_tokens,
            }
        })
        .collect()
}

#[cfg(test)]
fn collect_stats_with_tz(
    home: &Path,
    days: Option<u32>,
    now_unix_secs: u64,
    tz: &TimeZoneSpec,
) -> StatsReport {
    let roots = [
        ("cli", home.join(".gemini").join("antigravity-cli")),
        ("desktop", home.join(".gemini").join("antigravity")),
    ];
    let pricing = PricingCatalog::load_effective_with(
        Some(&home.join(".cache/agy-statusline")),
        SyncMode::Offline,
        now_unix_secs,
        DEFAULT_PRICING_TTL_SECS,
        |_| unreachable!(),
    );
    StatsDataset::collect_for_roots(&roots, now_unix_secs, tz, &pricing).to_report(days)
}

/// Shared runtime options for `agy-statusline stats` (static output and interactive TUI).
#[derive(Debug, Clone, Copy)]
pub struct StatsOptions<'a> {
    /// Optional XDG cache directory for `models_dev_pricing.json`.
    pub cache_dir: Option<&'a Path>,
    /// Optional trailing day window (`--days N`).
    pub days: Option<u32>,
    /// Primary aggregation metric (`--metric`).
    pub metric: StatsMetric,
    /// Disable ANSI color escapes (`NO_COLOR`).
    pub no_color: bool,
    /// Use pure 7-bit ASCII characters (`--ascii`).
    pub ascii: bool,
    /// Reference Unix epoch timestamp in seconds.
    pub now_unix_secs: u64,
}

/// Runs the non-interactive `agy-statusline stats` subcommand (`--plain` / `--json` / piped).
pub fn run_stats_command<W: Write>(
    roots: &[(&'static str, PathBuf)],
    opts: &StatsOptions<'_>,
    json: bool,
    out: &mut W,
) -> io::Result<()> {
    let dataset = StatsDataset::collect(roots, opts.cache_dir, opts.now_unix_secs);
    let report = if json {
        dataset.to_report(opts.days)
    } else {
        dataset.to_report_with_metric(opts.days, opts.metric)
    };

    if json {
        let formatted = serde_json::to_string_pretty(&report)
            .map_err(|e| io::Error::other(format!("failed to serialize stats JSON: {e}")))?;
        writeln!(out, "{formatted}")?;
        return Ok(());
    }

    let metric_heatmap = (opts.metric != StatsMetric::Total).then(|| {
        dataset
            .build_metric_heatmap(opts.metric, SourceFilter::All, None)
            .0
    });
    render_stats_report_with_metric(
        &report,
        opts.metric,
        metric_heatmap.as_deref(),
        opts.no_color,
        opts.ascii,
        out,
    )
}

#[cfg(test)]
pub(super) mod test_helpers {
    use crate::protobuf::{encode_gen_blob, GenBlobSpec};

    #[allow(clippy::too_many_arguments)]
    pub(super) fn build_gen_blob(
        step_idx: Option<u64>,
        model_enum: u64,
        resp_model: &str,
        disp_name: &str,
        input_tok: u64,
        cache_read_tok: u64,
        output_tok: u64,
        thinking_tok: u64,
        dur_secs: u64,
        ttft_ms: u64,
        model_cost: Option<f32>,
    ) -> Vec<u8> {
        encode_gen_blob(GenBlobSpec {
            step_indices: step_idx.into_iter().collect(),
            model_enum,
            resp_model: Some(resp_model).filter(|s| !s.is_empty()),
            disp_name: Some(disp_name).filter(|s| !s.is_empty()),
            model_cost,
            input_tok,
            output_tok,
            cache_read_tok,
            thinking_tok,
            ttft_secs: (ttft_ms / 1_000) as i64,
            ttft_nanos: ((ttft_ms % 1_000) * 1_000_000) as i64,
            dur_secs: dur_secs as i64,
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;
    use std::fs;

    use super::*;
    use crate::protobuf::{encode_gen_blob, encode_step_meta_blob, GenBlobSpec};
    use crate::timeutil::parse_rfc3339_unix_secs;

    fn create_conv_db_tables(db_path: &Path) -> Connection {
        fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let conn = Connection::open(db_path).unwrap();
        conn.execute(
            "CREATE TABLE steps (idx INTEGER PRIMARY KEY, metadata BLOB)",
            [],
        )
        .unwrap();
        conn.execute(
            "CREATE TABLE gen_metadata (idx INTEGER PRIMARY KEY, data BLOB)",
            [],
        )
        .unwrap();
        conn
    }

    fn insert_step(conn: &Connection, idx: i64, created_at_secs: i64) {
        conn.execute(
            "INSERT INTO steps (idx, metadata) VALUES (?1, ?2)",
            rusqlite::params![idx, encode_step_meta_blob(created_at_secs)],
        )
        .unwrap();
    }

    fn insert_gen(conn: &Connection, idx: i64, spec: GenBlobSpec<'_>) {
        conn.execute(
            "INSERT INTO gen_metadata (idx, data) VALUES (?1, ?2)",
            rusqlite::params![idx, encode_gen_blob(spec)],
        )
        .unwrap();
    }

    #[test]
    fn sqlite_aggregates_cli_and_desktop_roots_with_timezone_and_medians() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let uuid = "11111111-1111-4111-8111-111111111111";
        let desk_uuid = "22222222-2222-4222-8222-222222222222";

        // CLI DB has 2 turns on 2026-11-17T20:00:00Z (= 2026-11-18T04:00:00+08:00)
        // and 1 turn on 2026-11-10T10:00:00Z (= 2026-11-10T18:00:00+08:00).
        let ts_today_utc17 = parse_rfc3339_unix_secs("2026-11-17T20:00:00Z").unwrap();
        let ts_desk_utc17 = parse_rfc3339_unix_secs("2026-11-17T21:00:00Z").unwrap();
        let ts_old = parse_rfc3339_unix_secs("2026-11-10T10:00:00Z").unwrap();

        let db_path = home
            .join(".gemini/antigravity-cli/conversations")
            .join(format!("{uuid}.db"));
        let conn = create_conv_db_tables(&db_path);
        insert_step(&conn, 1, ts_today_utc17);
        insert_step(&conn, 2, ts_today_utc17);
        insert_step(&conn, 3, ts_old);
        insert_gen(
            &conn,
            1,
            GenBlobSpec {
                step_indices: vec![1],
                model_enum: 1526,
                resp_model: Some("model-alpha"),
                disp_name: Some("Model Alpha (Medium)"),
                input_tok: 1000,
                cache_read_tok: 9000,
                output_tok: 200,
                thinking_tok: 150,
                dur_secs: 2,
                ttft_nanos: 400_000_000,
                ..Default::default()
            },
        );
        insert_gen(
            &conn,
            2,
            GenBlobSpec {
                step_indices: vec![2],
                model_enum: 1525,
                resp_model: Some("model-alpha"),
                disp_name: Some("Model Alpha (High)"),
                input_tok: 2000,
                output_tok: 400,
                thinking_tok: 300,
                dur_secs: 2,
                ttft_nanos: 600_000_000,
                ..Default::default()
            },
        );
        insert_gen(
            &conn,
            3,
            GenBlobSpec {
                step_indices: vec![3],
                model_enum: 1322,
                resp_model: Some("worker-tiered"),
                input_tok: 500,
                output_tok: 100,
                thinking_tok: 50,
                dur_secs: 1,
                ttft_nanos: 250_000_000,
                ..Default::default()
            },
        );
        drop(conn);

        // Desktop SQLite session contributes alongside CLI.
        let desk_db_path = home
            .join(".gemini/antigravity/conversations")
            .join(format!("{desk_uuid}.db"));
        let desk_conn = create_conv_db_tables(&desk_db_path);
        insert_step(&desk_conn, 1, ts_desk_utc17);
        insert_gen(
            &desk_conn,
            1,
            GenBlobSpec {
                step_indices: vec![1],
                model_enum: 1400,
                resp_model: Some("desktop-model"),
                input_tok: 400,
                cache_read_tok: 600,
                output_tok: 100,
                dur_secs: 1,
                ttft_nanos: 300_000_000,
                ..Default::default()
            },
        );
        drop(desk_conn);

        let tz_plus8 = TimeZoneSpec::fixed_offset(8 * 3_600);
        let now_secs = parse_rfc3339_unix_secs("2026-11-17T22:00:00Z").unwrap() as u64;
        let report = collect_stats_with_tz(home, None, now_secs, &tz_plus8);

        assert_eq!(report.today_local, "2026-11-18");
        assert_eq!(report.timezone, "UTC+08:00");
        assert_eq!(report.today_utc, "2026-11-17");
        assert_eq!(report.first_active_date.as_deref(), Some("2026-11-10"));
        assert_eq!(report.sqlite.db_files, 2);

        // Today in UTC+08:00 has 2 CLI SQLite turns + 1 desktop SQLite turn = 3 turns.
        assert_eq!(report.windows.today.requests, 3);
        assert_eq!(report.windows.today.input_tokens, 3400);
        assert_eq!(report.windows.today.cache_read_tokens, 9600);
        assert_eq!(report.windows.today.output_tokens, 700);
        assert_eq!(report.windows.today.thinking_tokens, 450);
        assert_eq!(report.windows.all_time.requests, 4);
        assert_eq!(report.windows.all_time.priced_requests, 0);

        // Models: Model Alpha collapsed Medium+High -> 2 turns, speeds [100, 200] -> 150.0, ttfts [0.4, 0.6] -> 0.5
        assert_eq!(report.models.len(), 3);
        assert_eq!(report.models[0].model, "Model Alpha");
        assert_eq!(report.models[0].requests, 2);
        assert_eq!(report.models[0].thinking_tokens, 450);
        assert!((report.models[0].median_tok_per_sec.unwrap() - 150.0).abs() < 1e-6);
        assert!((report.models[0].median_ttft_secs.unwrap() - 0.5).abs() < 1e-6);
        assert_eq!(report.models[1].model, "desktop-model");
        assert_eq!(report.models[2].model, "worker-tiered");

        // No cost column when no turns are priced
        let mut rendered = Vec::new();
        render_stats_report(&report, true, true, &mut rendered).unwrap();
        let text = String::from_utf8(rendered).unwrap();
        assert!(text.contains("Local 2026-11-18 (UTC+08:00)"));
        assert!(!text.contains("Est. Cost"));
        assert!(!text.contains("unpriced"));
        assert!(text.contains("Thinking"));
        assert!(text.contains("Cache Hit"));
    }

    #[test]
    fn cached_rates_and_native_cost_enable_cost_column() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let uuid = "33333333-3333-4333-8333-333333333333";
        let ts = parse_rfc3339_unix_secs("2026-11-18T10:00:00Z").unwrap();

        let db_path = home
            .join(".gemini/antigravity-cli/conversations")
            .join(format!("{uuid}.db"));
        let conn = create_conv_db_tables(&db_path);
        insert_step(&conn, 1, ts);
        insert_step(&conn, 2, ts);
        insert_gen(
            &conn,
            1,
            GenBlobSpec {
                step_indices: vec![1],
                model_enum: 100,
                resp_model: Some("custom-pro"),
                disp_name: Some("Custom Pro (High)"),
                input_tok: 1_000_000,
                output_tok: 1_000_000,
                dur_secs: 10,
                ttft_nanos: 500_000_000,
                ..Default::default()
            },
        );
        insert_gen(
            &conn,
            2,
            GenBlobSpec {
                step_indices: vec![2],
                model_enum: 101,
                resp_model: Some("native-billed"),
                disp_name: Some("Native Billed"),
                model_cost: Some(0.25),
                input_tok: 1_000,
                output_tok: 100,
                dur_secs: 1,
                ttft_nanos: 200_000_000,
                ..Default::default()
            },
        );
        drop(conn);

        let pricing_path = home.join(".cache/agy-statusline/models_dev_pricing.json");
        fs::create_dir_all(pricing_path.parent().unwrap()).unwrap();
        fs::write(
            &pricing_path,
            r#"{"fetched_at":0,"models":{"custom-pro":{"input_per_mtok":2.0,"cache_read_per_mtok":0.2,"output_per_mtok":8.0}}}"#,
        )
        .unwrap();

        let report = collect_stats_with_tz(home, None, ts as u64, &TimeZoneSpec::utc());
        assert_eq!(report.windows.all_time.priced_requests, 2);
        assert!((report.windows.all_time.priced_usd - 10.25).abs() < 1e-6);

        let mut rendered = Vec::new();
        render_stats_report(&report, true, true, &mut rendered).unwrap();
        let text = String::from_utf8(rendered).unwrap();
        assert!(text.contains("Est. Cost"));
        assert!(text.contains("~$10.25"));
    }

    #[test]
    fn days_filter_preserves_20w_weekly_activity() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let uuid = "44444444-4444-4444-8444-444444444444";
        let now_secs = parse_rfc3339_unix_secs("2026-11-18T12:00:00Z").unwrap() as u64;
        let old_secs = parse_rfc3339_unix_secs("2026-11-03T12:00:00Z").unwrap();

        let db_path = home
            .join(".gemini/antigravity-cli/conversations")
            .join(format!("{uuid}.db"));
        let conn = create_conv_db_tables(&db_path);
        insert_step(&conn, 1, old_secs);
        insert_gen(
            &conn,
            1,
            GenBlobSpec {
                step_indices: vec![1],
                model_enum: 1526,
                resp_model: Some("model-alpha"),
                disp_name: Some("Model Alpha"),
                input_tok: 2_500,
                output_tok: 500,
                dur_secs: 2,
                ttft_nanos: 300_000_000,
                ..Default::default()
            },
        );
        drop(conn);

        let report = collect_stats_with_tz(home, Some(7), now_secs, &TimeZoneSpec::utc());
        assert_eq!(report.filter_days, Some(7));
        assert!(report.daily.is_empty());
        assert!(report.models.is_empty());
        assert_eq!(report.weekly.len(), 20);
        assert_eq!(report.weekly[17].week_start, "2026-11-02");
        assert_eq!(report.weekly[17].week_end, "2026-11-08");
        assert_eq!(report.weekly[17].daily_tokens[1], Some(3_000));
        assert_eq!(report.weekly[17].total_tokens, 3_000);

        let mut out = Vec::new();
        render_stats_report(&report, true, true, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains("no activity"), "unexpected text:\n{text}");
        assert!(
            text.contains("Activity (20w): peak 3K on 2026-11-03 (Tue) | Less .. == -- ## ** More"),
            "missing 15d-old activity peak in:\n{text}"
        );
        assert!(
            text.contains("  Tue  .. .. .. .. .. .. .. .. .. .. .. .. .. .. .. .. .. ** .. .."),
            "missing Tue tile row in:\n{text}"
        );
    }
}
