//! Read-only local usage, optional user-defined cost, and speed statistics (`agy-statusline stats`).
//!
//! Scans AGY local directories in read-only mode:
//! - `conversations/<uuid>.db` (`gen_metadata` joined with `steps.metadata`) as the
//!   authoritative primary source for turns, tokens, thinking tokens, model names,
//!   timestamps, decode speed, and TTFT.
//! - `brain/**/transcript.jsonl` as a deduplicated fallback ONLY for conversations that
//!   do not have a corresponding `<uuid>.db`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::db::{is_valid_uuid, resolve_agy_cli_dir, resolve_home_dir};
use crate::format::format_tokens;
use crate::protobuf::{
    extract_gen_metadata_record, extract_step_created_at_secs, MAX_TTFT_SECS,
    MAX_TURN_SPEED_TPS, MIN_TURN_DECODE_SECS,
};

const SECS_PER_DAY: i64 = 86_400;
const MAX_TRANSCRIPT_SCAN_DEPTH: usize = 4;
const HEATMAP_DAYS: i64 = 28;
const SQLITE_BUSY_TIMEOUT_MS: u64 = 25;

const SKIP_DIRS: &[&str] = &[
    "steps",
    "tasks",
    "scratch",
    "media",
    "artifacts",
    "node_modules",
    ".git",
    "__pycache__",
];

const UNICODE_HEATMAP: [char; 5] = ['·', '░', '▒', '▓', '█'];
const ASCII_HEATMAP: [char; 5] = ['.', '=', '-', '#', '*'];

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
    /// Per-model breakdown sorted by `total_tokens` descending.
    pub models: Vec<ModelStats>,
    /// Aggregate statistics from SQLite `conversations/*.db` (`gen_metadata`).
    pub sqlite: SqliteStats,
}

/// Per-million-token pricing rates supplied by user-configured `pricing.json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelPricingRate {
    /// USD per 1M uncached input tokens.
    #[serde(default)]
    pub input_per_mtok: f64,
    /// USD per 1M prompt cache read tokens.
    #[serde(default)]
    pub cache_read_per_mtok: f64,
    /// USD per 1M prompt cache write tokens.
    #[serde(default)]
    pub cache_write_per_mtok: f64,
    /// USD per 1M output tokens.
    #[serde(default)]
    pub output_per_mtok: f64,
}

/// Optional user-defined pricing catalog loaded from `$XDG_CONFIG_HOME/agy-statusline/pricing.json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PricingCatalog {
    /// Map of case-insensitive model name or substring pattern to per-million-token rates.
    #[serde(default)]
    pub models: HashMap<String, ModelPricingRate>,
}

impl PricingCatalog {
    /// Loads a [`PricingCatalog`] from `path`, returning an empty catalog if missing or invalid.
    pub fn load_from_file(path: &Path) -> Self {
        let Ok(raw) = fs::read_to_string(path) else {
            return Self::default();
        };
        serde_json::from_str::<Self>(&raw).unwrap_or_default()
    }

    /// Estimates USD cost for one turn if any candidate model string matches a valid entry.
    pub fn estimate_cost(
        &self,
        candidates: &[&str],
        input_tokens: u64,
        cache_read_tokens: u64,
        cache_write_tokens: u64,
        output_tokens: u64,
    ) -> Option<(String, f64)> {
        if self.models.is_empty() {
            return None;
        }
        let lowered_candidates: Vec<String> = candidates
            .iter()
            .map(|c| c.trim().to_ascii_lowercase())
            .filter(|c| !c.is_empty())
            .collect();
        if lowered_candidates.is_empty() {
            return None;
        }

        let mut exact_match: Option<(&str, &ModelPricingRate)> = None;
        let mut substr_match: Option<(usize, &str, &ModelPricingRate)> = None;

        for (pattern, rate) in &self.models {
            if !is_valid_rate(rate) {
                continue;
            }
            let pat_lower = pattern.trim().to_ascii_lowercase();
            if pat_lower.is_empty() {
                continue;
            }
            if lowered_candidates.iter().any(|c| c == &pat_lower) {
                exact_match = Some((pattern.as_str(), rate));
                break;
            }
            if lowered_candidates.iter().any(|c| c.contains(&pat_lower)) {
                let replace = match substr_match {
                    None => true,
                    Some((best_len, best_key, _)) => {
                        pat_lower.len() > best_len
                            || (pat_lower.len() == best_len && pattern.as_str() < best_key)
                    }
                };
                if replace {
                    substr_match = Some((pat_lower.len(), pattern.as_str(), rate));
                }
            }
        }

        let (matched_key, rate) = exact_match.or_else(|| substr_match.map(|(_, k, r)| (k, r)))?;
        let cost = ((input_tokens as f64) * rate.input_per_mtok
            + (cache_read_tokens as f64) * rate.cache_read_per_mtok
            + (cache_write_tokens as f64) * rate.cache_write_per_mtok
            + (output_tokens as f64) * rate.output_per_mtok)
            / 1_000_000.0;
        if !cost.is_finite() || cost < 0.0 {
            return None;
        }
        Some((matched_key.to_string(), round_usd_6(cost)))
    }
}

fn is_valid_rate(rate: &ModelPricingRate) -> bool {
    [
        rate.input_per_mtok,
        rate.cache_read_per_mtok,
        rate.cache_write_per_mtok,
        rate.output_per_mtok,
    ]
    .iter()
    .all(|v| v.is_finite() && *v >= 0.0)
}

/// Returns the default optional user pricing catalog path
/// (`$XDG_CONFIG_HOME/agy-statusline/pricing.json` or `~/.config/agy-statusline/pricing.json`).
pub fn default_pricing_path() -> Option<PathBuf> {
    let cfg_base = env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| resolve_home_dir().map(|h| h.join(".config")))?;
    Some(cfg_base.join("agy-statusline").join("pricing.json"))
}

fn round_usd_6(v: f64) -> f64 {
    if v.is_finite() {
        (v * 1_000_000.0).round() / 1_000_000.0
    } else {
        0.0
    }
}

fn is_leap_year(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Converts a civil Gregorian date to days since `1970-01-01` (Howard Hinnant algorithm).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = i64::from(month);
    let d = i64::from(day);
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Converts days since `1970-01-01` back into `(year, month, day)`.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let year = y + i64::from(m <= 2);
    (year, m as u32, d as u32)
}

