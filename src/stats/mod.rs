//! Read-only local usage, estimated cost, and speed statistics (`agy-statusline stats`).
//!
//! Scans AGY local directories in read-only mode:
//! - `conversations/<uuid>.db` (`gen_metadata` joined with `steps.metadata`) as the
//!   authoritative primary source for turns, tokens, thinking tokens, model names,
//!   timestamps, decode speed, and TTFT.
//! - `brain/**/transcript.jsonl` as a deduplicated fallback ONLY for conversations that
//!   have no usable SQLite usage records.

mod pricing;
mod render;
mod scan;
mod tz;

use std::collections::{BTreeMap, HashMap};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub use pricing::{run_pricing_command, PricingCatalog, SyncMode};
pub(crate) use scan::estimate_conversation_cost_usd;
pub use tz::unix_secs_to_utc_date;

use crate::db::compute_median;
use crate::paths::{resolve_agy_cli_dir, xdg_cache_dir};
use crate::timeutil::civil_date_from_unix_day;
use pricing::{fetch_models_dev_via_curl, round_usd_6, DEFAULT_PRICING_TTL_SECS};
use render::render_stats_report;
use scan::{scan_sqlite_conversations, scan_transcripts, UsageEvent};
use tz::{format_day, TimeZoneSpec};

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

/// Aggregate speed and TTFT statistics extracted from `conversations/*.db`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SqliteStats {
    /// Number of valid UUID `.db` files scanned.
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
    /// Aggregate statistics from SQLite `conversations/*.db` (`gen_metadata`).
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

fn build_weekly_usage(
    today_idx: i64,
    daily_by_date: &HashMap<&str, u64>,
) -> Vec<WeeklyUsage> {
    let today_wd = i64::from(
        civil_date_from_unix_day(today_idx)
            .weekday()
            .to_monday_zero_offset(),
    );
    let curr_monday = today_idx - today_wd;
    let first_monday = curr_monday - ((ACTIVITY_WEEKS as i64 - 1) * DAYS_PER_WEEK);
    let mut weeks = Vec::with_capacity(ACTIVITY_WEEKS);

    for w in 0..ACTIVITY_WEEKS {
        let week_mon = first_monday + (w as i64) * DAYS_PER_WEEK;
        let week_sun = week_mon + (DAYS_PER_WEEK - 1);
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

        weeks.push(WeeklyUsage {
            week_start: format_day(week_mon),
            week_end: format_day(week_sun),
            daily_tokens,
            total_tokens,
        });
    }

    weeks
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
    collect_stats_for_roots(&roots, days, now_unix_secs, tz, &pricing)
}

