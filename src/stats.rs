//! Read-only local usage, reference API cost, and speed statistics (`agy-statusline stats`).
//!
//! Scans `~/.gemini/antigravity-cli` and `~/.gemini/antigravity` in read-only mode:
//! - `brain/**/transcript.jsonl` for per-request token counts, reference API cost,
//!   and optional per-model decode speed / TTFT from `chatModel`.
//! - `conversations/<uuid>.db` (`gen_metadata`) for completed turn count, median
//!   decode speed, and median TTFT via [`crate::protobuf::extract_turn_metrics`].

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::db::is_valid_uuid;
use crate::format::format_tokens;
use crate::protobuf::extract_turn_metrics;

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

/// Aggregated token and reference API cost totals for a time window.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PeriodSummary {
    /// Total deduplicated requests with non-zero token usage.
    pub requests: u64,
    /// Uncached input tokens.
    pub input_tokens: u64,
    /// Prompt cache read tokens.
    pub cache_read_tokens: u64,
    /// Output tokens (includes thinking tokens when reported by AGY).
    pub output_tokens: u64,
    /// Thinking/reasoning output tokens (subset of `output_tokens`).
    pub thinking_tokens: u64,
    /// Total tokens (`input_tokens + cache_read_tokens + output_tokens`).
    pub total_tokens: u64,
    /// Estimated reference API cost in USD across priced requests.
    pub priced_usd: f64,
    /// Count of requests matched to a known pricing tier.
    pub priced_requests: u64,
    /// Count of requests whose model has no public pricing entry.
    pub unpriced_requests: u64,
}

/// Standard fixed-window summaries (`today`, `last_7d`, `last_30d`, `all_time`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WindowSummaries {
    /// UTC today (`now_unix_secs` date).
    pub today: PeriodSummary,
    /// Last 7 UTC days including today.
    pub last_7d: PeriodSummary,
    /// Last 30 UTC days including today.
    pub last_30d: PeriodSummary,
    /// All recorded requests regardless of date.
    pub all_time: PeriodSummary,
}

/// Daily UTC token and cost aggregate.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DailyUsage {
    /// UTC date in `YYYY-MM-DD` format.
    pub date: String,
    /// Request count on this UTC date.
    pub requests: u64,
    /// Uncached input tokens.
    pub input_tokens: u64,
    /// Cache read tokens.
    pub cache_read_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Thinking tokens.
    pub thinking_tokens: u64,
    /// Total tokens (`input + cache_read + output`).
    pub total_tokens: u64,
    /// Estimated reference API cost in USD for priced requests.
    pub priced_usd: f64,
    /// Count of unpriced requests on this UTC date.
    pub unpriced_requests: u64,
}

/// Per-model usage, reference cost, and speed/TTFT leaderboard entry.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelStats {
    /// Canonical model key (or raw model string when unpriced).
    pub model: String,
    /// Matched pricing table key when publicly priced.
    pub pricing_key: Option<String>,
    /// Total requests attributed to this model.
    pub requests: u64,
    /// Uncached input tokens.
    pub input_tokens: u64,
    /// Cache read tokens.
    pub cache_read_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Thinking tokens.
    pub thinking_tokens: u64,
    /// Total tokens (`input + cache_read + output`).
    pub total_tokens: u64,
    /// Estimated reference API cost in USD.
    pub priced_usd: f64,
    /// Count of unpriced requests in this model bucket.
    pub unpriced_requests: u64,
    /// Number of valid transcript decode speed samples.
    pub speed_samples: usize,
    /// Median decode throughput in tokens/second, if speed samples exist.
    pub median_tok_per_sec: Option<f64>,
    /// Number of valid transcript TTFT samples.
    pub ttft_samples: usize,
    /// Median time-to-first-token in seconds, if TTFT samples exist.
    pub median_ttft_secs: Option<f64>,
}

/// Aggregate speed and TTFT statistics extracted from `conversations/*.db`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SqliteStats {
    /// Number of valid UUID `.db` files scanned.
    pub db_files: u64,
    /// Completed turns decoded from `gen_metadata`.
    pub completed_turns: u64,
    /// Sum of output tokens across decoded turns.
    pub total_output_tokens: u64,
    /// Median decode speed across all completed turns (tokens/second).
    pub median_tok_per_sec: Option<f64>,
    /// Count of turns with valid TTFT (`chat.11`).
    pub ttft_samples: usize,
    /// Median time-to-first-token across decoded turns (seconds).
    pub median_ttft_secs: Option<f64>,
}

/// Complete report returned by [`collect_stats`] and serialized by `--json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StatsReport {
    /// Reference UTC date (`YYYY-MM-DD`) derived from `now_unix_secs`.
    pub today_utc: String,
    /// Active `--days` filter (`None` for all-time).
    pub filter_days: Option<u32>,
    /// Earliest UTC date observed across active usage events.
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

#[derive(Debug, Clone, Copy)]
struct PricingRule {
    effective_from: Option<&'static str>,
    effective_to: Option<&'static str>,
    min_prompt_tokens: Option<u64>,
    max_prompt_tokens: Option<u64>,
    input_per_million: f64,
    output_per_million: f64,
    cache_read_per_million: f64,
}

