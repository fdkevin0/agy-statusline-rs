//! Read-only local usage, optional user-defined cost, and speed statistics (`agy-statusline stats`).
//!
//! Scans AGY local directories in read-only mode:
//! - `conversations/<uuid>.db` (`gen_metadata` joined with `steps.metadata`) as the
//!   authoritative primary source for turns, tokens, thinking tokens, model names,
//!   timestamps, decode speed, and TTFT.
//! - `brain/**/transcript.jsonl` as a deduplicated fallback ONLY for conversations that
//!   do not have a corresponding `<uuid>.db`.

mod pricing;
mod render;
mod scan;
mod tz;

use std::collections::{BTreeMap, HashMap};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub use pricing::{
    fetch_models_dev_via_curl, parse_models_dev_json, pricing_cache_status, run_pricing_command,
    sync_models_dev_pricing_with, ModelPricingRate, PricingCacheStatus, PricingCatalog, SyncMode,
    DEFAULT_PRICING_TTL_SECS,
};
pub use render::render_stats_report;
pub use scan::{derive_model_name, estimate_conversation_cost_usd};
pub use tz::{
    parse_rfc3339_unix_secs, parse_rfc3339_utc_day, unix_secs_to_utc_date, TimeZoneSpec,
};

use pricing::round_usd_6;
use scan::{scan_sqlite_conversations, scan_transcripts, UsageEvent};
use tz::format_day;

const DAYS_PER_WEEK: i64 = 7;
const ACTIVITY_WEEKS: usize = 20;

use crate::db::compute_median;
use crate::paths::{resolve_agy_cli_dir, xdg_cache_dir, xdg_config_dir};

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
    #[serde(default)]
    pub cache_write_tokens: u64,
    /// Output tokens (includes thinking tokens when reported by AGY).
    pub output_tokens: u64,
    /// Thinking/reasoning output tokens (subset of `output_tokens`).
    pub thinking_tokens: u64,
    /// Total tokens (`input_tokens + cache_read_tokens + cache_write_tokens + output_tokens`).
    pub total_tokens: u64,
    /// Estimated cost in USD across priced turns (from native `model_cost` or user `pricing.json`).
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
    #[serde(default)]
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
    /// Matched user `pricing.json` key (or `"native"`) when priced.
    pub pricing_key: Option<String>,
    /// Total turns attributed to this model.
    pub requests: u64,
    /// Uncached input tokens.
    pub input_tokens: u64,
    /// Cache read tokens.
    pub cache_read_tokens: u64,
    /// Cache write tokens.
    #[serde(default)]
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

/// Complete report returned by [`collect_stats`] and serialized by `--json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StatsReport {
    /// Reference local calendar date (`YYYY-MM-DD`) derived from `now_unix_secs` and `timezone`.
    #[serde(default)]
    pub today_local: String,
    /// Active timezone label (e.g., `"UTC+08:00"` or `"UTC"`).
    #[serde(default)]
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
    #[serde(default)]
    pub weekly: Vec<WeeklyUsage>,
    /// Unfiltered per-local-day token totals (`YYYY-MM-DD -> total_tokens`) for the 20-week activity grid.
    #[serde(skip)]
    pub unfiltered_daily_tokens: BTreeMap<String, u64>,
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

/// Monday-based weekday index (`0 = Mon .. 6 = Sun`) for days since `1970-01-01` (Thursday).
fn weekday_mon0(day_idx: i64) -> usize {
    (day_idx + 3).rem_euclid(DAYS_PER_WEEK) as usize
}