fn collect_stats_for_roots(
    roots: &[(&'static str, PathBuf)],
    days: Option<u32>,
    now_unix_secs: u64,
    tz: &TimeZoneSpec,
    pricing: &PricingCatalog,
) -> StatsReport {
    let now_i64 = now_unix_secs as i64;
    let (today_day_idx, today_local) = tz.local_day_and_date(now_i64);
    let timezone = tz.format_offset_at(now_i64);
    let today_utc = unix_secs_to_utc_date(now_unix_secs);
    let filter_days = days.filter(|&d| d > 0);
    let filter_cutoff = filter_days.map(|d| today_day_idx - i64::from(d - 1));

    let (sqlite, mut events, covered) = scan_sqlite_conversations(roots, tz, pricing);
    let fallback_events = scan_transcripts(roots, tz, pricing, &covered);
    events.extend(fallback_events);

    let mut windows = WindowSummaries::default();
    let mut totals = PeriodSummary::default();
    let mut daily_map: BTreeMap<i64, (String, PeriodSummary)> = BTreeMap::new();
    let mut unfiltered_daily_tokens: BTreeMap<String, u64> = BTreeMap::new();
    let mut model_map: HashMap<String, ModelAccumulator> = HashMap::new();
    let mut first_day: Option<i64> = None;

    for ev in &events {
        first_day = Some(match first_day {
            Some(prev) => prev.min(ev.local_day),
            None => ev.local_day,
        });

        let day_tok = unfiltered_daily_tokens
            .entry(ev.local_date.clone())
            .or_insert(0);
        *day_tok = day_tok.saturating_add(ev.total_tokens);

        accumulate_period(&mut windows.all_time, ev);
        if ev.local_day == today_day_idx {
            accumulate_period(&mut windows.today, ev);
        }
        if ev.local_day >= today_day_idx - 6 && ev.local_day <= today_day_idx {
            accumulate_period(&mut windows.last_7d, ev);
        }
        if ev.local_day >= today_day_idx - 29 && ev.local_day <= today_day_idx {
            accumulate_period(&mut windows.last_30d, ev);
        }

        let in_filter = filter_cutoff
            .is_none_or(|cutoff| ev.local_day >= cutoff && ev.local_day <= today_day_idx);
        if !in_filter {
            continue;
        }

        accumulate_period(&mut totals, ev);

        let (_, day_summary) = daily_map
            .entry(ev.local_day)
            .or_insert_with(|| (ev.local_date.clone(), PeriodSummary::default()));
        accumulate_period(day_summary, ev);

        let model_entry = model_map.entry(ev.model_group.clone()).or_default();
        if model_entry.pricing_key.is_none() {
            model_entry.pricing_key.clone_from(&ev.pricing_key);
        }
        accumulate_period(&mut model_entry.summary, ev);
        if let Some(speed) = ev.decode_speed {
            model_entry.speeds.push(speed);
        }
        if let Some(ttft) = ev.ttft_secs {
            model_entry.ttfts.push(ttft);
        }
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
        b.total_tokens
            .cmp(&a.total_tokens)
            .then_with(|| a.model.cmp(&b.model))
    });

    let unfiltered_lookup: HashMap<&str, u64> = unfiltered_daily_tokens
        .iter()
        .map(|(date, tokens)| (date.as_str(), *tokens))
        .collect();
    let weekly = build_weekly_usage(today_day_idx, &unfiltered_lookup);

    StatsReport {
        today_local,
        timezone,
        today_utc,
        filter_days,
        first_active_date: first_day.map(format_day),
        windows,
        totals,
        daily,
        weekly,
        models,
        sqlite,
    }
}

/// Runs the `agy-statusline stats` subcommand against the resolved environment directories.
pub fn run_stats_command<W: Write>(
    home: Option<&Path>,
    days: Option<u32>,
    json: bool,
    no_color: bool,
    ascii: bool,
    now_unix_secs: u64,
    out: &mut W,
) -> io::Result<()> {
    let mut roots: Vec<(&'static str, PathBuf)> = Vec::new();
    if let Some(cli_dir) = resolve_agy_cli_dir()
        .or_else(|| home.map(|h| h.join(".gemini").join("antigravity-cli")))
    {
        roots.push(("cli", cli_dir));
    }
    if let Some(h) = home {
        roots.push(("desktop", h.join(".gemini").join("antigravity")));
    }

    let cache_dir =
        xdg_cache_dir().or_else(|| home.map(|h| h.join(".cache").join("agy-statusline")));
    let pricing = PricingCatalog::load_effective_with(
        cache_dir.as_deref(),
        SyncMode::BlockingIfStale,
        now_unix_secs,
        DEFAULT_PRICING_TTL_SECS,
        fetch_models_dev_via_curl,
    );
    let tz = TimeZoneSpec::local();
    let report = collect_stats_for_roots(&roots, days, now_unix_secs, &tz, &pricing);

    if json {
        let formatted = serde_json::to_string_pretty(&report)
            .map_err(|e| io::Error::other(format!("failed to serialize stats JSON: {e}")))?;
        writeln!(out, "{formatted}")?;
        return Ok(());
    }
    render_stats_report(&report, no_color, ascii, out)
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
    fn sqlite_is_primary_and_transcript_fallback_skips_covered_uuids() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let uuid = "11111111-1111-4111-8111-111111111111";
        let desk_uuid = "22222222-2222-4222-8222-222222222222";

        // CLI DB has 2 turns on 2026-11-17T20:00:00Z (= 2026-11-18T04:00:00+08:00)
        // and 1 turn on 2026-11-10T10:00:00Z (= 2026-11-10T18:00:00+08:00).
        let ts_today_utc17 = parse_rfc3339_unix_secs("2026-11-17T20:00:00Z").unwrap();
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

        // Matching CLI transcript for the SAME uuid MUST be ignored (no double counting).
        let cli_transcript = home.join(format!(
            ".gemini/antigravity-cli/brain/{uuid}/.system_generated/logs/transcript.jsonl"
        ));
        fs::create_dir_all(cli_transcript.parent().unwrap()).unwrap();
        let dup_line = r#"{"created_at":"2026-11-17T20:00:00Z","step_index":1,"input_tokens":99999,"output_tokens":999,"cache_read_tokens":0}"#;
        fs::write(&cli_transcript, format!("{dup_line}\n")).unwrap();

        // Desktop session WITHOUT a .db uses transcript fallback.
        let desk_transcript = home.join(format!(
            ".gemini/antigravity/brain/{desk_uuid}/transcript.jsonl"
        ));
        fs::create_dir_all(desk_transcript.parent().unwrap()).unwrap();
        let desk_line = r#"{"created_at":"2026-11-17T21:00:00Z","step_index":0,"model":"desktop-model","input_tokens":400,"cache_read_tokens":600,"output_tokens":100}"#;
        fs::write(&desk_transcript, format!("{desk_line}\n")).unwrap();

        let tz_plus8 = TimeZoneSpec::fixed_offset(8 * 3_600);
        let now_secs = parse_rfc3339_unix_secs("2026-11-17T22:00:00Z").unwrap() as u64;
        let report = collect_stats_with_tz(home, None, now_secs, &tz_plus8);

        assert_eq!(report.today_local, "2026-11-18");
        assert_eq!(report.timezone, "UTC+08:00");
        assert_eq!(report.today_utc, "2026-11-17");
        assert_eq!(report.first_active_date.as_deref(), Some("2026-11-10"));

        // Today in UTC+08:00 has 2 CLI SQLite turns + 1 desktop fallback turn = 3 turns.
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