const PRICING_GEMINI_3_8_FLASH: &[PricingRule] = &[
    PricingRule {
        effective_from: None,
        effective_to: Some("2026-12-31"),
        min_prompt_tokens: None,
        max_prompt_tokens: None,
        input_per_million: 0.75,
        output_per_million: 3.75,
        cache_read_per_million: 0.075,
    },
    PricingRule {
        effective_from: Some("2027-01-01"),
        effective_to: None,
        min_prompt_tokens: None,
        max_prompt_tokens: None,
        input_per_million: 1.50,
        output_per_million: 7.50,
        cache_read_per_million: 0.15,
    },
];

const PRICING_GEMINI_4_HYDROGEN: &[PricingRule] = &[PricingRule {
    effective_from: None,
    effective_to: None,
    min_prompt_tokens: None,
    max_prompt_tokens: None,
    input_per_million: 1.50,
    output_per_million: 7.50,
    cache_read_per_million: 0.075,
}];

const PRICING_GEMINI_4_CARBON_OR_ARGON: &[PricingRule] = &[
    PricingRule {
        effective_from: None,
        effective_to: Some("2026-11-30"),
        min_prompt_tokens: None,
        max_prompt_tokens: None,
        input_per_million: 2.00,
        output_per_million: 10.00,
        cache_read_per_million: 0.10,
    },
    PricingRule {
        effective_from: Some("2026-12-01"),
        effective_to: None,
        min_prompt_tokens: None,
        max_prompt_tokens: None,
        input_per_million: 4.00,
        output_per_million: 20.00,
        cache_read_per_million: 0.20,
    },
];

const PRICING_GEMINI_3_1_PRO_PREVIEW: &[PricingRule] = &[
    PricingRule {
        effective_from: None,
        effective_to: None,
        min_prompt_tokens: None,
        max_prompt_tokens: Some(200_000),
        input_per_million: 2.00,
        output_per_million: 12.00,
        cache_read_per_million: 0.20,
    },
    PricingRule {
        effective_from: None,
        effective_to: None,
        min_prompt_tokens: Some(200_001),
        max_prompt_tokens: None,
        input_per_million: 4.00,
        output_per_million: 18.00,
        cache_read_per_million: 0.40,
    },
];

const PRICING_GEMINI_3_5_FLASH_LITE: &[PricingRule] = &[PricingRule {
    effective_from: None,
    effective_to: None,
    min_prompt_tokens: None,
    max_prompt_tokens: None,
    input_per_million: 0.30,
    output_per_million: 2.50,
    cache_read_per_million: 0.03,
}];

const PRICING_GEMINI_2_5_PRO: &[PricingRule] = &[
    PricingRule {
        effective_from: None,
        effective_to: None,
        min_prompt_tokens: None,
        max_prompt_tokens: Some(200_000),
        input_per_million: 1.25,
        output_per_million: 10.00,
        cache_read_per_million: 0.125,
    },
    PricingRule {
        effective_from: None,
        effective_to: None,
        min_prompt_tokens: Some(200_001),
        max_prompt_tokens: None,
        input_per_million: 2.50,
        output_per_million: 15.00,
        cache_read_per_million: 0.25,
    },
];

const PRICING_CLAUDE_OPUS_5_5: &[PricingRule] = &[PricingRule {
    effective_from: None,
    effective_to: None,
    min_prompt_tokens: None,
    max_prompt_tokens: None,
    input_per_million: 4.00,
    output_per_million: 20.00,
    cache_read_per_million: 0.20,
}];

const PRICING_CLAUDE_SONNET_5_5: &[PricingRule] = &[PricingRule {
    effective_from: None,
    effective_to: None,
    min_prompt_tokens: None,
    max_prompt_tokens: None,
    input_per_million: 2.00,
    output_per_million: 10.00,
    cache_read_per_million: 0.10,
}];

fn pricing_rules_for(model_key: &str) -> Option<&'static [PricingRule]> {
    match model_key {
        "gemini-3.8-flash" => Some(PRICING_GEMINI_3_8_FLASH),
        "gemini-4-hydrogen" => Some(PRICING_GEMINI_4_HYDROGEN),
        "gemini-4-carbon" | "gemini-4-argon" => Some(PRICING_GEMINI_4_CARBON_OR_ARGON),
        "gemini-3.1-pro-preview" => Some(PRICING_GEMINI_3_1_PRO_PREVIEW),
        "gemini-3.5-flash-lite" => Some(PRICING_GEMINI_3_5_FLASH_LITE),
        "gemini-2.5-pro" => Some(PRICING_GEMINI_2_5_PRO),
        "claude-opus-5-5" => Some(PRICING_CLAUDE_OPUS_5_5),
        "claude-sonnet-5-5" => Some(PRICING_CLAUDE_SONNET_5_5),
        _ => None,
    }
}