fn format_day(day_idx: i64) -> String {
    let (y, m, d) = civil_from_days(day_idx);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Formats a Unix epoch timestamp (seconds) as a UTC `YYYY-MM-DD` date string.
pub fn unix_secs_to_utc_date(unix_secs: u64) -> String {
    format_day((unix_secs / 86_400) as i64)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TzTransition {
    time: i64,
    offset_secs: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DstRuleDate {
    JulianZeroBased(u32),
    JulianOneBased(u32),
    MonthWeekDay { month: u32, week: u32, weekday: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DstBoundary {
    date: DstRuleDate,
    secs_in_day: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PosixTzRule {
    std_offset_secs: i32,
    dst: Option<(i32, DstBoundary, DstBoundary)>,
}

/// Pure safe Rust timezone specification supporting RFC 8536 TZif v1/v2/v3 and POSIX TZ rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimeZoneSpec {
    transitions: Vec<TzTransition>,
    default_offset_secs: i32,
    post_rule: Option<PosixTzRule>,
}

impl Default for TimeZoneSpec {
    fn default() -> Self {
        Self::utc()
    }
}

impl TimeZoneSpec {
    /// Returns a UTC (`+00:00`) timezone specification.
    pub fn utc() -> Self {
        Self {
            transitions: Vec::new(),
            default_offset_secs: 0,
            post_rule: None,
        }
    }

    /// Returns a fixed-offset timezone specification (`offset_secs` east of UTC, e.g. `+28_800` for `UTC+08:00`).
    pub fn fixed_offset(offset_secs: i32) -> Self {
        Self {
            transitions: Vec::new(),
            default_offset_secs: offset_secs,
            post_rule: None,
        }
    }

    /// Parses an RFC 8536 TZif v1/v2/v3/v4 binary blob (e.g. `/etc/localtime` or `/usr/share/zoneinfo/*`).
    pub fn from_tzif(bytes: &[u8]) -> Option<Self> {
        let header = parse_tzif_header(bytes, 0)?;
        let v1_data_len = tzif_data_len(&header, 4)?;
        let v1_end = 44usize.checked_add(v1_data_len)?;
        if bytes.len() < v1_end {
            return None;
        }

        if header.version == 0 {
            return parse_tzif_block(&bytes[44..v1_end], &header, 4, None);
        }

        let v2_header = parse_tzif_header(bytes, v1_end)?;
        let v2_start = v1_end.checked_add(44)?;
        let v2_data_len = tzif_data_len(&v2_header, 8)?;
        let v2_end = v2_start.checked_add(v2_data_len)?;
        if bytes.len() < v2_end {
            return None;
        }

        let footer_rule = parse_tzif_footer(&bytes[v2_end..]);
        parse_tzif_block(&bytes[v2_start..v2_end], &v2_header, 8, footer_rule)
    }

    /// Loads the host local timezone from `$TZ` or `/etc/localtime`, falling back to UTC.
    pub fn local() -> Self {
        if let Ok(tz_val) = env::var("TZ") {
            let trimmed = tz_val.trim();
            if !trimmed.is_empty() {
                if let Some(spec) = Self::from_tz_env(trimmed) {
                    return spec;
                }
            }
        }
        if let Ok(bytes) = fs::read("/etc/localtime") {
            if let Some(spec) = Self::from_tzif(&bytes) {
                return spec;
            }
        }
        Self::utc()
    }

    /// Resolves a `TZ` environment variable string (zoneinfo path/name, ISO offset, or POSIX TZ string).
    pub fn from_tz_env(raw: &str) -> Option<Self> {
        let s = raw.trim().strip_prefix(':').unwrap_or(raw.trim()).trim();
        if s.is_empty() {
            return None;
        }
        if s.eq_ignore_ascii_case("UTC") || s.eq_ignore_ascii_case("GMT") {
            return Some(Self::utc());
        }
        if s.starts_with('/') {
            let bytes = fs::read(s).ok()?;
            return Self::from_tzif(&bytes);
        }
        if s.starts_with('+') || s.starts_with('-') {
            let offset = parse_iso_offset_secs(s)?;
            return Some(Self::fixed_offset(offset));
        }
        if !s.contains("..") && !s.contains(',') {
            let tzdir = env::var_os("TZDIR")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/usr/share/zoneinfo"));
            let candidate = tzdir.join(s);
            if candidate.is_file() {
                if let Ok(bytes) = fs::read(&candidate) {
                    if let Some(spec) = Self::from_tzif(&bytes) {
                        return Some(spec);
                    }
                }
            }
        }
        PosixTzRule::parse(s).map(|rule| Self {
            transitions: Vec::new(),
            default_offset_secs: rule.std_offset_secs,
            post_rule: Some(rule),
        })
    }

    /// Returns the UTC offset in seconds (east of UTC positive) at `unix_secs`.
    pub fn offset_secs_at(&self, unix_secs: i64) -> i32 {
        if self.transitions.is_empty() {
            return self
                .post_rule
                .map_or(self.default_offset_secs, |r| r.offset_secs_at(unix_secs));
        }
        let idx = self.transitions.partition_point(|t| t.time <= unix_secs);
        if idx == 0 {
            return self.default_offset_secs;
        }
        if idx == self.transitions.len() {
            if let Some(rule) = self.post_rule {
                return rule.offset_secs_at(unix_secs);
            }
        }
        self.transitions[idx - 1].offset_secs
    }

    /// Formats the active UTC offset at `unix_secs` (e.g., `"UTC+08:00"` or `"UTC"`).
    pub fn format_offset_at(&self, unix_secs: i64) -> String {
        let offset = self.offset_secs_at(unix_secs);
        if offset == 0 {
            return "UTC".to_string();
        }
        let sign = if offset >= 0 { '+' } else { '-' };
        let abs = offset.unsigned_abs();
        let hours = abs / 3_600;
        let mins = (abs % 3_600) / 60;
        let secs = abs % 60;
        if secs == 0 {
            format!("UTC{sign}{hours:02}:{mins:02}")
        } else {
            format!("UTC{sign}{hours:02}:{mins:02}:{secs:02}")
        }
    }

    /// Converts `unix_secs` into `(local_day_index, "YYYY-MM-DD")` using the offset at `unix_secs`.
    pub fn local_day_and_date(&self, unix_secs: i64) -> (i64, String) {
        let offset = i64::from(self.offset_secs_at(unix_secs));
        let local_secs = unix_secs.saturating_add(offset);
        let day_idx = local_secs.div_euclid(SECS_PER_DAY);
        (day_idx, format_day(day_idx))
    }
}

struct TzifHeader {
    version: u8,
    ttisutcnt: usize,
    ttisstdcnt: usize,
    leapcnt: usize,
    timecnt: usize,
    typecnt: usize,
    charcnt: usize,
}

fn read_be_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let slice = bytes.get(offset..offset + 4)?;
    Some(u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn read_be_i32(bytes: &[u8], offset: usize) -> Option<i32> {
    let slice = bytes.get(offset..offset + 4)?;
    Some(i32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn read_be_i64(bytes: &[u8], offset: usize) -> Option<i64> {
    let slice = bytes.get(offset..offset + 8)?;
    Some(i64::from_be_bytes([
        slice[0], slice[1], slice[2], slice[3], slice[4], slice[5], slice[6], slice[7],
    ]))
}

fn parse_tzif_header(bytes: &[u8], offset: usize) -> Option<TzifHeader> {
    let hdr = bytes.get(offset..offset + 44)?;
    if &hdr[0..4] != b"TZif" {
        return None;
    }
    let version = hdr[4];
    if !matches!(version, 0 | b'2' | b'3' | b'4') {
        return None;
    }
    let ttisutcnt = read_be_u32(hdr, 20)? as usize;
    let ttisstdcnt = read_be_u32(hdr, 24)? as usize;
    let leapcnt = read_be_u32(hdr, 28)? as usize;
    let timecnt = read_be_u32(hdr, 32)? as usize;
    let typecnt = read_be_u32(hdr, 36)? as usize;
    let charcnt = read_be_u32(hdr, 40)? as usize;
    if typecnt == 0 || charcnt == 0 {
        return None;
    }
    Some(TzifHeader {
        version,
        ttisutcnt,
        ttisstdcnt,
        leapcnt,
        timecnt,
        typecnt,
        charcnt,
    })
}

fn tzif_data_len(h: &TzifHeader, time_bytes: usize) -> Option<usize> {
    let times = h.timecnt.checked_mul(time_bytes)?;
    let types = h.timecnt;
    let ttinfos = h.typecnt.checked_mul(6)?;
    let chars = h.charcnt;
    let leaps = h.leapcnt.checked_mul(time_bytes + 4)?;
    let stds = h.ttisstdcnt;
    let uts = h.ttisutcnt;
    times
        .checked_add(types)?
        .checked_add(ttinfos)?
        .checked_add(chars)?
        .checked_add(leaps)?
        .checked_add(stds)?
        .checked_add(uts)
}

fn parse_tzif_block(
    block: &[u8],
    header: &TzifHeader,
    time_bytes: usize,
    post_rule: Option<PosixTzRule>,
) -> Option<TimeZoneSpec> {
    let times_len = header.timecnt.checked_mul(time_bytes)?;
    let types_start = times_len;
    let ttinfo_start = types_start.checked_add(header.timecnt)?;

    let mut ttinfos = Vec::with_capacity(header.typecnt);
    for i in 0..header.typecnt {
        let base = ttinfo_start + i * 6;
        let utoff = read_be_i32(block, base)?;
        let is_dst = *block.get(base + 4)? != 0;
        ttinfos.push((utoff, is_dst));
    }

    let default_offset_secs = ttinfos
        .iter()
        .find(|(_, is_dst)| !*is_dst)
        .or_else(|| ttinfos.first())
        .map_or(0, |(off, _)| *off);

    let mut transitions = Vec::with_capacity(header.timecnt);
    for i in 0..header.timecnt {
        let time = if time_bytes == 8 {
            read_be_i64(block, i * 8)?
        } else {
            i64::from(read_be_i32(block, i * 4)?)
        };
        let type_idx = usize::from(*block.get(types_start + i)?);
        let (offset_secs, _) = *ttinfos.get(type_idx)?;
        transitions.push(TzTransition { time, offset_secs });
    }

    Some(TimeZoneSpec {
        transitions,
        default_offset_secs,
        post_rule,
    })
}

fn parse_tzif_footer(tail: &[u8]) -> Option<PosixTzRule> {
    let s = std::str::from_utf8(tail).ok()?;
    let trimmed = s.trim_matches(|c| c == '\n' || c == '\r');
    if trimmed.is_empty() {
        return None;
    }
    PosixTzRule::parse(trimmed)
}

fn parse_iso_offset_secs(raw: &str) -> Option<i32> {
    let b = raw.as_bytes();
    let sign = match b.first().copied()? {
        b'+' => 1i32,
        b'-' => -1i32,
        _ => return None,
    };
    let rest = &raw[1..];
    let (h, m, s) = parse_hms_parts(rest)?;
    if h > 24 || m >= 60 || s >= 60 {
        return None;
    }
    Some(sign * (h * 3_600 + m * 60 + s))
}

fn parse_hms_parts(raw: &str) -> Option<(i32, i32, i32)> {
    let mut parts = raw.split(':');
    let h_str = parts.next()?;
    let h: i32 = h_str.parse().ok()?;
    let m: i32 = match parts.next() {
        Some(ms) => ms.parse().ok()?,
        None => 0,
    };
    let s: i32 = match parts.next() {
        Some(ss) => ss.parse().ok()?,
        None => 0,
    };
    if parts.next().is_some() {
        return None;
    }
    Some((h, m, s))
}

fn parse_tz_abbr(s: &str) -> Option<(&str, &str)> {
    if let Some(rest) = s.strip_prefix('<') {
        let end = rest.find('>')?;
        if end < 3 {
            return None;
        }
        Some((&rest[..end], &rest[end + 1..]))
    } else {
        let end = s
            .find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(s.len());
        if end < 3 {
            return None;
        }
        Some((&s[..end], &s[end..]))
    }
}

fn parse_posix_offset(s: &str) -> Option<(i32, &str)> {
    let (sign, after_sign) = if let Some(rest) = s.strip_prefix('+') {
        (-1i32, rest)
    } else if let Some(rest) = s.strip_prefix('-') {
        (1i32, rest)
    } else {
        (-1i32, s)
    };
    let end = after_sign
        .find(|c: char| !c.is_ascii_digit() && c != ':')
        .unwrap_or(after_sign.len());
    if end == 0 {
        return None;
    }
    let (h, m, sec) = parse_hms_parts(&after_sign[..end])?;
    if h > 167 || m >= 60 || sec >= 60 {
        return None;
    }
    Some((sign * (h * 3_600 + m * 60 + sec), &after_sign[end..]))
}

fn parse_dst_boundary(s: &str) -> Option<DstBoundary> {
    let (date_str, time_str) = match s.split_once('/') {
        Some((d, t)) => (d, Some(t)),
        None => (s, None),
    };
    let secs_in_day = match time_str {
        Some(t) => {
            let (neg, mag_str) = if let Some(rest) = t.strip_prefix('-') {
                (true, rest)
            } else {
                (false, t.strip_prefix('+').unwrap_or(t))
            };
            let (h, m, sec) = parse_hms_parts(mag_str)?;
            let mag = i64::from(h) * 3_600 + i64::from(m) * 60 + i64::from(sec);
            if neg {
                -mag
            } else {
                mag
            }
        }
        None => 2 * 3_600,
    };
    let date = if let Some(rest) = date_str.strip_prefix('M') {
        let mut parts = rest.split('.');
        let month: u32 = parts.next()?.parse().ok()?;
        let week: u32 = parts.next()?.parse().ok()?;
        let weekday: u32 = parts.next()?.parse().ok()?;
        if parts.next().is_some()
            || !(1..=12).contains(&month)
            || !(1..=5).contains(&week)
            || weekday > 6
        {
            return None;
        }
        DstRuleDate::MonthWeekDay {
            month,
            week,
            weekday,
        }
    } else if let Some(rest) = date_str.strip_prefix('J') {
        let j: u32 = rest.parse().ok()?;
        if !(1..=365).contains(&j) {
            return None;
        }
        DstRuleDate::JulianOneBased(j)
    } else {
        let n: u32 = date_str.parse().ok()?;
        if n > 365 {
            return None;
        }
        DstRuleDate::JulianZeroBased(n)
    };
    Some(DstBoundary { date, secs_in_day })
}

impl PosixTzRule {
    fn parse(raw: &str) -> Option<Self> {
        let s = raw.trim();
        let (_, after_std) = parse_tz_abbr(s)?;
        let (std_offset_secs, rest) = parse_posix_offset(after_std)?;
        if rest.is_empty() {
            return Some(Self {
                std_offset_secs,
                dst: None,
            });
        }
        let (_, after_dst) = parse_tz_abbr(rest)?;
        let (dst_offset_secs, rule_part) = match after_dst.strip_prefix(',') {
            Some(r) => (std_offset_secs + 3_600, r),
            None => {
                let (d_off, after_off) = parse_posix_offset(after_dst)?;
                (d_off, after_off.strip_prefix(',')?)
            }
        };
        let (start_str, end_str) = rule_part.split_once(',')?;
        let start = parse_dst_boundary(start_str)?;
        let end = parse_dst_boundary(end_str)?;
        Some(Self {
            std_offset_secs,
            dst: Some((dst_offset_secs, start, end)),
        })
    }

    fn transition_unix_secs(&self, year: i64, boundary: DstBoundary, prev_offset: i32) -> i64 {
        let day_idx = match boundary.date {
            DstRuleDate::JulianZeroBased(n) => {
                days_from_civil(year, 1, 1) + i64::from(n)
            }
            DstRuleDate::JulianOneBased(j) => {
                let jan1 = days_from_civil(year, 1, 1);
                let extra = if is_leap_year(year) && j >= 60 { 1 } else { 0 };
                jan1 + i64::from(j - 1 + extra)
            }
            DstRuleDate::MonthWeekDay {
                month,
                week,
                weekday,
            } => {
                let first_day = days_from_civil(year, month, 1);
                // 1970-01-01 was Thursday (weekday 4, with Sunday = 0).
                let first_wd = (first_day + 4).rem_euclid(7) as u32;
                let offset_days = (7 + weekday - first_wd) % 7;
                let dim = days_in_month(year, month);
                let mut dom = 1 + offset_days + (week - 1) * 7;
                if week == 5 {
                    while dom > dim {
                        dom -= 7;
                    }
                } else if dom > dim {
                    dom = dim;
                }
                days_from_civil(year, month, dom)
            }
        };
        day_idx * SECS_PER_DAY + boundary.secs_in_day - i64::from(prev_offset)
    }

    fn offset_secs_at(&self, unix_secs: i64) -> i32 {
        let Some((dst_offset_secs, start, end)) = self.dst else {
            return self.std_offset_secs;
        };
        let approx_day = (unix_secs + i64::from(self.std_offset_secs)).div_euclid(SECS_PER_DAY);
        let (year, _, _) = civil_from_days(approx_day);
        let start_ts = self.transition_unix_secs(year, start, self.std_offset_secs);
        let end_ts = self.transition_unix_secs(year, end, dst_offset_secs);
        let in_dst = if start_ts < end_ts {
            unix_secs >= start_ts && unix_secs < end_ts
        } else {
            unix_secs >= start_ts || unix_secs < end_ts
        };
        if in_dst {
            dst_offset_secs
        } else {
            self.std_offset_secs
        }
    }
}

fn parse_ascii_digits<T: std::str::FromStr>(bytes: &[u8]) -> Option<T> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(bytes).ok()?.parse::<T>().ok()
}

/// Parses an RFC3339 timestamp into Unix epoch seconds.
pub fn parse_rfc3339_unix_secs(raw: &str) -> Option<i64> {
    let s = raw.trim();
    let b = s.as_bytes();
    if b.len() < 20 {
        return None;
    }
    if b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') {
        return None;
    }
    if b[13] != b':' || b[16] != b':' {
        return None;
    }

    let year: i64 = parse_ascii_digits(&b[0..4])?;
    let month: u32 = parse_ascii_digits(&b[5..7])?;
    let day: u32 = parse_ascii_digits(&b[8..10])?;
    let hour: i64 = parse_ascii_digits(&b[11..13])?;
    let minute: i64 = parse_ascii_digits(&b[14..16])?;
    let second: i64 = parse_ascii_digits(&b[17..19])?;

    if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
        return None;
    }
    if !(0..=23).contains(&hour) || !(0..=59).contains(&minute) || !(0..=60).contains(&second) {
        return None;
    }

    let mut idx = 19;
    if b.get(idx) == Some(&b'.') {
        idx += 1;
        let frac_start = idx;
        while idx < b.len() && b[idx].is_ascii_digit() {
            idx += 1;
        }
        if idx == frac_start {
            return None;
        }
    }

    let tz_offset_secs = match b.get(idx).copied() {
        Some(b'Z' | b'z') if idx + 1 == b.len() => 0i64,
        Some(sign @ (b'+' | b'-')) => {
            let rest = &b[idx + 1..];
            let (tz_h, tz_m) = match rest.len() {
                5 if rest[2] == b':' => (
                    parse_ascii_digits::<i64>(&rest[0..2])?,
                    parse_ascii_digits::<i64>(&rest[3..5])?,
                ),
                4 => (
                    parse_ascii_digits::<i64>(&rest[0..2])?,
                    parse_ascii_digits::<i64>(&rest[2..4])?,
                ),
                _ => return None,
            };
            if !(0..=23).contains(&tz_h) || !(0..=59).contains(&tz_m) {
                return None;
            }
            let mag = tz_h * 3_600 + tz_m * 60;
            if sign == b'+' {
                mag
            } else {
                -mag
            }
        }
        _ => return None,
    };

    let days = days_from_civil(year, month, day);
    let clamped_sec = second.min(59);
    let local_secs = days * SECS_PER_DAY + hour * 3_600 + minute * 60 + clamped_sec;
    Some(local_secs - tz_offset_secs)
}

/// Parses an RFC3339 timestamp into `(utc_day_index, "YYYY-MM-DD")`.
pub fn parse_rfc3339_utc_day(raw: &str) -> Option<(i64, String)> {
    let utc_secs = parse_rfc3339_unix_secs(raw)?;
    let utc_day = utc_secs.div_euclid(SECS_PER_DAY);
    Some((utc_day, format_day(utc_day)))
}

fn parse_civil_date_day(raw: &str) -> Option<i64> {
    let b = raw.trim().as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let year: i64 = parse_ascii_digits(&b[0..4])?;
    let month: u32 = parse_ascii_digits(&b[5..7])?;
    let day: u32 = parse_ascii_digits(&b[8..10])?;
    if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
        return None;
    }
    Some(days_from_civil(year, month, day))
}

fn is_effort_qualifier(tag: &str) -> bool {
    let norm: String = tag
        .trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|c| !matches!(c, ' ' | '-' | '_'))
        .collect();
    matches!(
        norm.as_str(),
        "low"
            | "medium"
            | "med"
            | "high"
            | "xhigh"
            | "extrahigh"
            | "extra"
            | "minimal"
            | "max"
            | "none"
            | "off"
    )
}

/// Derives a clean display model group name strictly from AGY metadata without hardcoded models.
///
/// Strips trailing parenthetical effort tiers (e.g. `(Low)`, `(Medium)`, `(High)`) while
/// preserving non-effort qualifiers (e.g. `(Orchestrator)` or tier/backend labels).
pub fn derive_model_name(
    model_display_name: Option<&str>,
    response_model: Option<&str>,
    model_enum: u64,
) -> String {
    if let Some(disp) = model_display_name.map(str::trim).filter(|s| !s.is_empty()) {
        if let Some(open_idx) = disp.rfind(" (") {
            if let Some(inner) = disp[open_idx + 2..].strip_suffix(')') {
                if is_effort_qualifier(inner) {
                    let base = disp[..open_idx].trim();
                    if !base.is_empty() {
                        return base.to_string();
                    }
                }
            }
        }
        return disp.to_string();
    }
    if let Some(resp) = response_model.map(str::trim).filter(|s| !s.is_empty()) {
        return resp.to_string();
    }
    if model_enum > 0 {
        return format!("model#{model_enum}");
    }
    "unknown".to_string()
}

fn value_as_u64(v: Option<&Value>) -> u64 {
    match v {
        Some(Value::Number(n)) => {
            if let Some(u) = n.as_u64() {
                u
            } else if let Some(i) = n.as_i64() {
                i.max(0) as u64
            } else if let Some(f) = n.as_f64() {
                if f.is_finite() && f >= 0.0 {
                    f as u64
                } else {
                    0
                }
            } else {
                0
            }
        }
        Some(Value::String(s)) => s.trim().parse::<u64>().unwrap_or(0),
        _ => 0,
    }
}

fn value_as_positive_f64(v: Option<&Value>) -> Option<f64> {
    let f = v.and_then(Value::as_f64)?;
    (f.is_finite() && f > 0.0).then_some(f)
}

fn has_non_null_field(obj: &serde_json::Map<String, Value>, keys: &[&str]) -> bool {
    keys.iter()
        .any(|k| obj.get(*k).is_some_and(|v| !v.is_null()))
}

fn parse_duration_secs(v: Option<&Value>) -> Option<f64> {
    let val = v?;
    match val {
        Value::Number(n) => {
            let f = n.as_f64()?;
            (f.is_finite() && f >= 0.0).then_some(f)
        }
        Value::String(raw) => {
            let s = raw.trim();
            if s.is_empty() {
                return None;
            }
            let (num_str, scale) = if let Some(prefix) =
                s.strip_suffix("ms").or_else(|| s.strip_suffix("MS"))
            {
                (prefix.trim(), 0.001)
            } else if let Some(prefix) = s.strip_suffix('s').or_else(|| s.strip_suffix('S')) {
                (prefix.trim(), 1.0)
            } else {
                (s, 1.0)
            };
            let parsed = num_str.parse::<f64>().ok()?;
            let secs = parsed * scale;
            (secs.is_finite() && secs >= 0.0).then_some(secs)
        }
        _ => None,
    }
}

fn non_empty_str(v: Option<&Value>) -> Option<&str> {
    v.and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn extract_canonical_event_key(
    obj: &serde_json::Map<String, Value>,
    root_source: &str,
    default_cascade: &str,
    line_idx: usize,
) -> String {
    let usage_obj = obj
        .get("chatModel")
        .and_then(|c| c.get("usage"))
        .and_then(Value::as_object)
        .or_else(|| obj.get("usage").and_then(Value::as_object));

    let explicit = usage_obj
        .and_then(|u| {
            non_empty_str(u.get("responseId"))
                .or_else(|| non_empty_str(u.get("response_id")))
                .or_else(|| non_empty_str(u.get("messageId")))
                .or_else(|| non_empty_str(u.get("message_id")))
        })
        .or_else(|| non_empty_str(obj.get("responseId")))
        .or_else(|| non_empty_str(obj.get("response_id")))
        .or_else(|| non_empty_str(obj.get("messageId")))
        .or_else(|| non_empty_str(obj.get("message_id")))
        .or_else(|| non_empty_str(obj.get("callId")))
        .or_else(|| non_empty_str(obj.get("call_id")));

    if let Some(id) = explicit {
        return format!("id:{id}");
    }

    let source = non_empty_str(obj.get("source")).unwrap_or(root_source);
    let cascade = non_empty_str(obj.get("cascadeId"))
        .or_else(|| non_empty_str(obj.get("cascade_id")))
        .unwrap_or(default_cascade);

    let step_idx = obj
        .get("stepIndices")
        .and_then(Value::as_array)
        .and_then(|arr| arr.first())
        .and_then(Value::as_u64)
        .or_else(|| obj.get("stepIndex").and_then(Value::as_u64))
        .or_else(|| obj.get("step_index").and_then(Value::as_u64))
        .unwrap_or(line_idx as u64);

    format!("{source}:{cascade}#{step_idx}")
}

#[derive(Debug, Clone)]
struct UsageEvent {
    local_day: i64,
    local_date: String,
    model_group: String,
    pricing_key: Option<String>,
    input_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    output_tokens: u64,
    thinking_tokens: u64,
    total_tokens: u64,
    cost_usd: Option<f64>,
    decode_speed: Option<f64>,
    ttft_secs: Option<f64>,
}

#[derive(Debug, Clone)]
struct TranscriptJob {
    path: PathBuf,
    source: &'static str,
    cascade_hint: String,
}

fn is_skipped_dir_name(name: &str) -> bool {
    SKIP_DIRS.contains(&name)
}

fn canonical_or_self(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn cascade_hint_from_transcript_path(path: &Path) -> String {
    let components: Vec<String> = path
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    for i in 0..components.len() {
        if components[i].eq_ignore_ascii_case("brain") {
            if let Some(next) = components.get(i + 1) {
                return next.clone();
            }
        }
    }
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "session".to_string())
}

fn collect_transcript_jobs_in_brain(
    dir: &Path,
    source: &'static str,
    depth: usize,
    visited_dirs: &mut HashSet<PathBuf>,
    seen_files: &mut HashSet<PathBuf>,
    jobs: &mut Vec<TranscriptJob>,
) {
    if depth > MAX_TRANSCRIPT_SCAN_DEPTH || !dir.is_dir() {
        return;
    }
    let real_dir = canonical_or_self(dir);
    if !visited_dirs.insert(real_dir) {
        return;
    }

    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut sorted_entries: Vec<_> = entries.filter_map(Result::ok).collect();
    sorted_entries.sort_by_key(|e| e.file_name());

    for entry in sorted_entries {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if is_skipped_dir_name(&name_str) {
            continue;
        }
        let path = entry.path();
        let Ok(meta) = fs::metadata(&path) else {
            continue;
        };

        if meta.is_dir() {
            if depth == 0 {
                let std_log = path
                    .join(".system_generated")
                    .join("logs")
                    .join("transcript.jsonl");
                let direct_log = path.join("transcript.jsonl");
                let mut matched_fast = false;
                for candidate in [std_log, direct_log] {
                    if candidate.is_file() {
                        let real = canonical_or_self(&candidate);
                        if seen_files.insert(real) {
                            jobs.push(TranscriptJob {
                                cascade_hint: name_str.clone().into_owned(),
                                path: candidate,
                                source,
                            });
                        }
                        matched_fast = true;
                    }
                }
                if matched_fast {
                    continue;
                }
            }
            collect_transcript_jobs_in_brain(
                &path,
                source,
                depth + 1,
                visited_dirs,
                seen_files,
                jobs,
            );
        } else if meta.is_file() && name_str == "transcript.jsonl" {
            let real = canonical_or_self(&path);
            if seen_files.insert(real) {
                jobs.push(TranscriptJob {
                    cascade_hint: cascade_hint_from_transcript_path(&path),
                    path,
                    source,
                });
            }
        }
    }
}

fn parse_transcript_line(
    line: &str,
    job: &TranscriptJob,
    line_idx: usize,
    tz: &TimeZoneSpec,
    pricing: &PricingCatalog,
    covered_conversations: &HashSet<(&'static str, String)>,
) -> Option<(String, UsageEvent)> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let Value::Object(obj) = serde_json::from_str::<Value>(trimmed).ok()? else {
        return None;
    };

    if let Some(cid) =
        non_empty_str(obj.get("cascadeId")).or_else(|| non_empty_str(obj.get("cascade_id")))
    {
        if is_valid_uuid(cid)
            && covered_conversations.contains(&(job.source, cid.to_ascii_lowercase()))
        {
            return None;
        }
    }

    let created_at = non_empty_str(obj.get("created_at"))?;
    let unix_secs = parse_rfc3339_unix_secs(created_at)?;
    let (local_day, local_date) = tz.local_day_and_date(unix_secs);

    let cm_obj = obj.get("chatModel").and_then(Value::as_object);
    let usage_obj = cm_obj.and_then(|cm| cm.get("usage")).and_then(Value::as_object);

    let (input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, thinking_tokens) =
        if has_non_null_field(&obj, &["input_tokens", "output_tokens", "cache_read_tokens"]) {
            (
                value_as_u64(obj.get("input_tokens")),
                value_as_u64(obj.get("output_tokens")),
                value_as_u64(obj.get("cache_read_tokens")),
                value_as_u64(obj.get("cache_write_tokens")),
                value_as_u64(obj.get("thinking_output_tokens")),
            )
        } else if let Some(u) = usage_obj {
            (
                value_as_u64(u.get("inputTokens").or_else(|| u.get("input_tokens"))),
                value_as_u64(u.get("outputTokens").or_else(|| u.get("output_tokens"))),
                value_as_u64(
                    u.get("cacheReadTokens")
                        .or_else(|| u.get("cache_read_tokens")),
                ),
                value_as_u64(
                    u.get("cacheWriteTokens")
                        .or_else(|| u.get("cache_write_tokens")),
                ),
                value_as_u64(
                    u.get("thinkingOutputTokens")
                        .or_else(|| u.get("thinking_output_tokens")),
                ),
            )
        } else {
            (0, 0, 0, 0, 0)
        };

    let total_tokens = input_tokens
        .saturating_add(cache_read_tokens)
        .saturating_add(cache_write_tokens)
        .saturating_add(output_tokens);
    if total_tokens == 0 {
        return None;
    }

    let disp = cm_obj
        .and_then(|m| non_empty_str(m.get("modelDisplayName")))
        .or_else(|| non_empty_str(obj.get("modelDisplayName")));
    let resp = cm_obj
        .and_then(|m| {
            non_empty_str(m.get("responseModel"))
                .or_else(|| non_empty_str(m.get("model")))
                .or_else(|| non_empty_str(m.get("modelId")))
        })
        .or_else(|| non_empty_str(obj.get("responseModel")))
        .or_else(|| non_empty_str(obj.get("model")))
        .or_else(|| non_empty_str(obj.get("modelId")));
    let model_group = derive_model_name(disp, resp, 0);

    let native_cost = cm_obj
        .and_then(|cm| value_as_positive_f64(cm.get("modelCost")))
        .or_else(|| value_as_positive_f64(obj.get("cost_usd")))
        .or_else(|| value_as_positive_f64(obj.get("cost")));

    let (pricing_key, cost_usd) = if let Some(usd) = native_cost {
        (Some("native".to_string()), Some(round_usd_6(usd)))
    } else {
        let mut candidates = Vec::with_capacity(3);
        candidates.push(model_group.as_str());
        if let Some(r) = resp {
            candidates.push(r);
        }
        if let Some(d) = disp {
            candidates.push(d);
        }
        match pricing.estimate_cost(
            &candidates,
            input_tokens,
            cache_read_tokens,
            cache_write_tokens,
            output_tokens,
        ) {
            Some((k, usd)) => (Some(k), Some(usd)),
            None => (None, None),
        }
    };

    let streaming_secs = cm_obj
        .and_then(|cm| parse_duration_secs(cm.get("streamingDuration")))
        .or_else(|| parse_duration_secs(obj.get("streamingSec")));
    let decode_speed = streaming_secs.and_then(|secs| {
        if secs < MIN_TURN_DECODE_SECS || output_tokens == 0 {
            return None;
        }
        let speed = (output_tokens as f64) / secs;
        (speed.is_finite() && speed > 0.0 && speed < MAX_TURN_SPEED_TPS).then_some(speed)
    });

    let ttft_secs = cm_obj
        .and_then(|cm| parse_duration_secs(cm.get("timeToFirstToken")))
        .or_else(|| parse_duration_secs(obj.get("ttftSec")))
        .filter(|&t| t > 0.0 && t < MAX_TTFT_SECS);

    let event_key = extract_canonical_event_key(&obj, job.source, &job.cascade_hint, line_idx);

    Some((
        event_key,
        UsageEvent {
            local_day,
            local_date,
            model_group,
            pricing_key,
            input_tokens,
            cache_read_tokens,
            cache_write_tokens,
            output_tokens,
            thinking_tokens,
            total_tokens,
            cost_usd,
            decode_speed,
            ttft_secs,
        },
    ))
}

fn scan_transcripts(
    roots: &[(&'static str, PathBuf)],
    tz: &TimeZoneSpec,
    pricing: &PricingCatalog,
    covered_conversations: &HashSet<(&'static str, String)>,
) -> Vec<UsageEvent> {
    let mut visited_dirs = HashSet::new();
    let mut seen_files = HashSet::new();
    let mut jobs = Vec::new();

    for (source, root) in roots {
        let brain_dir = root.join("brain");
        collect_transcript_jobs_in_brain(
            &brain_dir,
            source,
            0,
            &mut visited_dirs,
            &mut seen_files,
            &mut jobs,
        );
    }

    let mut seen_events = HashSet::new();
    let mut events = Vec::new();
    let mut byte_buf = Vec::with_capacity(4096);

    for job in &jobs {
        if is_valid_uuid(&job.cascade_hint)
            && covered_conversations.contains(&(job.source, job.cascade_hint.to_ascii_lowercase()))
        {
            continue;
        }
        let Ok(file) = File::open(&job.path) else {
            continue;
        };
        let mut reader = BufReader::new(file);
        let mut line_idx = 0usize;

        loop {
            byte_buf.clear();
            let Ok(read_bytes) = reader.read_until(b'\n', &mut byte_buf) else {
                break;
            };
            if read_bytes == 0 {
                break;
            }
            if let Ok(line) = std::str::from_utf8(&byte_buf) {
                if let Some((event_key, event)) =
                    parse_transcript_line(line, job, line_idx, tz, pricing, covered_conversations)
                {
                    if seen_events.insert(event_key) {
                        events.push(event);
                    }
                }
            }
            line_idx += 1;
        }
    }

    events
}

struct DbTarget {
    source: &'static str,
    uuid: String,
    path: PathBuf,
}

fn scan_sqlite_conversations(
    roots: &[(&'static str, PathBuf)],
    tz: &TimeZoneSpec,
    pricing: &PricingCatalog,
) -> (SqliteStats, Vec<UsageEvent>, HashSet<(&'static str, String)>) {
    let mut seen_dbs = HashSet::new();
    let mut targets = Vec::new();
    let mut covered = HashSet::new();

    for (source, root) in roots {
        let conv_dir = root.join("conversations");
        let Ok(entries) = fs::read_dir(&conv_dir) else {
            continue;
        };
        let mut sorted: Vec<_> = entries.filter_map(Result::ok).collect();
        sorted.sort_by_key(|e| e.file_name());
        for entry in sorted {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.ends_with("-wal")
                || name_str.ends_with("-shm")
                || name_str.ends_with("-journal")
            {
                continue;
            }
            let Some(stem) = name_str.strip_suffix(".db") else {
                continue;
            };
            if !is_valid_uuid(stem) {
                continue;
            }
            let path = entry.path();
            let Ok(meta) = fs::metadata(&path) else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            let uuid_lower = stem.to_ascii_lowercase();
            covered.insert((*source, uuid_lower.clone()));
            let real = canonical_or_self(&path);
            if seen_dbs.insert(real) {
                targets.push(DbTarget {
                    source,
                    uuid: uuid_lower,
                    path,
                });
            }
        }
    }

    let mut stats = SqliteStats {
        db_files: targets.len() as u64,
        ..Default::default()
    };
    let mut speed_samples = Vec::new();
    let mut ttft_samples = Vec::new();
    let mut events = Vec::new();

    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    for target in &targets {
        let _ = (&target.source, &target.uuid);
        let db_mtime_secs = fs::metadata(&target.path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64);

        let Ok(conn) = Connection::open_with_flags(&target.path, flags) else {
            continue;
        };
        let _ = conn.busy_timeout(Duration::from_millis(SQLITE_BUSY_TIMEOUT_MS));

        let mut step_timestamps: HashMap<u64, i64> = HashMap::new();
        if let Ok(mut step_stmt) =
            conn.prepare("SELECT idx, metadata FROM steps WHERE metadata IS NOT NULL")
        {
            if let Ok(mut step_rows) = step_stmt.query([]) {
                while let Ok(Some(row)) = step_rows.next() {
                    if let (Ok(idx), Ok(meta_blob)) =
                        (row.get::<_, i64>(0), row.get::<_, Vec<u8>>(1))
                    {
                        if idx >= 0 {
                            if let Some(secs) = extract_step_created_at_secs(&meta_blob) {
                                step_timestamps.insert(idx as u64, secs);
                            }
                        }
                    }
                }
            }
        }

        let Ok(mut stmt) =
            conn.prepare("SELECT data FROM gen_metadata WHERE data IS NOT NULL ORDER BY idx ASC")
        else {
            continue;
        };
        let Ok(mut rows) = stmt.query([]) else {
            continue;
        };

        while let Ok(Some(row)) = rows.next() {
            let Ok(blob) = row.get::<_, Vec<u8>>(0) else {
                continue;
            };
            let Some(rec) = extract_gen_metadata_record(&blob) else {
                continue;
            };

            let decode_speed = rec.decode_speed_tps();
            if let Some(speed) = decode_speed {
                stats.completed_turns += 1;
                stats.total_output_tokens =
                    stats.total_output_tokens.saturating_add(rec.output_tokens);
                speed_samples.push(speed);
                if let Some(ttft) = rec.ttft_secs {
                    if ttft.is_finite() && ttft > 0.0 {
                        ttft_samples.push(ttft);
                    }
                }
            }

            let total_tokens = rec
                .input_tokens
                .saturating_add(rec.cache_read_tokens)
                .saturating_add(rec.cache_write_tokens)
                .saturating_add(rec.output_tokens);
            if total_tokens == 0 {
                continue;
            }

            let Some(ts_secs) = rec
                .first_step_index
                .and_then(|step_idx| step_timestamps.get(&step_idx).copied())
                .or(rec.chat_start_created_at_secs)
                .or(db_mtime_secs)
            else {
                continue;
            };

            let (local_day, local_date) = tz.local_day_and_date(ts_secs);
            let model_group =
                derive_model_name(rec.model_display_name, rec.response_model, rec.model_enum);

            let (pricing_key, cost_usd) = if let Some(usd) = rec.model_cost_usd {
                (Some("native".to_string()), Some(round_usd_6(usd)))
            } else {
                let mut candidates = Vec::with_capacity(3);
                candidates.push(model_group.as_str());
                if let Some(r) = rec.response_model {
                    candidates.push(r);
                }
                if let Some(d) = rec.model_display_name {
                    candidates.push(d);
                }
                match pricing.estimate_cost(
                    &candidates,
                    rec.input_tokens,
                    rec.cache_read_tokens,
                    rec.cache_write_tokens,
                    rec.output_tokens,
                ) {
                    Some((k, usd)) => (Some(k), Some(usd)),
                    None => (None, None),
                }
            };

            events.push(UsageEvent {
                local_day,
                local_date,
                model_group,
                pricing_key,
                input_tokens: rec.input_tokens,
                cache_read_tokens: rec.cache_read_tokens,
                cache_write_tokens: rec.cache_write_tokens,
                output_tokens: rec.output_tokens,
                thinking_tokens: rec.thinking_output_tokens,
                total_tokens,
                cost_usd,
                decode_speed,
                ttft_secs: rec.ttft_secs,
            });
        }
    }

    stats.ttft_samples = ttft_samples.len();
    stats.median_tok_per_sec = compute_median(&mut speed_samples);
    stats.median_ttft_secs = compute_median(&mut ttft_samples);
    (stats, events, covered)
}

fn compute_median(values: &mut Vec<f64>) -> Option<f64> {
    values.retain(|v| v.is_finite());
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    if values.len() % 2 == 1 {
        Some(values[mid])
    } else {
        Some((values[mid - 1] + values[mid]) / 2.0)
    }
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
    requests: u64,
    input_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    output_tokens: u64,
    thinking_tokens: u64,
    total_tokens: u64,
    priced_usd: f64,
    unpriced_requests: u64,
    speeds: Vec<f64>,
    ttfts: Vec<f64>,
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
    let mut daily_map: BTreeMap<i64, DailyUsage> = BTreeMap::new();
    let mut model_map: HashMap<String, ModelAccumulator> = HashMap::new();
    let mut first_day: Option<i64> = None;

    for ev in &events {
        first_day = Some(match first_day {
            Some(prev) => prev.min(ev.local_day),
            None => ev.local_day,
        });

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

        let day_entry = daily_map.entry(ev.local_day).or_insert_with(|| DailyUsage {
            date: ev.local_date.clone(),
            ..Default::default()
        });
        day_entry.requests += 1;
        day_entry.input_tokens = day_entry.input_tokens.saturating_add(ev.input_tokens);
        day_entry.cache_read_tokens = day_entry
            .cache_read_tokens
            .saturating_add(ev.cache_read_tokens);
        day_entry.cache_write_tokens = day_entry
            .cache_write_tokens
            .saturating_add(ev.cache_write_tokens);
        day_entry.output_tokens = day_entry.output_tokens.saturating_add(ev.output_tokens);
        day_entry.thinking_tokens = day_entry.thinking_tokens.saturating_add(ev.thinking_tokens);
        day_entry.total_tokens = day_entry.total_tokens.saturating_add(ev.total_tokens);
        match ev.cost_usd {
            Some(usd) => day_entry.priced_usd += usd,
            None => day_entry.unpriced_requests += 1,
        }

        let model_entry = model_map.entry(ev.model_group.clone()).or_default();
        if model_entry.pricing_key.is_none() {
            model_entry.pricing_key.clone_from(&ev.pricing_key);
        }
        model_entry.requests += 1;
        model_entry.input_tokens = model_entry.input_tokens.saturating_add(ev.input_tokens);
        model_entry.cache_read_tokens = model_entry
            .cache_read_tokens
            .saturating_add(ev.cache_read_tokens);
        model_entry.cache_write_tokens = model_entry
            .cache_write_tokens
            .saturating_add(ev.cache_write_tokens);
        model_entry.output_tokens = model_entry.output_tokens.saturating_add(ev.output_tokens);
        model_entry.thinking_tokens = model_entry
            .thinking_tokens
            .saturating_add(ev.thinking_tokens);
        model_entry.total_tokens = model_entry.total_tokens.saturating_add(ev.total_tokens);
        match ev.cost_usd {
            Some(usd) => model_entry.priced_usd += usd,
            None => model_entry.unpriced_requests += 1,
        }
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
        .map(|mut d| {
            d.priced_usd = round_usd_6(d.priced_usd);
            d
        })
        .collect();

    let mut models: Vec<ModelStats> = model_map
        .into_iter()
        .map(|(model, mut acc)| {
            let speed_samples = acc.speeds.len();
            let ttft_samples = acc.ttfts.len();
            ModelStats {
                model,
                pricing_key: acc.pricing_key,
                requests: acc.requests,
                input_tokens: acc.input_tokens,
                cache_read_tokens: acc.cache_read_tokens,
                cache_write_tokens: acc.cache_write_tokens,
                output_tokens: acc.output_tokens,
                thinking_tokens: acc.thinking_tokens,
                total_tokens: acc.total_tokens,
                priced_usd: round_usd_6(acc.priced_usd),
                unpriced_requests: acc.unpriced_requests,
                speed_samples,
                median_tok_per_sec: compute_median(&mut acc.speeds),
                ttft_samples,
                median_ttft_secs: compute_median(&mut acc.ttfts),
            }
        })
        .collect();

    models.sort_by(|a, b| {
        b.total_tokens
            .cmp(&a.total_tokens)
            .then_with(|| a.model.cmp(&b.model))
    });

    StatsReport {
        today_local,
        timezone,
        today_utc,
        filter_days,
        first_active_date: first_day.map(format_day),
        windows,
        totals,
        daily,
        models,
        sqlite,
    }
}

fn fmt_tok(value: u64) -> String {
    let mut s = String::new();
    format_tokens(value, &mut s);
    s
}

fn fmt_cache_hit(summary: &PeriodSummary) -> String {
    let denom = summary
        .input_tokens
        .saturating_add(summary.cache_read_tokens)
        .saturating_add(summary.cache_write_tokens);
    if denom == 0 {
        return "-".to_string();
    }
    let pct = (summary.cache_read_tokens as f64) * 100.0 / (denom as f64);
    format!("{pct:.1}%")
}

fn fmt_usd(usd: f64, priced_requests: u64, unpriced_requests: u64) -> String {
    if priced_requests == 0 {
        return "-".to_string();
    }
    let base = if usd > 0.0 && usd < 0.01 {
        format!("~${usd:.4}")
    } else {
        format!("~${usd:.2}")
    };
    if unpriced_requests > 0 {
        format!("{base} (+{unpriced_requests})")
    } else {
        base
    }
}

fn heatmap_level(tokens: u64, peak: u64) -> usize {
    if tokens == 0 || peak == 0 {
        return 0;
    }
    let pct = (tokens as f64) / (peak as f64);
    if pct <= 0.25 {
        1
    } else if pct <= 0.50 {
        2
    } else if pct <= 0.75 {
        3
    } else {
        4
    }
}

fn build_28d_heatmap(report: &StatsReport, ascii: bool) -> (String, Option<(String, u64)>) {
    let ref_date = if report.today_local.is_empty() {
        &report.today_utc
    } else {
        &report.today_local
    };
    let Some(today_idx) = parse_civil_date_day(ref_date) else {
        return (String::new(), None);
    };
    let daily_by_date: HashMap<&str, u64> = report
        .daily
        .iter()
        .map(|d| (d.date.as_str(), d.total_tokens))
        .collect();

    let start_idx = today_idx - (HEATMAP_DAYS - 1);
    let mut counts = Vec::with_capacity(HEATMAP_DAYS as usize);
    let mut peak: Option<(String, u64)> = None;

    for offset in 0..HEATMAP_DAYS {
        let date = format_day(start_idx + offset);
        let tokens = daily_by_date.get(date.as_str()).copied().unwrap_or(0);
        if tokens > 0 && peak.as_ref().is_none_or(|(_, p)| tokens > *p) {
            peak = Some((date.clone(), tokens));
        }
        counts.push(tokens);
    }

    let peak_tokens = peak.as_ref().map_or(0, |(_, p)| *p);
    let glyphs = if ascii {
        &ASCII_HEATMAP
    } else {
        &UNICODE_HEATMAP
    };
    let bar: String = counts
        .into_iter()
        .map(|t| glyphs[heatmap_level(t, peak_tokens)])
        .collect();
    (bar, peak)
}

fn style_wrap(no_color: bool, code: &str, text: &str) -> String {
    if no_color {
        text.to_string()
    } else {
        format!("\x1b[{code}m{text}\x1b[0m")
    }
}

fn write_window_row<W: Write>(
    out: &mut W,
    label: &str,
    summary: &PeriodSummary,
    show_cost: bool,
) -> io::Result<()> {
    if show_cost {
        writeln!(
            out,
            "  {:<10} {:>6}  {:>8}  {:>8}  {:>8}  {:>8}  {:>9}  {}",
            label,
            summary.requests,
            fmt_tok(summary.input_tokens),
            fmt_tok(summary.cache_read_tokens),
            fmt_tok(summary.output_tokens),
            fmt_tok(summary.thinking_tokens),
            fmt_cache_hit(summary),
            fmt_usd(
                summary.priced_usd,
                summary.priced_requests,
                summary.unpriced_requests
            ),
        )
    } else {
        writeln!(
            out,
            "  {:<10} {:>6}  {:>8}  {:>8}  {:>8}  {:>8}  {:>9}",
            label,
            summary.requests,
            fmt_tok(summary.input_tokens),
            fmt_tok(summary.cache_read_tokens),
            fmt_tok(summary.output_tokens),
            fmt_tok(summary.thinking_tokens),
            fmt_cache_hit(summary),
        )
    }
}

/// Renders a concise terminal summary of [`StatsReport`], respecting `no_color` and `ascii`.
pub fn render_stats_report<W: Write>(
    report: &StatsReport,
    no_color: bool,
    ascii: bool,
    out: &mut W,
) -> io::Result<()> {
    let date_label = if report.today_local.is_empty() {
        &report.today_utc
    } else {
        &report.today_local
    };
    let tz_label = if report.timezone.is_empty() {
        "UTC"
    } else {
        &report.timezone
    };
    let mut header = format!("agy-statusline stats · Local {date_label} ({tz_label})");
    if let Some(d) = report.filter_days {
        header.push_str(&format!(" · Last {d}d"));
    }
    if let Some(first) = &report.first_active_date {
        header.push_str(&format!(" · first {first}"));
    }
    writeln!(out, "{}", style_wrap(no_color, "1;38;5;78", &header))?;

    let show_cost =
        report.windows.all_time.priced_requests > 0 || report.totals.priced_requests > 0;

    let window_header = if show_cost {
        "  Window      Turns     Input     Cache    Output  Thinking  Cache Hit  Est. Cost"
    } else {
        "  Window      Turns     Input     Cache    Output  Thinking  Cache Hit"
    };
    writeln!(out, "{}", style_wrap(no_color, "2", window_header))?;

    if let Some(d) = report.filter_days {
        write_window_row(out, &format!("Last {d}d"), &report.totals, show_cost)?;
    }
    write_window_row(out, "Today", &report.windows.today, show_cost)?;
    write_window_row(out, "7d", &report.windows.last_7d, show_cost)?;
    write_window_row(out, "30d", &report.windows.last_30d, show_cost)?;
    write_window_row(out, "All-Time", &report.windows.all_time, show_cost)?;

    let (heatmap, peak) = build_28d_heatmap(report, ascii);
    let peak_note = match peak {
        Some((date, tok)) => format!("peak {} on {date}", fmt_tok(tok)),
        None => "no activity".to_string(),
    };
    writeln!(
        out,
        "\n{} [{}] ({peak_note})",
        style_wrap(no_color, "38;5;117", "Activity (28d):"),
        heatmap
    )?;

    if report.models.is_empty() {
        writeln!(out, "\nModels: none recorded")?;
    } else {
        let model_w = report
            .models
            .iter()
            .map(|m| m.model.len())
            .max()
            .unwrap_or(24)
            .clamp(24, 38);
        let model_hdr = if show_cost {
            format!(
                "  {:<model_w$} {:>6}  {:>8}  {:>8}  {:>8}  {:>9}  {:>8}  Est. Cost",
                "Model", "Turns", "Tokens", "Output", "Thinking", "Med tok/s", "Med TTFT"
            )
        } else {
            format!(
                "  {:<model_w$} {:>6}  {:>8}  {:>8}  {:>8}  {:>9}  {:>8}",
                "Model", "Turns", "Tokens", "Output", "Thinking", "Med tok/s", "Med TTFT"
            )
        };
        writeln!(out, "\n{}", style_wrap(no_color, "2", &model_hdr))?;

        for m in &report.models {
            let speed_str = m
                .median_tok_per_sec
                .map_or_else(|| "-".to_string(), |s| format!("{s:.1}"));
            let ttft_str = m
                .median_ttft_secs
                .map_or_else(|| "-".to_string(), |t| format!("{t:.2}s"));
            if show_cost {
                let priced_reqs = m.requests.saturating_sub(m.unpriced_requests);
                let cost_str = fmt_usd(m.priced_usd, priced_reqs, m.unpriced_requests);
                writeln!(
                    out,
                    "  {:<model_w$} {:>6}  {:>8}  {:>8}  {:>8}  {:>9}  {:>8}  {}",
                    m.model,
                    m.requests,
                    fmt_tok(m.total_tokens),
                    fmt_tok(m.output_tokens),
                    fmt_tok(m.thinking_tokens),
                    speed_str,
                    ttft_str,
                    cost_str
                )?;
            } else {
                writeln!(
                    out,
                    "  {:<model_w$} {:>6}  {:>8}  {:>8}  {:>8}  {:>9}  {:>8}",
                    m.model,
                    m.requests,
                    fmt_tok(m.total_tokens),
                    fmt_tok(m.output_tokens),
                    fmt_tok(m.thinking_tokens),
                    speed_str,
                    ttft_str
                )?;
            }
        }
    }

    let db_speed = report
        .sqlite
        .median_tok_per_sec
        .map_or_else(|| "-".to_string(), |s| format!("{s:.1} tok/s"));
    let db_ttft = report
        .sqlite
        .median_ttft_secs
        .map_or_else(|| "-".to_string(), |t| format!("{t:.2}s"));
    writeln!(
        out,
        "\nSQLite Turns: {} turns across {} DBs · med {} · TTFT {}",
        report.sqlite.completed_turns, report.sqlite.db_files, db_speed, db_ttft
    )?;

    Ok(())
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

    let pricing_path = default_pricing_path().or_else(|| {
        (!home.as_os_str().is_empty()).then(|| {
            home.join(".config")
                .join("agy-statusline")
                .join("pricing.json")
        })
    });
    let pricing = pricing_path
        .map(|p| PricingCatalog::load_from_file(&p))
        .unwrap_or_default();
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
    use std::sync::atomic::{AtomicU64, Ordering};

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
        out.extend_from_slice(b"LOC\0");
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
        out.extend_from_slice(b"LOC\0");
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
            derive_model_name(
                Some("Orchestrator (TierA)"),
                Some("pro-orchestrator"),
                1667
            ),
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
        assert!(empty_text.contains("Activity (28d): [............................] (no activity)"));
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
        let blob_50 = build_gen_blob(
            None,
            10,
            "tiny-model",
            "",
            100,
            200,
            100,
            40,
            2,
            1000,
            None,
        );
        conn.execute("INSERT INTO gen_metadata (idx, data) VALUES (1, ?1)", [blob_50])
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
}