fn build_weekly_usage(
    today_idx: i64,
    daily_by_date: &HashMap<&str, u64>,
) -> Vec<WeeklyUsage> {
    let today_wd = weekday_mon0(today_idx) as i64;
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

/// Scans AGY local directories under `home` using the host local timezone and builds a [`StatsReport`].
///
/// Reads `$HOME/.config/agy-statusline/pricing.json` if present under `home` without inspecting
/// process-wide `XDG_CONFIG_HOME` or `ANTIGRAVITY_CLI_HOME`.
pub fn collect_stats(home: &Path, days: Option<u32>, now_unix_secs: u64) -> StatsReport {
    let tz = TimeZoneSpec::local();
    collect_stats_with_tz(home, days, now_unix_secs, &tz)
}

/// Deterministic [`StatsReport`] collector parameterized by an explicit [`TimeZoneSpec`].
pub fn collect_stats_with_tz(
    home: &Path,
    days: Option<u32>,
    now_unix_secs: u64,
    tz: &TimeZoneSpec,
) -> StatsReport {
    let roots = [
        ("cli", home.join(".gemini").join("antigravity-cli")),
        ("desktop", home.join(".gemini").join("antigravity")),
    ];
    let pricing = PricingCatalog::load_from_file(
        &home
            .join(".config")
            .join("agy-statusline")
            .join("pricing.json"),
    );
    collect_stats_for_roots(&roots, days, now_unix_secs, tz, &pricing)
}

/// Builds a [`StatsReport`] for explicit `(source_kind, root_path)` directories, timezone, and pricing.
pub fn collect_stats_for_roots(
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
        .map(|(model, acc)| {
            let speed_samples = acc.speeds.iter().filter(|v| v.is_finite()).count();
            let ttft_samples = acc.ttfts.iter().filter(|v| v.is_finite()).count();
            ModelStats {
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
                speed_samples,
                median_tok_per_sec: compute_median(&acc.speeds),
                ttft_samples,
                median_ttft_secs: compute_median(&acc.ttfts),
            }
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
        unfiltered_daily_tokens,
        models,
        sqlite,
    }
}


/// Runs the `agy-statusline stats` subcommand against the resolved environment directories.
pub fn run_stats_command<W: Write>(
    home: &Path,
    days: Option<u32>,
    json: bool,
    no_color: bool,
    ascii: bool,
    now_unix_secs: u64,
    out: &mut W,
) -> io::Result<()> {
    let mut roots: Vec<(&'static str, PathBuf)> = Vec::new();
    if let Some(cli_dir) = resolve_agy_cli_dir().or_else(|| {
        (!home.as_os_str().is_empty()).then(|| home.join(".gemini").join("antigravity-cli"))
    }) {
        roots.push(("cli", cli_dir));
    }
    if !home.as_os_str().is_empty() {
        roots.push(("desktop", home.join(".gemini").join("antigravity")));
    }

    let cache_dir = xdg_cache_dir().or_else(|| {
        (!home.as_os_str().is_empty()).then(|| home.join(".cache").join("agy-statusline"))
    });
    let pricing_path = xdg_config_dir()
        .ok()
        .map(|d| d.join("pricing.json"))
        .or_else(|| {
            (!home.as_os_str().is_empty()).then(|| {
                home.join(".config")
                    .join("agy-statusline")
                    .join("pricing.json")
            })
        });
    let pricing = PricingCatalog::load_effective_with(
        cache_dir.as_deref(),
        pricing_path.as_deref(),
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
mod tests {
    use super::*;
    use rusqlite::Connection;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use unicode_width::UnicodeWidthStr;

    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_home(tag: &str) -> PathBuf {
        let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agy_statusline_stats_{tag}_{}_{seq}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn push_varint(buf: &mut Vec<u8>, mut val: u64) {
        loop {
            let mut byte = (val & 0x7f) as u8;
            val >>= 7;
            if val != 0 {
                byte |= 0x80;
            }
            buf.push(byte);
            if val == 0 {
                break;
            }
        }
    }

    fn push_field_varint(buf: &mut Vec<u8>, id: u64, val: u64) {
        push_varint(buf, id << 3);
        push_varint(buf, val);
    }

    fn push_field_bytes(buf: &mut Vec<u8>, id: u64, bytes: &[u8]) {
        push_varint(buf, (id << 3) | 2);
        push_varint(buf, bytes.len() as u64);
        buf.extend_from_slice(bytes);
    }

    fn push_field_f32(buf: &mut Vec<u8>, id: u64, val: f32) {
        push_varint(buf, (id << 3) | 5);
        buf.extend_from_slice(&val.to_le_bytes());
    }

    #[allow(clippy::too_many_arguments)]
    fn build_gen_blob(
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
        let mut usage = Vec::new();
        push_field_varint(&mut usage, 2, input_tok);
        push_field_varint(&mut usage, 3, output_tok);
        push_field_varint(&mut usage, 5, cache_read_tok);
        push_field_varint(&mut usage, 9, thinking_tok);

        let mut ttft = Vec::new();
        push_field_varint(&mut ttft, 1, ttft_ms / 1_000);
        push_field_varint(&mut ttft, 2, (ttft_ms % 1_000) * 1_000_000);

        let mut dur = Vec::new();
        push_field_varint(&mut dur, 1, dur_secs);

        let mut chat = Vec::new();
        push_field_varint(&mut chat, 3, model_enum);
        push_field_bytes(&mut chat, 4, &usage);
        if let Some(c) = model_cost {
            push_field_f32(&mut chat, 5, c);
        }
        push_field_bytes(&mut chat, 11, &ttft);
        push_field_bytes(&mut chat, 12, &dur);
        if !resp_model.is_empty() {
            push_field_bytes(&mut chat, 19, resp_model.as_bytes());
        }
        if !disp_name.is_empty() {
            push_field_bytes(&mut chat, 21, disp_name.as_bytes());
        }

        let mut root = Vec::new();
        push_field_bytes(&mut root, 1, &chat);
        if let Some(s) = step_idx {
            let mut packed = Vec::new();
            push_varint(&mut packed, s);
            push_field_bytes(&mut root, 2, &packed);
        }
        root
    }

    fn build_step_meta_blob(created_at_secs: u64) -> Vec<u8> {
        let mut ts = Vec::new();
        push_field_varint(&mut ts, 1, created_at_secs);
        let mut step = Vec::new();
        push_field_bytes(&mut step, 1, &ts);
        step
    }

    fn create_full_conv_db(db_path: &Path, steps: &[(i64, Vec<u8>)], gens: &[(i64, Vec<u8>)]) {
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
        for (idx, meta) in steps {
            conn.execute(
                "INSERT INTO steps (idx, metadata) VALUES (?1, ?2)",
                rusqlite::params![idx, meta],
            )
            .unwrap();
        }
        for (idx, data) in gens {
            conn.execute(
                "INSERT INTO gen_metadata (idx, data) VALUES (?1, ?2)",
                rusqlite::params![idx, data],
            )
            .unwrap();
        }
    }

    fn build_tzif_v2(offset_secs: i32, footer: &str) -> Vec<u8> {
        let mut out = Vec::new();
        // v1 header: timecnt=0, typecnt=1, charcnt=4
        out.extend_from_slice(b"TZif2");
        out.extend_from_slice(&[0u8; 15]);
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(&1u32.to_be_bytes());
        out.extend_from_slice(&4u32.to_be_bytes());
        // v1 ttinfo (6 bytes) + abbr (4 bytes)
        out.extend_from_slice(&offset_secs.to_be_bytes());
        out.push(0);
        out.push(0);
        out.extend_from_slice(b"CST\0");
        // v2 header: timecnt=1, typecnt=1, charcnt=4
        out.extend_from_slice(b"TZif2");
        out.extend_from_slice(&[0u8; 15]);
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(&1u32.to_be_bytes());
        out.extend_from_slice(&1u32.to_be_bytes());
        out.extend_from_slice(&4u32.to_be_bytes());
        // v2 transition time (8 bytes), type_idx (1 byte), ttinfo (6 bytes), abbr (4 bytes)
        out.extend_from_slice(&0i64.to_be_bytes());
        out.push(0);
        out.extend_from_slice(&offset_secs.to_be_bytes());
        out.push(0);
        out.push(0);
        out.extend_from_slice(b"CST\0");
        out.push(b'\n');
        out.extend_from_slice(footer.as_bytes());
        out.push(b'\n');
        out
    }

    #[test]
    fn rfc3339_utc_day_handles_z_and_timezone_rollover() {
        let (d1, s1) = parse_rfc3339_utc_day("2026-11-18T03:05:35Z").unwrap();
        assert_eq!(s1, "2026-11-18");

        let (d2, s2) = parse_rfc3339_utc_day("2026-11-17T23:30:00-05:00").unwrap();
        assert_eq!(s2, "2026-11-18");
        assert_eq!(d1, d2);

        let (d3, s3) = parse_rfc3339_utc_day("2026-11-18T01:00:00+08:00").unwrap();
        assert_eq!(s3, "2026-11-17");
        assert_eq!(d3, d1 - 1);

        assert_eq!(parse_rfc3339_utc_day("not-a-timestamp"), None);
        assert_eq!(parse_rfc3339_utc_day("2026-02-29T00:00:00Z"), None);
    }

    #[test]
    fn tzif_and_posix_dst_timezone_bucketing() {
        let shanghai_bytes = build_tzif_v2(8 * 3_600, "CST-8");
        let shanghai = TimeZoneSpec::from_tzif(&shanghai_bytes).unwrap();
        // 2026-11-17T20:00:00Z is 2026-11-18 04:00:00 in UTC+08:00
        let ts_utc_17 = parse_rfc3339_unix_secs("2026-11-17T20:00:00Z").unwrap();
        assert_eq!(shanghai.format_offset_at(ts_utc_17), "UTC+08:00");
        assert_eq!(shanghai.local_day_and_date(ts_utc_17).1, "2026-11-18");

        // US Eastern POSIX rule with DST: EST5EDT,M3.2.0,M11.1.0
        let ny = TimeZoneSpec::from_tz_env("EST5EDT,M3.2.0,M11.1.0").unwrap();
        let summer = parse_rfc3339_unix_secs("2026-07-01T12:00:00Z").unwrap();
        let winter = parse_rfc3339_unix_secs("2026-11-17T12:00:00Z").unwrap();
        assert_eq!(ny.format_offset_at(summer), "UTC-04:00");
        assert_eq!(ny.format_offset_at(winter), "UTC-05:00");
        assert_eq!(TimeZoneSpec::from_tzif(b"bad"), None);
    }

    #[test]
    fn derive_model_name_is_dynamic_and_strips_only_effort_suffixes() {
        assert_eq!(
            derive_model_name(Some("Model Alpha (Medium)"), Some("model-alpha"), 1526),
            "Model Alpha"
        );
        assert_eq!(
            derive_model_name(Some("Model Alpha (High)"), Some("model-alpha"), 1525),
            "Model Alpha"
        );
        assert_eq!(
            derive_model_name(Some("Orchestrator (TierA)"), Some("pro-orchestrator"), 1667),
            "Orchestrator (TierA)"
        );
        assert_eq!(
            derive_model_name(
                Some("Orchestrator (TierB)"),
                Some("pro-orchestrator-b"),
                1681
            ),
            "Orchestrator (TierB)"
        );
        assert_eq!(
            derive_model_name(
                Some("Worker Alpha (Orchestrator)"),
                Some("worker-agy"),
                1669
            ),
            "Worker Alpha (Orchestrator)"
        );
        assert_eq!(
            derive_model_name(None, Some("worker-tiered"), 1322),
            "worker-tiered"
        );
        assert_eq!(derive_model_name(None, None, 1999), "model#1999");
        assert_eq!(derive_model_name(None, None, 0), "unknown");
    }

    #[test]
    fn sqlite_is_primary_and_transcript_fallback_skips_covered_uuids() {
        let home = temp_home("sqlite_primary");
        let uuid = "11111111-1111-4111-8111-111111111111";
        let desk_uuid = "22222222-2222-4222-8222-222222222222";

        // CLI DB has 2 turns on 2026-11-17T20:00:00Z (= 2026-11-18T04:00:00+08:00)
        // and 1 turn on 2026-11-10T10:00:00Z (= 2026-11-10T18:00:00+08:00).
        let ts_today_utc17 = parse_rfc3339_unix_secs("2026-11-17T20:00:00Z").unwrap() as u64;
        let ts_old = parse_rfc3339_unix_secs("2026-11-10T10:00:00Z").unwrap() as u64;

        let db_path = home
            .join(".gemini/antigravity-cli/conversations")
            .join(format!("{uuid}.db"));
        let steps = vec![
            (1, build_step_meta_blob(ts_today_utc17)),
            (2, build_step_meta_blob(ts_today_utc17)),
            (3, build_step_meta_blob(ts_old)),
        ];
        let gens = vec![
            (
                1,
                build_gen_blob(
                    Some(1),
                    1526,
                    "model-alpha",
                    "Model Alpha (Medium)",
                    1000,
                    9000,
                    200,
                    150,
                    2,
                    400,
                    None,
                ),
            ),
            (
                2,
                build_gen_blob(
                    Some(2),
                    1525,
                    "model-alpha",
                    "Model Alpha (High)",
                    2000,
                    0,
                    400,
                    300,
                    2,
                    600,
                    None,
                ),
            ),
            (
                3,
                build_gen_blob(
                    Some(3),
                    1322,
                    "worker-tiered",
                    "",
                    500,
                    0,
                    100,
                    50,
                    1,
                    250,
                    None,
                ),
            ),
        ];
        create_full_conv_db(&db_path, &steps, &gens);

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
        let report = collect_stats_with_tz(&home, None, now_secs, &tz_plus8);

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

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn optional_pricing_json_and_native_cost_enable_cost_column() {
        let home = temp_home("pricing");
        let uuid = "33333333-3333-4333-8333-333333333333";
        let ts = parse_rfc3339_unix_secs("2026-11-18T10:00:00Z").unwrap() as u64;

        let db_path = home
            .join(".gemini/antigravity-cli/conversations")
            .join(format!("{uuid}.db"));
        let steps = vec![(1, build_step_meta_blob(ts)), (2, build_step_meta_blob(ts))];
        let gens = vec![
            (
                1,
                build_gen_blob(
                    Some(1),
                    100,
                    "custom-pro",
                    "Custom Pro (High)",
                    1_000_000,
                    0,
                    1_000_000,
                    0,
                    10,
                    500,
                    None,
                ),
            ),
            (
                2,
                build_gen_blob(
                    Some(2),
                    101,
                    "native-billed",
                    "Native Billed",
                    1_000,
                    0,
                    100,
                    0,
                    1,
                    200,
                    Some(0.25),
                ),
            ),
        ];
        create_full_conv_db(&db_path, &steps, &gens);

        let pricing_path = home.join(".config/agy-statusline/pricing.json");
        fs::create_dir_all(pricing_path.parent().unwrap()).unwrap();
        fs::write(
            &pricing_path,
            r#"{"models":{"custom-pro":{"input_per_mtok":2.0,"cache_read_per_mtok":0.2,"output_per_mtok":8.0}}}"#,
        )
        .unwrap();

        let report = collect_stats_with_tz(&home, None, ts, &TimeZoneSpec::utc());
        assert_eq!(report.windows.all_time.priced_requests, 2);
        assert!((report.windows.all_time.priced_usd - 10.25).abs() < 1e-6);

        let mut rendered = Vec::new();
        render_stats_report(&report, true, true, &mut rendered).unwrap();
        let text = String::from_utf8(rendered).unwrap();
        assert!(text.contains("Est. Cost"));
        assert!(text.contains("~$10.25"));

        let _ = fs::remove_dir_all(&home);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_cycles_and_duplicate_links_do_not_loop_or_double_count() {
        let home = temp_home("symlink");
        let brain = home.join(".gemini/antigravity-cli/brain");
        let real_log = brain.join("session-a/transcript.jsonl");
        fs::create_dir_all(real_log.parent().unwrap()).unwrap();
        let ev = r#"{"created_at":"2026-11-18T10:00:00Z","step_index":0,"chatModel":{"model":"local-model-v1","usage":{"inputTokens":1000,"cacheReadTokens":0,"outputTokens":100,"responseId":"sym-1"}}}"#;
        fs::write(&real_log, format!("{ev}\n")).unwrap();

        std::os::unix::fs::symlink(&brain, brain.join("cycle")).unwrap();
        let desk_session = home.join(".gemini/antigravity/brain/session-a");
        fs::create_dir_all(&desk_session).unwrap();
        std::os::unix::fs::symlink(&real_log, desk_session.join("transcript.jsonl")).unwrap();

        let now_secs = parse_rfc3339_unix_secs("2026-11-18T12:00:00Z").unwrap() as u64;
        let report = collect_stats_with_tz(&home, None, now_secs, &TimeZoneSpec::utc());
        assert_eq!(report.totals.requests, 1);
        assert_eq!(report.totals.total_tokens, 1100);
        assert_eq!(report.models[0].model, "local-model-v1");

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn scans_empty_home_and_falls_back_to_db_mtime_when_step_missing() {
        let home = temp_home("empty_and_mtime");
        let now_secs = parse_rfc3339_unix_secs("2026-11-18T12:00:00Z").unwrap() as u64;

        let empty = collect_stats_with_tz(&home, None, now_secs, &TimeZoneSpec::utc());
        assert_eq!(empty.totals.requests, 0);
        assert_eq!(empty.sqlite.db_files, 0);
        let mut empty_out = Vec::new();
        render_stats_report(&empty, true, true, &mut empty_out).unwrap();
        let empty_text = String::from_utf8(empty_out).unwrap();
        assert!(!empty_text.contains('\x1b'));
        assert!(
            empty_text.contains("Activity (20w): no activity | Less .. == -- ## ** More"),
            "unexpected text:\n{empty_text}"
        );
        assert!(
            empty_text.contains("       Jul         Aug            Sep         Oct         Nov")
        );
        assert!(empty_text
            .contains("  Mon  .. .. .. .. .. .. .. .. .. .. .. .. .. .. .. .. .. .. .. .."));
        assert!(
            empty_text.contains("  Sun  .. .. .. .. .. .. .. .. .. .. .. .. .. .. .. .. .. .. ..")
        );
        assert!(empty_text.contains("Models: none recorded"));

        // Minimal gen_metadata-only DB without steps table falls back to DB mtime.
        let conv_dir = home.join(".gemini/antigravity-cli/conversations");
        fs::create_dir_all(&conv_dir).unwrap();
        let db_path = conv_dir.join("8d0371e1-5a05-423b-a8bc-ed75995f9a8b.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute(
            "CREATE TABLE gen_metadata (idx INTEGER PRIMARY KEY, data BLOB)",
            [],
        )
        .unwrap();
        let blob_50 = build_gen_blob(None, 10, "tiny-model", "", 100, 200, 100, 40, 2, 1000, None);
        conn.execute(
            "INSERT INTO gen_metadata (idx, data) VALUES (1, ?1)",
            [blob_50],
        )
        .unwrap();
        drop(conn);
        fs::write(
            conv_dir.join("8d0371e1-5a05-423b-a8bc-ed75995f9a8b.db-wal"),
            b"junk",
        )
        .unwrap();
        fs::write(conv_dir.join("not-a-uuid.db"), b"junk").unwrap();

        let populated = collect_stats_with_tz(&home, None, now_secs, &TimeZoneSpec::utc());
        assert_eq!(populated.sqlite.db_files, 1);
        assert_eq!(populated.sqlite.completed_turns, 1);
        assert_eq!(populated.windows.all_time.requests, 1);
        assert_eq!(populated.models[0].model, "tiny-model");

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn days_filter_preserves_20w_weekly_activity() {
        let home = temp_home("days_heatmap");
        let uuid = "44444444-4444-4444-8444-444444444444";
        let now_secs = parse_rfc3339_unix_secs("2026-11-18T12:00:00Z").unwrap() as u64;
        let old_secs = parse_rfc3339_unix_secs("2026-11-03T12:00:00Z").unwrap() as u64;

        let db_path = home
            .join(".gemini/antigravity-cli/conversations")
            .join(format!("{uuid}.db"));
        let steps = vec![(1, build_step_meta_blob(old_secs))];
        let gens = vec![(
            1,
            build_gen_blob(
                Some(1),
                1526,
                "model-alpha",
                "Model Alpha",
                2_500,
                0,
                500,
                0,
                2,
                300,
                None,
            ),
        )];
        create_full_conv_db(&db_path, &steps, &gens);

        let report = collect_stats_with_tz(&home, Some(7), now_secs, &TimeZoneSpec::utc());
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

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn github_tile_activity_renders_20w_grid_months_and_blanks_future_days() {
        let mut unfiltered_daily_tokens = BTreeMap::new();
        unfiltered_daily_tokens.insert("2026-11-03".to_string(), 250_000);
        unfiltered_daily_tokens.insert("2026-11-15".to_string(), 1_000_000);
        unfiltered_daily_tokens.insert("2026-11-16".to_string(), 500_000);
        unfiltered_daily_tokens.insert("2026-11-17".to_string(), 750_000);
        // Clock-skewed future date after today_local ("2026-11-18" Wed) must stay blank and uncounted.
        unfiltered_daily_tokens.insert("2026-11-20".to_string(), 9_999_999);

        let report = StatsReport {
            today_local: "2026-11-18".to_string(),
            timezone: "UTC+08:00".to_string(),
            today_utc: "2026-11-18".to_string(),
            unfiltered_daily_tokens,
            ..Default::default()
        };

        let mut unicode_out = Vec::new();
        render_stats_report(&report, true, false, &mut unicode_out).unwrap();
        let unicode_text = String::from_utf8(unicode_out).unwrap();
        assert!(unicode_text
            .contains("Activity (20w): peak 1M on 2026-11-15 (Sun) · Less ·· ░░ ▒▒ ▓▓ ██ More"));
        assert!(
            unicode_text.contains("       Jul         Aug            Sep         Oct         Nov")
        );
        assert!(unicode_text
            .contains("  Mon  ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ▒▒"));
        assert!(unicode_text
            .contains("  Tue  ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ░░ ·· ▓▓"));
        assert!(unicode_text
            .contains("  Wed  ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ··"));
        assert!(unicode_text
            .contains("  Thu  ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ··\n"));
        assert!(unicode_text
            .contains("  Sun  ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ██\n"));

        // Trailing-month week 0 (2026-11-11 -> w=0 is 2026-06-29 Jun, w=1 is 2026-07-06 Jul) omits Jun collision,
        // and year-rollover (2027-01-06 -> w=19 is 2027-01-04 Jan) renders Jan cleanly.
        let trailing = StatsReport {
            today_local: "2026-11-11".to_string(),
            timezone: "UTC".to_string(),
            today_utc: "2026-11-11".to_string(),
            ..Default::default()
        };
        let mut trailing_out = Vec::new();
        render_stats_report(&trailing, true, true, &mut trailing_out).unwrap();
        let trailing_text = String::from_utf8(trailing_out).unwrap();
        assert!(!trailing_text.contains("JunJul"));
        assert!(trailing_text.contains("          Jul         Aug"));

        let rollover = StatsReport {
            today_local: "2027-01-06".to_string(),
            timezone: "UTC".to_string(),
            today_utc: "2027-01-06".to_string(),
            ..Default::default()
        };
        let mut rollover_out = Vec::new();
        render_stats_report(&rollover, false, true, &mut rollover_out).unwrap();
        let rollover_text = String::from_utf8(rollover_out).unwrap();
        assert!(rollover_text.contains("\x1b["));
        assert!(rollover_text.contains("Dec         Jan"));
    }

    #[test]
    fn pricing_catalog_prefers_exact_specificity_then_longest_deterministic_substring() {
        let mut models = BTreeMap::new();
        models.insert(
            "claude-sonnet".to_string(),
            ModelPricingRate {
                input_per_mtok: 1.0,
                output_per_mtok: 2.0,
                ..Default::default()
            },
        );
        models.insert(
            "claude-sonnet-4-6-thinking".to_string(),
            ModelPricingRate {
                input_per_mtok: 10.0,
                output_per_mtok: 20.0,
                ..Default::default()
            },
        );
        models.insert(
            "alpha-a".to_string(),
            ModelPricingRate {
                input_per_mtok: 3.0,
                ..Default::default()
            },
        );
        models.insert(
            "alpha-b".to_string(),
            ModelPricingRate {
                input_per_mtok: 9.0,
                ..Default::default()
            },
        );
        let catalog = PricingCatalog::from_models(models);

        // Exact specificity: response_model ("claude-sonnet-4-6-thinking") beats model_group ("claude-sonnet")
        // even though "claude-sonnet" < "claude-sonnet-4-6-thinking" in BTreeMap order.
        let (matched_key, cost) = catalog
            .estimate_cost(
                &[
                    "claude-sonnet-4-6-thinking",
                    "Claude Sonnet 4.6 (Thinking)",
                    "claude-sonnet",
                ],
                1_000_000,
                0,
                0,
                0,
            )
            .unwrap();
        assert_eq!(matched_key, "claude-sonnet-4-6-thinking");
        assert!((cost - 10.0).abs() < 1e-6);

        // Substring fallback: longest match wins, and equal-length tie breaks by BTreeMap key order ("alpha-a" < "alpha-b").
        let (tie_key, tie_cost) = catalog
            .estimate_cost(&["alpha-b-and-alpha-a-model"], 1_000_000, 0, 0, 0)
            .unwrap();
        assert_eq!(tie_key, "alpha-a");
        assert!((tie_cost - 3.0).abs() < 1e-6);
    }

    #[test]
    fn empty_or_schemaless_db_falls_back_to_transcript_jsonl() {
        let home = temp_home("empty_db_fallback");
        let uuid_empty = "55555555-5555-4555-8555-555555555555";
        let uuid_schemaless = "66666666-6666-4666-8666-666666666666";
        let conv_dir = home.join(".gemini/antigravity-cli/conversations");
        fs::create_dir_all(&conv_dir).unwrap();

        // 0-byte .db file.
        fs::write(conv_dir.join(format!("{uuid_empty}.db")), b"").unwrap();
        let t1 = home.join(format!(
            ".gemini/antigravity-cli/brain/{uuid_empty}/.system_generated/logs/transcript.jsonl"
        ));
        fs::create_dir_all(t1.parent().unwrap()).unwrap();
        fs::write(
            &t1,
            "{\"created_at\":\"2026-11-18T10:00:00Z\",\"step_index\":1,\"model\":\"fallback-a\",\"input_tokens\":500,\"output_tokens\":50}\n",
        )
        .unwrap();

        // Schema-less SQLite .db missing gen_metadata.
        let conn = Connection::open(conv_dir.join(format!("{uuid_schemaless}.db"))).unwrap();
        conn.execute("CREATE TABLE unrelated (id INTEGER PRIMARY KEY)", [])
            .unwrap();
        drop(conn);
        let t2 = home.join(format!(
            ".gemini/antigravity-cli/brain/{uuid_schemaless}/.system_generated/logs/transcript.jsonl"
        ));
        fs::create_dir_all(t2.parent().unwrap()).unwrap();
        fs::write(
            &t2,
            "{\"created_at\":\"2026-11-18T11:00:00Z\",\"step_index\":1,\"model\":\"fallback-b\",\"input_tokens\":700,\"output_tokens\":100}\n",
        )
        .unwrap();

        let now_secs = parse_rfc3339_unix_secs("2026-11-18T12:00:00Z").unwrap() as u64;
        let report = collect_stats_with_tz(&home, None, now_secs, &TimeZoneSpec::utc());
        assert_eq!(report.sqlite.db_files, 0);
        assert_eq!(report.totals.requests, 2);
        assert_eq!(report.totals.total_tokens, 1350);

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn render_stats_report_aligns_long_and_unicode_model_names() {
        let report = StatsReport {
            today_local: "2026-11-18".to_string(),
            timezone: "UTC".to_string(),
            today_utc: "2026-11-18".to_string(),
            models: vec![
                ModelStats {
                    model: "extremely-long-model-display-name-45-chars-xy".to_string(),
                    requests: 3,
                    total_tokens: 1_500,
                    output_tokens: 200,
                    thinking_tokens: 50,
                    median_tok_per_sec: Some(64.0),
                    median_ttft_secs: Some(0.5),
                    ..Default::default()
                },
                ModelStats {
                    model: "模型-α-wide".to_string(),
                    requests: 1,
                    total_tokens: 500,
                    output_tokens: 100,
                    thinking_tokens: 0,
                    median_tok_per_sec: Some(32.0),
                    median_ttft_secs: Some(0.8),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        let mut out = Vec::new();
        render_stats_report(&report, true, false, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        let hdr_idx = lines
            .iter()
            .position(|l| l.contains("Model") && l.contains("Turns"))
            .unwrap();
        let hdr_col = lines[hdr_idx].find(" Turns").unwrap();
        let row1_col = lines[hdr_idx + 1].find("     3").unwrap();
        let row2_col = lines[hdr_idx + 2].find("     1").unwrap();
        assert_eq!(
            lines[hdr_idx][..hdr_col].width(),
            lines[hdr_idx + 1][..row1_col].width()
        );
        assert_eq!(
            lines[hdr_idx][..hdr_col].width(),
            lines[hdr_idx + 2][..row2_col].width()
        );
    }

    #[test]
    fn models_dev_sync_respects_ttl_overrides_and_normalizes_model_names() {
        let home = temp_home("models_dev_ttl");
        let cache_dir = home.join("cache");
        let override_path = home.join("pricing.json");
        fs::create_dir_all(&cache_dir).unwrap();

        let sample_api = r#"{
          "reseller": {
            "models": {
              "gemini-4-hydrogen": { "cost": { "input": 99.0, "output": 99.0 } }
            }
          },
          "google": {
            "models": {
              "gemini-4-hydrogen": { "cost": { "input": 1.5, "output": 7.5, "cache_read": 0.075 } },
              "gemini-4-carbon": { "cost": { "input": 2.0, "output": 10.0, "cache_read": 0.1 } },
              "gemini-4-argon": { "cost": { "input": 2.0, "output": 10.0, "cache_read": 0.1 } }
            }
          },
          "anthropic": {
            "models": {
              "claude-opus-5-5": { "cost": { "input": 4.0, "output": 20.0, "cache_read": 0.2, "cache_write": 5.0 } }
            }
          }
        }"#;

        let t0 = 1_700_000_000;
        let mut fetch_calls = 0usize;
        let cat1 = PricingCatalog::load_effective_with(
            Some(&cache_dir),
            Some(&override_path),
            SyncMode::BlockingIfStale,
            t0,
            DEFAULT_PRICING_TTL_SECS,
            |_| {
                fetch_calls += 1;
                Ok(sample_api.to_string())
            },
        );
        assert_eq!(fetch_calls, 1);
        // Canonical provider (`google`) takes precedence over `reseller`.
        assert_eq!(cat1.models()["gemini-4-hydrogen"].input_per_mtok, 1.5);

        // Separator-normalized matching (`Claude Opus 5.5` -> `claude-opus-5-5`, `Gemini 4 Carbon (High)` -> `gemini-4-carbon`).
        let (_, opus_usd) = cat1
            .estimate_cost(&["Claude Opus 5.5"], 1_000_000, 0, 0, 0)
            .unwrap();
        assert!((opus_usd - 4.0).abs() < 1e-6);
        let (_, carbon_usd) = cat1
            .estimate_cost(&["Gemini 4 Carbon (High)"], 1_000_000, 0, 0, 0)
            .unwrap();
        assert!((carbon_usd - 2.0).abs() < 1e-6);

        // Orchestrator aliases map to their underlying Carbon/Argon models when no explicit rate exists.
        let (orch_c_key, orch_c_usd) = cat1
            .estimate_cost(
                &["gemini-pro-orchestrator", "Orchestrator (Carbon)"],
                1_000_000,
                0,
                0,
                0,
            )
            .unwrap();
        assert_eq!(orch_c_key, "gemini-4-carbon");
        assert!((orch_c_usd - 2.0).abs() < 1e-6);

        let (orch_a_key, orch_a_usd) = cat1
            .estimate_cost(
                &["gemini-pro-orchestrator-argon", "Orchestrator (Argon)"],
                1_000_000,
                0,
                0,
                0,
            )
            .unwrap();
        assert_eq!(orch_a_key, "gemini-4-argon");
        assert!((orch_a_usd - 2.0).abs() < 1e-6);

        // Within TTL: fetcher must NOT be called.
        let _cat2 = PricingCatalog::load_effective_with(
            Some(&cache_dir),
            Some(&override_path),
            SyncMode::BlockingIfStale,
            t0 + 100,
            DEFAULT_PRICING_TTL_SECS,
            |_| -> io::Result<String> { panic!("unexpected fetch while cache is fresh") },
        );

        // User override `pricing.json` overrides cached rate.
        fs::write(
            &override_path,
            r#"{"models":{"gemini-4-hydrogen":{"input_per_mtok":9.0,"output_per_mtok":9.0}}}"#,
        )
        .unwrap();
        let cat3 = PricingCatalog::load_effective_with(
            Some(&cache_dir),
            Some(&override_path),
            SyncMode::Offline,
            t0 + 100,
            DEFAULT_PRICING_TTL_SECS,
            |_| unreachable!(),
        );
        assert_eq!(cat3.models()["gemini-4-hydrogen"].input_per_mtok, 9.0);

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn user_override_mixed_case_and_separator_variant_beats_models_dev_cache() {
        let home = temp_home("pricing_override_precedence");
        let cache_dir = home.join("cache");
        let override_path = home.join("pricing.json");
        fs::create_dir_all(&cache_dir).unwrap();

        let sample_api = r#"{
          "google": {
            "models": {
              "gemini-4-hydrogen": { "cost": { "input": 1.5, "output": 7.5 } },
              "gemini-4-carbon": { "cost": { "input": 2.0, "output": 10.0 } }
            }
          }
        }"#;
        let t0 = 1_700_000_000;
        let _ = PricingCatalog::load_effective_with(
            Some(&cache_dir),
            None,
            SyncMode::BlockingIfStale,
            t0,
            DEFAULT_PRICING_TTL_SECS,
            |_| Ok(sample_api.to_string()),
        );

        // Mixed-case override ("Gemini-4-Hydrogen") beats lowercase cached "gemini-4-hydrogen".
        fs::write(
            &override_path,
            r#"{"models":{"Gemini-4-Hydrogen":{"input_per_mtok":12.0,"output_per_mtok":24.0}}}"#,
        )
        .unwrap();
        let cat_mixed = PricingCatalog::load_effective_with(
            Some(&cache_dir),
            Some(&override_path),
            SyncMode::Offline,
            t0 + 10,
            DEFAULT_PRICING_TTL_SECS,
            |_| unreachable!(),
        );
        let (k1, c1) = cat_mixed
            .estimate_cost(&["gemini-4-hydrogen"], 1_000_000, 0, 0, 0)
            .unwrap();
        assert_eq!(k1, "Gemini-4-Hydrogen");
        assert!((c1 - 12.0).abs() < 1e-6);

        // Space-separated override ("Gemini 4 Hydrogen") also beats cached "gemini-4-hydrogen".
        fs::write(
            &override_path,
            r#"{"models":{"Gemini 4 Hydrogen":{"input_per_mtok":15.0,"output_per_mtok":30.0}}}"#,
        )
        .unwrap();
        let cat_sep = PricingCatalog::load_effective_with(
            Some(&cache_dir),
            Some(&override_path),
            SyncMode::Offline,
            t0 + 10,
            DEFAULT_PRICING_TTL_SECS,
            |_| unreachable!(),
        );
        let (k2, c2) = cat_sep
            .estimate_cost(&["gemini-4-hydrogen"], 1_000_000, 0, 0, 0)
            .unwrap();
        assert_eq!(k2, "Gemini 4 Hydrogen");
        assert!((c2 - 15.0).abs() < 1e-6);

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn orchestrator_alias_matches_normalized_user_override() {
        let mut models = BTreeMap::new();
        models.insert(
            "Gemini 4 Carbon".to_string(),
            ModelPricingRate {
                input_per_mtok: 7.5,
                output_per_mtok: 15.0,
                ..Default::default()
            },
        );
        let catalog = PricingCatalog::from_models(models);

        let (matched_key, usd) = catalog
            .estimate_cost(
                &["gemini-pro-orchestrator", "Orchestrator (Carbon)"],
                1_000_000,
                0,
                0,
                0,
            )
            .unwrap();
        assert_eq!(matched_key, "Gemini 4 Carbon");
        assert!((usd - 7.5).abs() < 1e-6);
    }
}