fn is_word_boundary_byte(b: Option<u8>) -> bool {
    !matches!(b, Some(b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_'))
}

fn matches_orchestrator_argon(s: &str) -> bool {
    let bytes = s.as_bytes();
    let needle = b"gemini-pro-orchestrator";
    let mut offset = 0;
    while let Some(rel) = s[offset..].find("gemini-pro-orchestrator") {
        let start = offset + rel;
        let left_ok = start == 0 || !bytes[start - 1].is_ascii_alphanumeric();
        let after = &s[start + needle.len()..];
        let rest_after = if let Some(r) = after.strip_prefix("-argon") {
            r
        } else if let Some(r) = after.strip_prefix("-g1-pro") {
            r
        } else if let Some(r) = after.strip_prefix("-g1") {
            r
        } else {
            after
        };
        if left_ok && rest_after.is_empty() {
            return true;
        }
        offset = start + 1;
    }
    false
}

fn is_valid_claude_5_5_suffix(rest: &str) -> bool {
    let bytes = rest.as_bytes();
    let Some(&first) = bytes.first() else {
        return true;
    };
    if first.is_ascii_digit() {
        return false;
    }
    if first == b'.' && bytes.get(1).is_some_and(u8::is_ascii_digit) {
        return false;
    }
    if (first == b'-' || first == b'_') && bytes.get(1).is_some_and(u8::is_ascii_digit) {
        // Allow 8-digit YYYYMMDD date tags (`20xxxxxx`) while rejecting version suffixes (`5-5-1`).
        let tail = &bytes[1..];
        let has_8_digit_date = tail.len() >= 8
            && tail[0] == b'2'
            && tail[1] == b'0'
            && tail[2..8].iter().all(u8::is_ascii_digit)
            && !tail.get(8).is_some_and(u8::is_ascii_digit);
        return has_8_digit_date;
    }
    true
}

fn matches_claude_5_5_variant(s: &str, family: &str) -> bool {
    let bytes = s.as_bytes();
    let fam_len = family.len();
    let mut offset = 0;
    while let Some(rel) = s[offset..].find(family) {
        let pos = offset + rel;
        let after = pos + fam_len;
        if after + 4 <= bytes.len()
            && matches!(bytes[after], b'-' | b'_' | b' ')
            && bytes[after + 1] == b'5'
            && matches!(bytes[after + 2], b'.' | b'_' | b'-')
            && bytes[after + 3] == b'5'
            && is_valid_claude_5_5_suffix(&s[after + 4..])
        {
            return true;
        }
        if pos >= 4
            && bytes[pos - 4] == b'5'
            && matches!(bytes[pos - 3], b'.' | b'_' | b'-')
            && bytes[pos - 2] == b'5'
            && matches!(bytes[pos - 1], b'-' | b'_' | b' ')
            && is_word_boundary_byte(bytes.get(pos + fam_len).copied())
        {
            return true;
        }
        offset = pos + 1;
    }
    false
}

/// Normalizes a raw AGY model string into its canonical 2026 pricing key, if known.
pub fn normalize_model_id(raw: &str) -> Option<&'static str> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let s = trimmed.to_ascii_lowercase();
    if s.contains("3.8-flash") || s.contains("3.8 flash") || s.contains("flash-n") {
        return Some("gemini-3.8-flash");
    }
    if s.contains("4-hydrogen") || s.contains("4 hydrogen") || s.contains("hydrogen") {
        return Some("gemini-4-hydrogen");
    }
    if s.contains("4-carbon")
        || s.contains("4 carbon")
        || s.contains("carbon")
        || s.contains("m670")
    {
        return Some("gemini-4-carbon");
    }
    if s.contains("4-argon")
        || s.contains("4 argon")
        || s.contains("argon")
        || s.contains("m668")
        || matches_orchestrator_argon(&s)
    {
        return Some("gemini-4-argon");
    }
    if s.contains("3.1-pro") || s.contains("3.1 pro") {
        return Some("gemini-3.1-pro-preview");
    }
    if s.contains("3.5-flash-lite") || s.contains("3.5 flash-lite") || s.contains("flash-lite") {
        return Some("gemini-3.5-flash-lite");
    }
    if s.contains("2.5-pro") || s.contains("2.5 pro") {
        return Some("gemini-2.5-pro");
    }
    if matches_claude_5_5_variant(&s, "opus") {
        return Some("claude-opus-5-5");
    }
    if matches_claude_5_5_variant(&s, "sonnet") {
        return Some("claude-sonnet-5-5");
    }
    None
}

/// Estimates reference API cost in USD for a single request.
///
/// Returns `Some((pricing_key, cost_usd))` when the model maps to a known pricing
/// rule, or `None` when the model is unpriced.
pub fn estimate_request_cost_usd(
    raw_model: &str,
    utc_date: &str,
    input_tokens: u64,
    cache_read_tokens: u64,
    output_tokens: u64,
) -> Option<(&'static str, f64)> {
    let key = normalize_model_id(raw_model)?;
    let rules = pricing_rules_for(key)?;
    let total_prompt = input_tokens.saturating_add(cache_read_tokens);

    let rule = rules
        .iter()
        .find(|r| {
            r.effective_to.is_none_or(|to| utc_date <= to)
                && r.effective_from.is_none_or(|from| utc_date >= from)
                && r.max_prompt_tokens
                    .is_none_or(|max| total_prompt <= max)
                && r.min_prompt_tokens
                    .is_none_or(|min| total_prompt >= min)
        })
        .unwrap_or(&rules[0]);

    let cost = ((input_tokens as f64) * rule.input_per_million
        + (cache_read_tokens as f64) * rule.cache_read_per_million
        + (output_tokens as f64) * rule.output_per_million)
        / 1_000_000.0;
    Some((key, round_usd_6(cost)))
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

/// Converts days since `1970-01-01` back into a UTC `(year, month, day)`.
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

fn format_utc_day(day_idx: i64) -> String {
    let (y, m, d) = civil_from_days(day_idx);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Formats a Unix epoch timestamp (seconds) as a UTC `YYYY-MM-DD` date string.
pub fn unix_secs_to_utc_date(unix_secs: u64) -> String {
    format_utc_day((unix_secs / 86_400) as i64)
}

fn parse_ascii_digits<T: std::str::FromStr>(bytes: &[u8]) -> Option<T> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(bytes).ok()?.parse::<T>().ok()
}

/// Parses an RFC3339 timestamp into `(utc_day_index, "YYYY-MM-DD")`.
pub fn parse_rfc3339_utc_day(raw: &str) -> Option<(i64, String)> {
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
    let utc_secs = local_secs - tz_offset_secs;
    let utc_day = utc_secs.div_euclid(SECS_PER_DAY);
    Some((utc_day, format_utc_day(utc_day)))
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

fn extract_model_name(obj: &serde_json::Map<String, Value>) -> String {
    let cm = obj.get("chatModel").and_then(Value::as_object);
    let candidate = cm
        .and_then(|m| {
            non_empty_str(m.get("model"))
                .or_else(|| non_empty_str(m.get("responseModel")))
                .or_else(|| non_empty_str(m.get("modelDisplayName")))
                .or_else(|| non_empty_str(m.get("modelId")))
        })
        .or_else(|| non_empty_str(obj.get("model")))
        .or_else(|| non_empty_str(obj.get("responseModel")))
        .or_else(|| non_empty_str(obj.get("modelDisplayName")))
        .or_else(|| non_empty_str(obj.get("modelId")));

    candidate.unwrap_or("").to_string()
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
struct TranscriptEvent {
    utc_day: i64,
    utc_date: String,
    model_group: String,
    pricing_key: Option<&'static str>,
    input_tokens: u64,
    cache_read_tokens: u64,
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
) -> Option<(String, TranscriptEvent)> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let Value::Object(obj) = serde_json::from_str::<Value>(trimmed).ok()? else {
        return None;
    };

    let created_at = non_empty_str(obj.get("created_at"))?;
    let (utc_day, utc_date) = parse_rfc3339_utc_day(created_at)?;

    let cm_obj = obj.get("chatModel").and_then(Value::as_object);
    let usage_obj = cm_obj.and_then(|cm| cm.get("usage")).and_then(Value::as_object);

    let (input_tokens, output_tokens, cache_read_tokens, thinking_tokens) =
        if has_non_null_field(&obj, &["input_tokens", "output_tokens", "cache_read_tokens"]) {
            (
                value_as_u64(obj.get("input_tokens")),
                value_as_u64(obj.get("output_tokens")),
                value_as_u64(obj.get("cache_read_tokens")),
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
                    u.get("thinkingOutputTokens")
                        .or_else(|| u.get("thinking_output_tokens")),
                ),
            )
        } else {
            (0, 0, 0, 0)
        };

    let total_tokens = input_tokens
        .saturating_add(cache_read_tokens)
        .saturating_add(output_tokens);
    if total_tokens == 0 {
        return None;
    }

    let raw_model = extract_model_name(&obj);
    let cost_calc = estimate_request_cost_usd(
        &raw_model,
        &utc_date,
        input_tokens,
        cache_read_tokens,
        output_tokens,
    );
    let (pricing_key, cost_usd) = match cost_calc {
        Some((k, usd)) => (Some(k), Some(usd)),
        None => (None, None),
    };
    let model_group = if let Some(k) = pricing_key {
        k.to_string()
    } else if !raw_model.is_empty() {
        raw_model
    } else {
        "unknown".to_string()
    };

    let streaming_secs = cm_obj
        .and_then(|cm| parse_duration_secs(cm.get("streamingDuration")))
        .or_else(|| parse_duration_secs(obj.get("streamingSec")));
    let decode_speed = streaming_secs.and_then(|secs| {
        if secs < 0.02 || output_tokens == 0 {
            return None;
        }
        let speed = (output_tokens as f64) / secs;
        (speed.is_finite() && speed > 0.0 && speed < 2000.0).then_some(speed)
    });

    let ttft_secs = cm_obj
        .and_then(|cm| parse_duration_secs(cm.get("timeToFirstToken")))
        .or_else(|| parse_duration_secs(obj.get("ttftSec")))
        .filter(|&t| t > 0.0 && t < 600.0);

    let event_key = extract_canonical_event_key(&obj, job.source, &job.cascade_hint, line_idx);

    Some((
        event_key,
        TranscriptEvent {
            utc_day,
            utc_date,
            model_group,
            pricing_key,
            input_tokens,
            cache_read_tokens,
            output_tokens,
            thinking_tokens,
            total_tokens,
            cost_usd,
            decode_speed,
            ttft_secs,
        },
    ))
}

fn scan_transcripts(roots: &[(&'static str, PathBuf)]) -> Vec<TranscriptEvent> {
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
                if let Some((event_key, event)) = parse_transcript_line(line, job, line_idx) {
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

fn scan_sqlite_conversations(roots: &[(&'static str, PathBuf)]) -> SqliteStats {
    let mut seen_dbs = HashSet::new();
    let mut db_paths = Vec::new();

    for (_, root) in roots {
        let conv_dir = root.join("conversations");
        let Ok(entries) = fs::read_dir(&conv_dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
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
            let real = canonical_or_self(&path);
            if seen_dbs.insert(real) {
                db_paths.push(path);
            }
        }
    }

    let mut stats = SqliteStats {
        db_files: db_paths.len() as u64,
        ..Default::default()
    };
    let mut speed_samples = Vec::new();
    let mut ttft_samples = Vec::new();

    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    for db_path in &db_paths {
        let Ok(conn) = Connection::open_with_flags(db_path, flags) else {
            continue;
        };
        let _ = conn.busy_timeout(Duration::from_millis(SQLITE_BUSY_TIMEOUT_MS));
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
            let Some(tm) = extract_turn_metrics(&blob) else {
                continue;
            };
            stats.completed_turns += 1;
            stats.total_output_tokens = stats.total_output_tokens.saturating_add(tm.output_tokens);
            let speed = (tm.output_tokens as f64) / tm.decode_secs;
            if speed.is_finite() && speed > 0.0 {
                speed_samples.push(speed);
            }
            if let Some(ttft) = tm.ttft_secs {
                if ttft.is_finite() && ttft > 0.0 {
                    ttft_samples.push(ttft);
                }
            }
        }
    }

    stats.ttft_samples = ttft_samples.len();
    stats.median_tok_per_sec = compute_median(&mut speed_samples);
    stats.median_ttft_secs = compute_median(&mut ttft_samples);
    stats
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

fn accumulate_period(summary: &mut PeriodSummary, ev: &TranscriptEvent) {
    summary.requests += 1;
    summary.input_tokens = summary.input_tokens.saturating_add(ev.input_tokens);
    summary.cache_read_tokens = summary
        .cache_read_tokens
        .saturating_add(ev.cache_read_tokens);
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
    pricing_key: Option<&'static str>,
    requests: u64,
    input_tokens: u64,
    cache_read_tokens: u64,
    output_tokens: u64,
    thinking_tokens: u64,
    total_tokens: u64,
    priced_usd: f64,
    unpriced_requests: u64,
    speeds: Vec<f64>,
    ttfts: Vec<f64>,
}

/// Scans AGY local directories under `home` and builds a deterministic [`StatsReport`].
///
/// All timestamps are evaluated against `now_unix_secs` in UTC so callers and tests
/// do not depend on wall clock or `$HOME`.
pub fn collect_stats(home: &Path, days: Option<u32>, now_unix_secs: u64) -> StatsReport {
    let today_day_idx = (now_unix_secs / 86_400) as i64;
    let today_utc = format_utc_day(today_day_idx);
    let filter_days = days.filter(|&d| d > 0);
    let filter_cutoff = filter_days.map(|d| today_day_idx - i64::from(d - 1));

    let roots = [
        ("cli", home.join(".gemini").join("antigravity-cli")),
        ("desktop", home.join(".gemini").join("antigravity")),
    ];

    let events = scan_transcripts(&roots);
    let sqlite = scan_sqlite_conversations(&roots);

    let mut windows = WindowSummaries::default();
    let mut totals = PeriodSummary::default();
    let mut daily_map: BTreeMap<i64, DailyUsage> = BTreeMap::new();
    let mut model_map: HashMap<String, ModelAccumulator> = HashMap::new();
    let mut first_day: Option<i64> = None;

    for ev in &events {
        first_day = Some(match first_day {
            Some(prev) => prev.min(ev.utc_day),
            None => ev.utc_day,
        });

        accumulate_period(&mut windows.all_time, ev);
        if ev.utc_day == today_day_idx {
            accumulate_period(&mut windows.today, ev);
        }
        if ev.utc_day >= today_day_idx - 6 && ev.utc_day <= today_day_idx {
            accumulate_period(&mut windows.last_7d, ev);
        }
        if ev.utc_day >= today_day_idx - 29 && ev.utc_day <= today_day_idx {
            accumulate_period(&mut windows.last_30d, ev);
        }

        let in_filter = filter_cutoff
            .is_none_or(|cutoff| ev.utc_day >= cutoff && ev.utc_day <= today_day_idx);
        if !in_filter {
            continue;
        }

        accumulate_period(&mut totals, ev);

        let day_entry = daily_map.entry(ev.utc_day).or_insert_with(|| DailyUsage {
            date: ev.utc_date.clone(),
            ..Default::default()
        });
        day_entry.requests += 1;
        day_entry.input_tokens = day_entry.input_tokens.saturating_add(ev.input_tokens);
        day_entry.cache_read_tokens = day_entry
            .cache_read_tokens
            .saturating_add(ev.cache_read_tokens);
        day_entry.output_tokens = day_entry.output_tokens.saturating_add(ev.output_tokens);
        day_entry.thinking_tokens = day_entry.thinking_tokens.saturating_add(ev.thinking_tokens);
        day_entry.total_tokens = day_entry.total_tokens.saturating_add(ev.total_tokens);
        match ev.cost_usd {
            Some(usd) => day_entry.priced_usd += usd,
            None => day_entry.unpriced_requests += 1,
        }

        let model_entry = model_map.entry(ev.model_group.clone()).or_default();
        if model_entry.pricing_key.is_none() {
            model_entry.pricing_key = ev.pricing_key;
        }
        model_entry.requests += 1;
        model_entry.input_tokens = model_entry.input_tokens.saturating_add(ev.input_tokens);
        model_entry.cache_read_tokens = model_entry
            .cache_read_tokens
            .saturating_add(ev.cache_read_tokens);
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
                pricing_key: acc.pricing_key.map(str::to_string),
                requests: acc.requests,
                input_tokens: acc.input_tokens,
                cache_read_tokens: acc.cache_read_tokens,
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
        today_utc,
        filter_days,
        first_active_date: first_day.map(format_utc_day),
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

fn fmt_usd(usd: f64, priced_requests: u64, unpriced_requests: u64) -> String {
    if priced_requests == 0 && unpriced_requests > 0 {
        return "unpriced".to_string();
    }
    let base = if usd > 0.0 && usd < 0.01 {
        format!("~${usd:.4}")
    } else {
        format!("~${usd:.2}")
    };
    if unpriced_requests > 0 {
        format!("{base} (+{unpriced_requests} unpriced)")
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
    let Some((today_idx, _)) = parse_rfc3339_utc_day(&format!("{}T00:00:00Z", report.today_utc))
    else {
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
        let date = format_utc_day(start_idx + offset);
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
) -> io::Result<()> {
    writeln!(
        out,
        "  {:<10} {:>6}  {:>8}  {:>8}  {:>8}  {}",
        label,
        summary.requests,
        fmt_tok(summary.input_tokens),
        fmt_tok(summary.cache_read_tokens),
        fmt_tok(summary.output_tokens),
        fmt_usd(
            summary.priced_usd,
            summary.priced_requests,
            summary.unpriced_requests
        ),
    )
}

/// Renders a concise terminal summary of [`StatsReport`], respecting `no_color` and `ascii`.
pub fn render_stats_report<W: Write>(
    report: &StatsReport,
    no_color: bool,
    ascii: bool,
    out: &mut W,
) -> io::Result<()> {
    let mut header = format!("agy-statusline stats · UTC {}", report.today_utc);
    if let Some(d) = report.filter_days {
        header.push_str(&format!(" · Last {d}d"));
    }
    if let Some(first) = &report.first_active_date {
        header.push_str(&format!(" · first {first}"));
    }
    writeln!(out, "{}", style_wrap(no_color, "1;38;5;78", &header))?;

    writeln!(
        out,
        "{}",
        style_wrap(
            no_color,
            "2",
            "  Window       Reqs     Input     Cache    Output  Est. Cost"
        )
    )?;
    if let Some(d) = report.filter_days {
        write_window_row(out, &format!("Last {d}d"), &report.totals)?;
    }
    write_window_row(out, "Today", &report.windows.today)?;
    write_window_row(out, "7d", &report.windows.last_7d)?;
    write_window_row(out, "30d", &report.windows.last_30d)?;
    write_window_row(out, "All-Time", &report.windows.all_time)?;

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
        writeln!(
            out,
            "\n{}",
            style_wrap(
                no_color,
                "2",
                "  Model                     Reqs    Tokens  Est. Cost   Med tok/s  Med TTFT"
            )
        )?;
        for m in &report.models {
            let priced_reqs = m.requests.saturating_sub(m.unpriced_requests);
            let cost_str = fmt_usd(m.priced_usd, priced_reqs, m.unpriced_requests);
            let speed_str = m
                .median_tok_per_sec
                .map_or_else(|| "-".to_string(), |s| format!("{s:.1}"));
            let ttft_str = m
                .median_ttft_secs
                .map_or_else(|| "-".to_string(), |t| format!("{t:.2}s"));
            writeln!(
                out,
                "  {:<24} {:>5}  {:>8}  {:<10}  {:>9}  {:>8}",
                m.model,
                m.requests,
                fmt_tok(m.total_tokens),
                cost_str,
                speed_str,
                ttft_str
            )?;
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

/// Runs the `agy-statusline stats` subcommand against the resolved home directory.
pub fn run_stats_command<W: Write>(
    home: &Path,
    days: Option<u32>,
    json: bool,
    no_color: bool,
    ascii: bool,
    now_unix_secs: u64,
    out: &mut W,
) -> io::Result<()> {
    let report = collect_stats(home, days, now_unix_secs);
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

    fn create_gen_metadata_db(db_path: &Path, blobs: &[&[u8]]) {
        fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let conn = Connection::open(db_path).unwrap();
        conn.execute(
            "CREATE TABLE gen_metadata (idx INTEGER PRIMARY KEY, data BLOB)",
            [],
        )
        .unwrap();
        for (i, blob) in blobs.iter().enumerate() {
            conn.execute(
                "INSERT INTO gen_metadata (idx, data) VALUES (?1, ?2)",
                rusqlite::params![i as i64 + 1, *blob],
            )
            .unwrap();
        }
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
    fn tiered_pricing_and_model_normalization() {
        assert_eq!(normalize_model_id("Gemini 3.8 Flash"), Some("gemini-3.8-flash"));
        assert_eq!(
            normalize_model_id("gemini-pro-orchestrator"),
            Some("gemini-4-argon")
        );
        assert_eq!(normalize_model_id("m670-preview"), Some("gemini-4-carbon"));
        assert_eq!(
            normalize_model_id("claude-opus-5-5-20260101"),
            Some("claude-opus-5-5")
        );
        assert_eq!(normalize_model_id("claude-opus-5-5-2"), None);
        assert_eq!(normalize_model_id("unknown-model"), None);

        // <=200K tier for gemini-3.1-pro-preview: $2/1M in, $0.20/1M cache, $12/1M out
        let (k_low, cost_low) =
            estimate_request_cost_usd("gemini-3.1-pro-preview", "2026-11-18", 100_000, 100_000, 1_000)
                .unwrap();
        assert_eq!(k_low, "gemini-3.1-pro-preview");
        assert!((cost_low - 0.232).abs() < 1e-6);

        // >200K tier (200,001 total prompt tokens): $4/1M in, $0.40/1M cache, $18/1M out
        let (k_high, cost_high) = estimate_request_cost_usd(
            "gemini-3.1-pro-preview",
            "2026-11-18",
            100_001,
            100_000,
            1_000,
        )
        .unwrap();
        assert_eq!(k_high, "gemini-3.1-pro-preview");
        assert!((cost_high - 0.458004).abs() < 1e-6);

        // Effective date switch for gemini-4-carbon (<=2026-11-30 vs >=2026-12-01)
        let (_, nov_cost) =
            estimate_request_cost_usd("gemini-4-carbon", "2026-11-30", 1_000_000, 0, 0).unwrap();
        let (_, dec_cost) =
            estimate_request_cost_usd("gemini-4-carbon", "2026-12-01", 1_000_000, 0, 0).unwrap();
        assert!((nov_cost - 2.0).abs() < 1e-6);
        assert!((dec_cost - 4.0).abs() < 1e-6);

        // Tiered pricing for gemini-2.5-pro and date switch for gemini-3.8-flash
        let (_, p25_low) =
            estimate_request_cost_usd("gemini-2.5-pro", "2026-11-18", 200_000, 0, 0).unwrap();
        let (_, p25_high) =
            estimate_request_cost_usd("gemini-2.5-pro", "2026-11-18", 200_001, 0, 0).unwrap();
        assert!((p25_low - 0.25).abs() < 1e-6);
        assert!((p25_high - 0.500003).abs() < 1e-6);

        let (_, f38_2026) =
            estimate_request_cost_usd("gemini-3.8-flash", "2026-12-31", 1_000_000, 0, 0).unwrap();
        let (_, f38_2027) =
            estimate_request_cost_usd("gemini-3.8-flash", "2027-01-01", 1_000_000, 0, 0).unwrap();
        assert!((f38_2026 - 0.75).abs() < 1e-6);
        assert!((f38_2027 - 1.50).abs() < 1e-6);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_cycles_and_duplicate_links_do_not_loop_or_double_count() {
        let home = temp_home("symlink");
        let brain = home.join(".gemini/antigravity-cli/brain");
        let real_log = brain.join("session-a/transcript.jsonl");
        fs::create_dir_all(real_log.parent().unwrap()).unwrap();
        let ev = r#"{"created_at":"2026-11-18T10:00:00Z","step_index":0,"chatModel":{"model":"claude-sonnet-5-5","usage":{"inputTokens":1000,"cacheReadTokens":0,"outputTokens":100,"responseId":"sym-1"}}}"#;
        fs::write(&real_log, format!("{ev}\n")).unwrap();

        std::os::unix::fs::symlink(&brain, brain.join("cycle")).unwrap();
        let desk_session = home.join(".gemini/antigravity/brain/session-a");
        fs::create_dir_all(&desk_session).unwrap();
        std::os::unix::fs::symlink(&real_log, desk_session.join("transcript.jsonl")).unwrap();

        let (day_idx, _) = parse_rfc3339_utc_day("2026-11-18T00:00:00Z").unwrap();
        let report = collect_stats(&home, None, (day_idx as u64) * 86_400);
        assert_eq!(report.totals.requests, 1);
        assert_eq!(report.totals.total_tokens, 1100);
        assert_eq!(report.models[0].model, "claude-sonnet-5-5");

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn deduplicates_events_and_computes_per_model_median_speed() {
        let home = temp_home("dedup");
        let cli_log = home
            .join(".gemini/antigravity-cli/brain/11111111-1111-4111-8111-111111111111/.system_generated/logs/transcript.jsonl");
        let desk_log = home
            .join(".gemini/antigravity/brain/11111111-1111-4111-8111-111111111111/transcript.jsonl");
        let skipped_log = home
            .join(".gemini/antigravity-cli/brain/11111111-1111-4111-8111-111111111111/steps/transcript.jsonl");

        fs::create_dir_all(cli_log.parent().unwrap()).unwrap();
        fs::create_dir_all(desk_log.parent().unwrap()).unwrap();
        fs::create_dir_all(skipped_log.parent().unwrap()).unwrap();

        let ev_1 = r#"{"created_at":"2026-11-18T10:00:00Z","step_index":1,"chatModel":{"model":"gemini-3.8-flash","streamingDuration":"2.0s","timeToFirstToken":"0.5s","usage":{"inputTokens":1000,"cacheReadTokens":500,"outputTokens":100,"responseId":"resp-1"}}}"#;
        let ev_2 = r#"{"created_at":"2026-11-18T11:00:00Z","step_index":2,"chatModel":{"model":"gemini-3.8-flash","streamingDuration":"1.0s","timeToFirstToken":"1.5s","usage":{"inputTokens":2000,"cacheReadTokens":0,"outputTokens":100,"responseId":"resp-2"}}}"#;
        let ev_3 = r#"{"created_at":"2026-11-18T12:00:00Z","step_index":3,"chatModel":{"model":"gemini-3.8-flash","streamingDuration":"1.0s","timeToFirstToken":"1.0s","usage":{"inputTokens":500,"cacheReadTokens":0,"outputTokens":200,"responseId":"resp-3"}}}"#;
        let ev_unpriced = r#"{"created_at":"2026-11-10T10:00:00Z","source":"MODEL","cascadeId":"c-old","step_index":0,"model":"custom-local-7b","input_tokens":400,"output_tokens":100,"cache_read_tokens":0}"#;

        fs::write(&cli_log, format!("{ev_1}\n{ev_2}\n{ev_3}\n{ev_unpriced}\n")).unwrap();
        // Duplicate responseId="resp-1" in desktop log + duplicate fallback key
        fs::write(&desk_log, format!("{ev_1}\n{ev_unpriced}\nnot-json\n")).unwrap();
        // Inside skipped `steps/` directory: must be ignored
        fs::write(&skipped_log, format!("{ev_2}\n")).unwrap();

        // 2026-11-18T15:00:00Z
        let (day_idx, _) = parse_rfc3339_utc_day("2026-11-18T00:00:00Z").unwrap();
        let now_secs = (day_idx as u64) * 86_400 + 54_000;

        let report = collect_stats(&home, None, now_secs);
        assert_eq!(report.today_utc, "2026-11-18");
        assert_eq!(report.first_active_date.as_deref(), Some("2026-11-10"));
        assert_eq!(report.windows.today.requests, 3);
        assert_eq!(report.windows.today.input_tokens, 3500);
        assert_eq!(report.windows.today.cache_read_tokens, 500);
        assert_eq!(report.windows.today.output_tokens, 400);
        assert_eq!(report.windows.all_time.requests, 4);
        assert_eq!(report.windows.all_time.priced_requests, 3);
        assert_eq!(report.windows.all_time.unpriced_requests, 1);

        // Models sorted by total_tokens descending: gemini-3.8-flash (4400) then custom-local-7b (500)
        assert_eq!(report.models.len(), 2);
        let flash = &report.models[0];
        assert_eq!(flash.model, "gemini-3.8-flash");
        assert_eq!(flash.speed_samples, 3);
        // Speeds: [50.0, 100.0, 200.0] -> median 100.0
        assert!((flash.median_tok_per_sec.unwrap() - 100.0).abs() < 1e-6);
        // TTFTs: [0.5, 1.0, 1.5] -> median 1.0
        assert!((flash.median_ttft_secs.unwrap() - 1.0).abs() < 1e-6);

        // --days 3 excludes 2026-11-10
        let filtered = collect_stats(&home, Some(3), now_secs);
        assert_eq!(filtered.totals.requests, 3);
        assert_eq!(filtered.models.len(), 1);
        assert_eq!(filtered.windows.all_time.requests, 4);

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn scans_sqlite_conversations_and_renders_empty_and_populated_gracefully() {
        let home = temp_home("sqlite");
        let (day_idx, _) = parse_rfc3339_utc_day("2026-11-18T00:00:00Z").unwrap();
        let now_secs = (day_idx as u64) * 86_400;

        // Empty directory must succeed cleanly with zero counts.
        let empty = collect_stats(&home, None, now_secs);
        assert_eq!(empty.totals.requests, 0);
        assert_eq!(empty.sqlite.db_files, 0);
        let mut empty_out = Vec::new();
        render_stats_report(&empty, true, true, &mut empty_out).unwrap();
        let empty_text = String::from_utf8(empty_out).unwrap();
        assert!(!empty_text.contains('\x1b'));
        assert!(empty_text.contains("Activity (28d): [............................] (no activity)"));
        assert!(empty_text.contains("Models: none recorded"));

        // Add two valid UUID DBs + companion -wal file + invalid filename DB.
        let conv_dir = home.join(".gemini/antigravity-cli/conversations");
        let blob_50: &[u8] = &[
            0x0a, 0x0c, 0x22, 0x02, 0x18, 0x64, 0x5a, 0x02, 0x08, 0x01, 0x62, 0x02, 0x08, 0x02,
        ]; // 100 tok / 2s = 50 tok/s, ttft 1s
        let blob_100: &[u8] = &[
            0x0a, 0x0c, 0x22, 0x02, 0x18, 0x64, 0x5a, 0x02, 0x08, 0x03, 0x62, 0x02, 0x08, 0x01,
        ]; // 100 tok / 1s = 100 tok/s, ttft 3s
        create_gen_metadata_db(
            &conv_dir.join("8d0371e1-5a05-423b-a8bc-ed75995f9a8b.db"),
            &[blob_50, blob_100],
        );
        fs::write(
            conv_dir.join("8d0371e1-5a05-423b-a8bc-ed75995f9a8b.db-wal"),
            b"junk",
        )
        .unwrap();
        fs::write(conv_dir.join("not-a-uuid.db"), b"junk").unwrap();

        let populated = collect_stats(&home, None, now_secs);
        assert_eq!(populated.sqlite.db_files, 1);
        assert_eq!(populated.sqlite.completed_turns, 2);
        assert_eq!(populated.sqlite.total_output_tokens, 200);
        // Median of [50, 100] = 75.0; median of [1, 3] = 2.0
        assert!((populated.sqlite.median_tok_per_sec.unwrap() - 75.0).abs() < 1e-6);
        assert!((populated.sqlite.median_ttft_secs.unwrap() - 2.0).abs() < 1e-6);

        let _ = fs::remove_dir_all(&home);
    }
}
