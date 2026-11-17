//! Pure metric derivation for cache hit rate, context tokens, and quota pacing.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::statusline::{QuotaBucket, StatuslinePayload};

const WINDOW_5H_SECS: u64 = 18_000;
const WINDOW_WEEKLY_SECS: u64 = 604_800;
const PACE_BEHIND_DELTA_PCT: f64 = 15.0;

const THIRD_PARTY_NEEDLES: &[&str] = &[
    "claude",
    "anthropic",
    "sonnet",
    "opus",
    "haiku",
    "gpt",
    "openai",
    "o1",
    "o3",
    "o4",
    "codex",
];

/// Resolved rate-limit view for `quota_5h` or `quota_weekly` widgets.
#[derive(Debug, Clone, PartialEq)]
pub struct QuotaView {
    /// Short bucket label (`"5h"`, `"3p 5h"`, `"7d"`, `"3p 7d"`, `"argon"`, `"carbon"`).
    pub label: &'static str,
    /// Remaining quota percentage (`0.0..=100.0`).
    pub remaining_pct: f64,
    /// Compact countdown until reset (e.g. `"3h10m"`), if positive remaining time is known.
    pub reset_compact: Option<String>,
    /// True when remaining quota is lagging behind remaining time by `> 15%`.
    pub pace_behind: bool,
    /// Calibrated estimated remaining tokens for this bucket, when available.
    pub est_remaining_tokens: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelFamily {
    Carbon,
    Argon,
    ThirdParty,
    GeminiMain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuotaWindow {
    FiveHour,
    Weekly,
}

impl QuotaWindow {
    fn secs(self) -> u64 {
        match self {
            Self::FiveHour => WINDOW_5H_SECS,
            Self::Weekly => WINDOW_WEEKLY_SECS,
        }
    }

    fn default_gemini_label(self) -> &'static str {
        match self {
            Self::FiveHour => "5h",
            Self::Weekly => "7d",
        }
    }

    fn default_3p_label(self) -> &'static str {
        match self {
            Self::FiveHour => "3p 5h",
            Self::Weekly => "3p 7d",
        }
    }
}

/// Computes prompt cache hit percentage and cached token count from `current_usage`.
///
/// Includes `cache_creation_input_tokens` (when present) in the denominator so
/// incremental cache writes are accurately accounted for.
pub fn valid_cache_usage(payload: &StatuslinePayload) -> Option<(f64, u64)> {
    let u = payload.context_window.as_ref()?.current_usage?;
    let read = u.cache_read_input_tokens?;
    let input = u.input_tokens.unwrap_or(0);
    let create = u.cache_creation_input_tokens.unwrap_or(0);
    if input < 0 || read < 0 || create < 0 {
        return None;
    }
    let total = (input as f64) + (read as f64) + (create as f64);
    (total > 0.0).then(|| (100.0 * (read as f64) / total, read as u64))
}

/// Returns total active context tokens from `current_usage`.
pub fn current_context_tokens(payload: &StatuslinePayload) -> Option<u64> {
    let u = payload.context_window.as_ref()?.current_usage?;
    if u.input_tokens.is_none()
        && u.cache_read_input_tokens.is_none()
        && u.cache_creation_input_tokens.is_none()
    {
        return None;
    }
    let inp = u.input_tokens.unwrap_or(0).max(0) as u64;
    let read = u.cache_read_input_tokens.unwrap_or(0).max(0) as u64;
    let create = u.cache_creation_input_tokens.unwrap_or(0).max(0) as u64;
    Some(inp + read + create)
}

/// Formats a duration in seconds into a compact human-readable string (`45s`, `12m`, `3h10m`, `5d12h`).
pub fn format_duration_compact(secs: u64) -> String {
    if secs < 60 {
        return format!("{secs}s");
    }
    if secs < 3600 {
        return format!("{}m", secs / 60);
    }
    if secs < 86_400 {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        return if m == 0 {
            format!("{h}h")
        } else {
            format!("{h}h{m}m")
        };
    }
    let d = secs / 86_400;
    let h = (secs % 86_400) / 3600;
    if h == 0 {
        format!("{d}d")
    } else {
        format!("{d}d{h}h")
    }
}

/// Formats remaining quota percentage (`0.0..=100.0`) without rounding sub-1% values to `0%`.
///
/// Returns `"0%"` strictly when `remaining_pct <= 0.0` (or non-finite), `"<0.1%"`
/// when `0.0 < pct < 0.1`, `"{:.1}%"` when `0.1 <= pct < 1.0`, and rounded `"{:.0}%"`
/// when `pct >= 1.0`.
pub fn format_quota_pct(remaining_pct: f64) -> String {
    if !remaining_pct.is_finite() || remaining_pct <= 0.0 {
        return "0%".to_string();
    }
    let pct = remaining_pct.min(100.0);
    if pct < 0.1 {
        "<0.1%".to_string()
    } else if pct < 1.0 {
        format!("{pct:.1}%")
    } else {
        format!("{:.0}%", pct.round())
    }
}

fn parse_ascii_digits(bytes: &[u8]) -> Option<i64> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut val: i64 = 0;
    for &b in bytes {
        val = val * 10 + i64::from(b - b'0');
    }
    Some(val)
}

fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Converts a proleptic Gregorian calendar date to days since 1970-01-01 (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = year - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * m + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Parses an RFC3339 / ISO-8601 timestamp (`YYYY-MM-DDTHH:MM:SS[.fff](Z|+HH:MM|-HH:MM)`)
/// into Unix epoch seconds without external date crates.
pub fn parse_rfc3339_unix_secs(s: &str) -> Option<i64> {
    let b = s.trim().as_bytes();
    if b.len() < 20
        || b[4] != b'-'
        || b[7] != b'-'
        || !matches!(b[10], b'T' | b't' | b' ')
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let year = parse_ascii_digits(&b[0..4])?;
    let month = parse_ascii_digits(&b[5..7])?;
    let day = parse_ascii_digits(&b[8..10])?;
    let hour = parse_ascii_digits(&b[11..13])?;
    let minute = parse_ascii_digits(&b[14..16])?;
    let second = parse_ascii_digits(&b[17..19])?;

    if !(1..=12).contains(&month)
        || !(1..=days_in_month(year, month)).contains(&day)
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || !(0..=59).contains(&second)
    {
        return None;
    }

    let mut idx = 19;
    if idx < b.len() && b[idx] == b'.' {
        idx += 1;
        let frac_start = idx;
        while idx < b.len() && b[idx].is_ascii_digit() {
            idx += 1;
        }
        if idx == frac_start {
            return None;
        }
    }

    let tz = b.get(idx..)?;
    let offset_secs = match tz {
        [b'Z' | b'z'] => 0,
        [sign @ (b'+' | b'-'), h1, h2, b':', m1, m2] | [sign @ (b'+' | b'-'), h1, h2, m1, m2] => {
            let tz_h = parse_ascii_digits(&[*h1, *h2])?;
            let tz_m = parse_ascii_digits(&[*m1, *m2])?;
            if tz_h > 23 || tz_m > 59 {
                return None;
            }
            let mag = tz_h * 3600 + tz_m * 60;
            if *sign == b'-' {
                -mag
            } else {
                mag
            }
        }
        _ => return None,
    };

    let days = days_from_civil(year, month, day);
    let local_secs = days.checked_mul(86_400)? + hour * 3600 + minute * 60 + second;
    local_secs.checked_sub(offset_secs)
}

fn current_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn model_name_contains(payload: &StatuslinePayload, needles: &[&str]) -> bool {
    let Some(m) = payload.model.as_ref() else {
        return false;
    };
    let check = |s: &str| {
        let lower = s.to_ascii_lowercase();
        needles.iter().any(|n| lower.contains(n))
    };
    m.id.as_deref().is_some_and(check) || m.display_name.as_deref().is_some_and(check)
}

fn is_orchestrator_argon_text(raw: &str) -> bool {
    let lower = raw.to_ascii_lowercase();
    let trimmed = lower.trim();
    if trimmed.contains("argon")
        || trimmed.contains("m668")
        || trimmed == "orchestrator"
        || trimmed == "gemini pro orchestrator"
    {
        return true;
    }
    // Match bare `gemini-pro-orchestrator` or known Argon/G1 variants without swallowing unknown suffixes.
    let prefix = "gemini-pro-orchestrator";
    let mut search_from = 0;
    while let Some(rel) = trimmed[search_from..].find(prefix) {
        let start = search_from + rel;
        let before_ok = start == 0 || !trimmed.as_bytes()[start - 1].is_ascii_alphanumeric();
        let after = &trimmed[start + prefix.len()..];
        let after_ok = if after.is_empty() {
            true
        } else if let Some(rest) = after
            .strip_prefix("-argon")
            .or_else(|| after.strip_prefix("-g1-pro"))
            .or_else(|| after.strip_prefix("-g1"))
        {
            rest.is_empty()
                || (!rest.as_bytes()[0].is_ascii_alphanumeric() && rest.as_bytes()[0] != b'-')
        } else {
            !after.as_bytes()[0].is_ascii_alphanumeric() && after.as_bytes()[0] != b'-'
        };
        if before_ok && after_ok {
            return true;
        }
        search_from = start + prefix.len();
    }
    false
}

fn classify_model(payload: &StatuslinePayload) -> ModelFamily {
    if model_name_contains(payload, &["carbon", "m670"]) {
        return ModelFamily::Carbon;
    }
    let is_argon = payload.model.as_ref().is_some_and(|m| {
        m.id.as_deref().is_some_and(is_orchestrator_argon_text)
            || m.display_name
                .as_deref()
                .is_some_and(is_orchestrator_argon_text)
    });
    if is_argon {
        return ModelFamily::Argon;
    }
    if model_name_contains(payload, THIRD_PARTY_NEEDLES) {
        return ModelFamily::ThirdParty;
    }
    ModelFamily::GeminiMain
}

fn is_3p_bucket_key(lower_key: &str) -> bool {
    lower_key.starts_with("3p")
        || lower_key.contains("3p-")
        || lower_key.contains("3p_")
        || THIRD_PARTY_NEEDLES.iter().any(|n| lower_key.contains(n))
}

fn is_carbon_bucket_key(lower_key: &str) -> bool {
    lower_key.contains("carbon") || lower_key.contains("m670")
}

fn is_argon_bucket_key(lower_key: &str) -> bool {
    lower_key.contains("argon") || lower_key.contains("m668")
}

fn matches_window(lower_key: &str, window: QuotaWindow) -> bool {
    match window {
        QuotaWindow::FiveHour => lower_key.contains("5h") || lower_key.contains("hour"),
        QuotaWindow::Weekly => {
            lower_key.contains("weekly") || lower_key.contains("7d") || lower_key.contains("week")
        }
    }
}

fn infer_label_for_key(lower_key: &str, window: QuotaWindow) -> &'static str {
    if is_3p_bucket_key(lower_key) {
        window.default_3p_label()
    } else if is_carbon_bucket_key(lower_key) {
        "carbon"
    } else if is_argon_bucket_key(lower_key) {
        "argon"
    } else {
        window.default_gemini_label()
    }
}

fn resolve_reset_secs(bucket: &QuotaBucket, now_unix_secs: u64) -> Option<u64> {
    match bucket.reset_in_seconds {
        Some(0) => None,
        Some(secs) => Some(secs),
        None => {
            let reset_unix = bucket
                .reset_time
                .as_deref()
                .and_then(parse_rfc3339_unix_secs)?;
            let diff = i128::from(reset_unix) - i128::from(now_unix_secs);
            (diff > 0).then(|| diff.min(i128::from(u64::MAX)) as u64)
        }
    }
}

fn resolve_reset_at_secs(bucket: &QuotaBucket, now_unix_secs: u64) -> Option<u64> {
    if let Some(ts) = bucket
        .reset_time
        .as_deref()
        .and_then(parse_rfc3339_unix_secs)
    {
        return (ts >= 0).then_some(ts as u64);
    }
    bucket
        .reset_in_seconds
        .filter(|&s| s > 0)
        .map(|s| now_unix_secs.saturating_add(s))
}

fn build_quota_view(
    bucket: &QuotaBucket,
    label: &'static str,
    window_secs: u64,
    now_unix_secs: u64,
) -> Option<(QuotaView, Option<u64>)> {
    let frac = bucket.remaining_fraction.filter(|f| f.is_finite())?;
    let remaining_pct = (frac * 100.0).clamp(0.0, 100.0);
    let reset_secs = resolve_reset_secs(bucket, now_unix_secs);
    let reset_at_secs = resolve_reset_at_secs(bucket, now_unix_secs);
    let reset_compact = reset_secs.map(format_duration_compact);
    let pace_behind = reset_secs.filter(|&s| s <= window_secs).is_some_and(|s| {
        let time_left_pct = (s as f64) * 100.0 / (window_secs as f64);
        (time_left_pct - remaining_pct) > PACE_BEHIND_DELTA_PCT
    });
    Some((
        QuotaView {
            label,
            remaining_pct,
            reset_compact,
            pace_behind,
            est_remaining_tokens: None,
        },
        reset_at_secs,
    ))
}

fn try_priority_keys(
    quota: &BTreeMap<String, QuotaBucket>,
    keys: &[(&str, &'static str)],
    window: QuotaWindow,
    now_unix_secs: u64,
) -> Option<(QuotaView, Option<u64>)> {
    let window_secs = window.secs();
    for &(key, label) in keys {
        if let Some(bucket) = quota.get(key) {
            if let Some(found) = build_quota_view(bucket, label, window_secs, now_unix_secs) {
                return Some(found);
            }
        }
    }
    None
}

fn select_tightest_matching(
    quota: &BTreeMap<String, QuotaBucket>,
    window: QuotaWindow,
    now_unix_secs: u64,
    predicate: impl Fn(&str) -> bool,
    label_override: Option<&'static str>,
) -> Option<(QuotaView, Option<u64>)> {
    let window_secs = window.secs();
    let mut best: Option<(QuotaView, Option<u64>)> = None;
    for (key, bucket) in quota {
        let lower = key.to_ascii_lowercase();
        if !matches_window(&lower, window) || !predicate(&lower) {
            continue;
        }
        let label = label_override.unwrap_or_else(|| infer_label_for_key(&lower, window));
        let Some(found) = build_quota_view(bucket, label, window_secs, now_unix_secs) else {
            continue;
        };
        let replace = match &best {
            None => true,
            Some((cur, _)) => found.0.remaining_pct < cur.remaining_pct,
        };
        if replace {
            best = Some(found);
        }
    }
    best
}

struct FamilySpec {
    keys: &'static [(&'static str, &'static str)],
    pred: fn(&str) -> bool,
    label: Option<&'static str>,
}

fn is_gemini_main_bucket_key(lower_key: &str) -> bool {
    !is_3p_bucket_key(lower_key)
        && !is_argon_bucket_key(lower_key)
        && !is_carbon_bucket_key(lower_key)
}

fn family_spec(family: ModelFamily, window: QuotaWindow) -> FamilySpec {
    match (family, window) {
        (ModelFamily::Carbon, QuotaWindow::FiveHour) => FamilySpec {
            keys: &[
                ("gemini-carbon-5h", "carbon"),
                ("gemini-carbon-wtu-5h", "carbon"),
            ],
            pred: is_carbon_bucket_key,
            label: Some("carbon"),
        },
        (ModelFamily::Carbon, QuotaWindow::Weekly) => FamilySpec {
            keys: &[
                ("gemini-carbon-wtu-weekly", "carbon"),
                ("gemini-carbon-weekly", "carbon"),
            ],
            pred: is_carbon_bucket_key,
            label: Some("carbon"),
        },
        (ModelFamily::Argon, QuotaWindow::FiveHour) => FamilySpec {
            keys: &[
                ("gemini-argon-5h", "argon"),
                ("gemini-argon-wtu-5h", "argon"),
            ],
            pred: is_argon_bucket_key,
            label: Some("argon"),
        },
        (ModelFamily::Argon, QuotaWindow::Weekly) => FamilySpec {
            keys: &[
                ("gemini-argon-wtu-weekly", "argon"),
                ("gemini-argon-weekly", "argon"),
            ],
            pred: is_argon_bucket_key,
            label: Some("argon"),
        },
        (ModelFamily::ThirdParty, QuotaWindow::FiveHour) => FamilySpec {
            keys: &[("3p-5h", "3p 5h")],
            pred: is_3p_bucket_key,
            label: Some(window.default_3p_label()),
        },
        (ModelFamily::ThirdParty, QuotaWindow::Weekly) => FamilySpec {
            keys: &[("3p-weekly", "3p 7d")],
            pred: is_3p_bucket_key,
            label: Some(window.default_3p_label()),
        },
        (ModelFamily::GeminiMain, QuotaWindow::FiveHour) => FamilySpec {
            keys: &[("gemini-5h", "5h")],
            pred: is_gemini_main_bucket_key,
            label: Some(window.default_gemini_label()),
        },
        (ModelFamily::GeminiMain, QuotaWindow::Weekly) => FamilySpec {
            keys: &[("gemini-weekly", "7d")],
            pred: is_gemini_main_bucket_key,
            label: Some(window.default_gemini_label()),
        },
    }
}

fn resolve_quota_for_window(
    payload: &StatuslinePayload,
    window: QuotaWindow,
    now_unix_secs: u64,
) -> Option<(QuotaView, Option<u64>)> {
    let quota = payload.quota.as_ref()?;
    let family = classify_model(payload);
    let spec = family_spec(family, window);

    // 1. Exact priority keys for the active model family, then tightest family-compatible bucket.
    if let Some(found) = try_priority_keys(quota, spec.keys, window, now_unix_secs)
        .or_else(|| select_tightest_matching(quota, window, now_unix_secs, spec.pred, spec.label))
    {
        return Some(found);
    }

    // 2. For Carbon / Argon, fall back cleanly to Gemini main buckets before cross-family 3p.
    if matches!(family, ModelFamily::Carbon | ModelFamily::Argon) {
        let gemini_keys: &[(&str, &'static str)] = match window {
            QuotaWindow::FiveHour => &[("gemini-5h", "5h")],
            QuotaWindow::Weekly => &[("gemini-weekly", "7d")],
        };
        if let Some(found) =
            try_priority_keys(quota, gemini_keys, window, now_unix_secs).or_else(|| {
                select_tightest_matching(
                    quota,
                    window,
                    now_unix_secs,
                    is_gemini_main_bucket_key,
                    Some(window.default_gemini_label()),
                )
            })
        {
            return Some(found);
        }
    }

    // 3. Remaining non-3p Gemini buckets (e.g., GeminiMain falling back to argon/carbon when main is absent).
    if family != ModelFamily::ThirdParty {
        if let Some(found) =
            select_tightest_matching(quota, window, now_unix_secs, |k| !is_3p_bucket_key(k), None)
        {
            return Some(found);
        }
    }

    // 4. Last-resort cross-family fallback only when no family-matching bucket exists.
    let cross_keys: &[(&str, &'static str)] = match (family, window) {
        (ModelFamily::ThirdParty, QuotaWindow::FiveHour) => &[("gemini-5h", "5h")],
        (ModelFamily::ThirdParty, QuotaWindow::Weekly) => &[
            ("gemini-weekly", "7d"),
            ("gemini-argon-wtu-weekly", "argon"),
            ("gemini-carbon-wtu-weekly", "carbon"),
        ],
        (_, QuotaWindow::FiveHour) => &[("3p-5h", "3p 5h")],
        (_, QuotaWindow::Weekly) => &[("3p-weekly", "3p 7d")],
    };
    try_priority_keys(quota, cross_keys, window, now_unix_secs)
        .or_else(|| select_tightest_matching(quota, window, now_unix_secs, |_| true, None))
}

/// Resolves the active 5-hour quota bucket and its reset epoch timestamp at `now_unix_secs`.
pub fn resolve_quota_5h_with_reset_at(
    payload: &StatuslinePayload,
    now_unix_secs: u64,
) -> Option<(QuotaView, Option<u64>)> {
    resolve_quota_for_window(payload, QuotaWindow::FiveHour, now_unix_secs)
}

/// Resolves the active weekly quota bucket and its reset epoch timestamp at `now_unix_secs`.
pub fn resolve_quota_weekly_with_reset_at(
    payload: &StatuslinePayload,
    now_unix_secs: u64,
) -> Option<(QuotaView, Option<u64>)> {
    resolve_quota_for_window(payload, QuotaWindow::Weekly, now_unix_secs)
}

/// Resolves the active 5-hour quota bucket at an explicit Unix epoch timestamp.
pub fn resolve_quota_5h_at(payload: &StatuslinePayload, now_unix_secs: u64) -> Option<QuotaView> {
    resolve_quota_5h_with_reset_at(payload, now_unix_secs).map(|(v, _)| v)
}

/// Resolves the active weekly quota bucket at an explicit Unix epoch timestamp.
pub fn resolve_quota_weekly_at(
    payload: &StatuslinePayload,
    now_unix_secs: u64,
) -> Option<QuotaView> {
    resolve_quota_weekly_with_reset_at(payload, now_unix_secs).map(|(v, _)| v)
}

/// Resolves the active 5-hour quota bucket (`gemini-5h`, `3p-5h`, `carbon`, or tightest match).
pub fn resolve_quota_5h(payload: &StatuslinePayload) -> Option<QuotaView> {
    resolve_quota_5h_at(payload, current_unix_secs())
}

/// Resolves the active weekly quota bucket (`gemini-weekly`, `3p-weekly`, `argon`, `carbon`, or tightest match).
pub fn resolve_quota_weekly(payload: &StatuslinePayload) -> Option<QuotaView> {
    resolve_quota_weekly_at(payload, current_unix_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::statusline::{ContextWindow, JsonUsage, ModelInfo};

    #[test]
    fn cache_usage_accounts_for_cache_creation_and_missing_field() {
        let with_create = StatuslinePayload {
            context_window: Some(ContextWindow {
                current_usage: Some(JsonUsage {
                    input_tokens: Some(200),
                    output_tokens: Some(50),
                    cache_read_input_tokens: Some(800),
                    cache_creation_input_tokens: Some(1000),
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(valid_cache_usage(&with_create), Some((40.0, 800)));

        let without_create = StatuslinePayload {
            context_window: Some(ContextWindow {
                current_usage: Some(JsonUsage {
                    input_tokens: Some(200),
                    output_tokens: None,
                    cache_read_input_tokens: Some(800),
                    cache_creation_input_tokens: None,
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(valid_cache_usage(&without_create), Some((80.0, 800)));
    }

    #[test]
    fn quota_resolution_selects_model_bucket_and_pace_warning() {
        let mut quota = BTreeMap::new();
        quota.insert(
            "gemini-5h".to_string(),
            QuotaBucket {
                remaining_fraction: Some(0.40),
                reset_in_seconds: Some(14_400), // 80% time left vs 40% quota -> behind pace
                reset_time: None,
            },
        );
        quota.insert(
            "3p-5h".to_string(),
            QuotaBucket {
                remaining_fraction: Some(0.90),
                reset_in_seconds: Some(11_400),
                reset_time: None,
            },
        );
        quota.insert(
            "gemini-argon-wtu-weekly".to_string(),
            QuotaBucket {
                remaining_fraction: Some(0.9378),
                reset_in_seconds: Some(560_580),
                reset_time: None,
            },
        );

        let gemini_payload = StatuslinePayload {
            model: Some(ModelInfo {
                id: Some("Gemini 4 Argon (Medium)".to_string()),
                display_name: Some("Gemini 4 Argon (Medium)".to_string()),
                effort: None,
            }),
            quota: Some(quota.clone()),
            ..Default::default()
        };
        let q5 = resolve_quota_5h(&gemini_payload).unwrap();
        assert_eq!(q5.label, "5h");
        assert!((q5.remaining_pct - 40.0).abs() < 1e-6);
        assert_eq!(q5.reset_compact.as_deref(), Some("4h"));
        assert!(q5.pace_behind);

        let qw = resolve_quota_weekly(&gemini_payload).unwrap();
        assert_eq!(qw.label, "argon");
        assert_eq!(qw.reset_compact.as_deref(), Some("6d11h"));
        assert!(!qw.pace_behind);

        let claude_payload = StatuslinePayload {
            model: Some(ModelInfo {
                id: Some("claude-sonnet-4".to_string()),
                display_name: Some("Claude Sonnet 4".to_string()),
                effort: None,
            }),
            quota: Some(quota),
            ..Default::default()
        };
        let q3p = resolve_quota_5h(&claude_payload).unwrap();
        assert_eq!(q3p.label, "3p 5h");
        assert_eq!(q3p.reset_compact.as_deref(), Some("3h10m"));
        assert!(!q3p.pace_behind);
    }

    #[test]
    fn formats_compact_durations() {
        assert_eq!(format_duration_compact(45), "45s");
        assert_eq!(format_duration_compact(720), "12m");
        assert_eq!(format_duration_compact(11_400), "3h10m");
        assert_eq!(format_duration_compact(14_400), "4h");
        assert_eq!(format_duration_compact(172_800), "2d");
        assert_eq!(format_duration_compact(560_580), "6d11h");
    }

    #[test]
    fn formats_sub_one_percent_quota_without_false_zero() {
        assert_eq!(format_quota_pct(0.0), "0%");
        assert_eq!(format_quota_pct(-2.0), "0%");
        assert_eq!(format_quota_pct(f64::NAN), "0%");
        assert_eq!(format_quota_pct(0.0001), "<0.1%");
        assert_eq!(format_quota_pct(0.05), "<0.1%");
        assert_eq!(format_quota_pct(0.099), "<0.1%");
        assert_eq!(format_quota_pct(0.1), "0.1%");
        assert_eq!(format_quota_pct(0.4), "0.4%");
        assert_eq!(format_quota_pct(0.44), "0.4%");
        assert_eq!(format_quota_pct(0.94), "0.9%");
        assert_eq!(format_quota_pct(1.0), "1%");
        assert_eq!(format_quota_pct(84.4), "84%");
        assert_eq!(format_quota_pct(93.78), "94%");
        assert_eq!(format_quota_pct(100.0), "100%");
    }

    #[test]
    fn parses_rfc3339_timestamps_across_offsets_and_rejects_invalid() {
        assert_eq!(parse_rfc3339_unix_secs("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339_unix_secs("2026-11-18T12:00:00Z"),
            Some(1_795_003_200)
        );
        assert_eq!(
            parse_rfc3339_unix_secs("2026-11-18T12:00:00.456Z"),
            Some(1_795_003_200)
        );
        assert_eq!(
            parse_rfc3339_unix_secs("2026-11-18T20:00:00+08:00"),
            Some(1_795_003_200)
        );
        assert_eq!(
            parse_rfc3339_unix_secs("2026-11-18T07:00:00-05:00"),
            Some(1_795_003_200)
        );
        assert_eq!(
            parse_rfc3339_unix_secs("2024-02-29T23:59:59Z"),
            Some(1_709_251_199)
        );
        assert_eq!(parse_rfc3339_unix_secs("2026-02-29T00:00:00Z"), None);
        assert_eq!(parse_rfc3339_unix_secs("2026-13-01T00:00:00Z"), None);
        assert_eq!(parse_rfc3339_unix_secs("2026-11-18T12:00:00"), None);
        assert_eq!(parse_rfc3339_unix_secs("2026-11-18T12:00:00.Z"), None);
        assert_eq!(parse_rfc3339_unix_secs("2026-11-18T12:00:00Zextra"), None);
    }

    #[test]
    fn reset_time_fallback_handles_z_and_offset_and_pace_behind() {
        // Target reset is 2026-11-18T12:00:00Z (1_795_003_200); now is 4h earlier (14_400s).
        let now = 1_795_003_200 - 14_400;
        let mut quota = BTreeMap::new();
        quota.insert(
            "gemini-5h".to_string(),
            QuotaBucket {
                remaining_fraction: Some(0.40),
                reset_in_seconds: None,
                reset_time: Some("2026-11-18T12:00:00Z".to_string()),
            },
        );
        quota.insert(
            "gemini-weekly".to_string(),
            QuotaBucket {
                remaining_fraction: Some(0.80),
                reset_in_seconds: None,
                reset_time: Some("2026-11-18T20:00:00+08:00".to_string()),
            },
        );

        let payload = StatuslinePayload {
            model: Some(ModelInfo {
                id: Some("gemini-4-hydrogen".to_string()),
                ..Default::default()
            }),
            quota: Some(quota),
            ..Default::default()
        };

        let q5 = resolve_quota_5h_at(&payload, now).unwrap();
        assert_eq!(q5.reset_compact.as_deref(), Some("4h"));
        assert!(q5.pace_behind);

        let qw = resolve_quota_weekly_at(&payload, now).unwrap();
        assert_eq!(qw.reset_compact.as_deref(), Some("4h"));
        assert!(!qw.pace_behind);
    }

    #[test]
    fn expired_reset_time_and_zero_reset_in_seconds_emit_no_timer() {
        let now = 1_795_003_200;
        let mut quota = BTreeMap::new();
        quota.insert(
            "gemini-5h".to_string(),
            QuotaBucket {
                remaining_fraction: Some(0.10),
                reset_in_seconds: None,
                reset_time: Some("2026-11-18T12:00:00Z".to_string()), // == now (expired)
            },
        );
        quota.insert(
            "gemini-weekly".to_string(),
            QuotaBucket {
                remaining_fraction: Some(0.10),
                reset_in_seconds: Some(0),
                reset_time: Some("2099-01-01T00:00:00Z".to_string()),
            },
        );

        let payload = StatuslinePayload {
            quota: Some(quota),
            ..Default::default()
        };

        let q5 = resolve_quota_5h_at(&payload, now).unwrap();
        assert_eq!(q5.reset_compact, None);
        assert!(!q5.pace_behind);

        let qw = resolve_quota_weekly_at(&payload, now).unwrap();
        assert_eq!(qw.reset_compact, None);
        assert!(!qw.pace_behind);
    }

    #[test]
    fn routes_orchestrator_carbon_and_gpt_oss_models() {
        let mut base_quota = BTreeMap::new();
        base_quota.insert(
            "gemini-5h".to_string(),
            QuotaBucket {
                remaining_fraction: Some(0.80),
                reset_in_seconds: Some(3_600),
                reset_time: None,
            },
        );
        base_quota.insert(
            "gemini-weekly".to_string(),
            QuotaBucket {
                remaining_fraction: Some(0.70),
                reset_in_seconds: Some(86_400),
                reset_time: None,
            },
        );
        base_quota.insert(
            "gemini-argon-wtu-weekly".to_string(),
            QuotaBucket {
                remaining_fraction: Some(0.60),
                reset_in_seconds: Some(86_400),
                reset_time: None,
            },
        );
        base_quota.insert(
            "3p-5h".to_string(),
            QuotaBucket {
                remaining_fraction: Some(0.004),
                reset_in_seconds: Some(1_800),
                reset_time: None,
            },
        );
        base_quota.insert(
            "3p-weekly".to_string(),
            QuotaBucket {
                remaining_fraction: Some(0.50),
                reset_in_seconds: Some(172_800),
                reset_time: None,
            },
        );

        // Bare gemini-pro-orchestrator routes to argon when carbon is absent.
        let orch = StatuslinePayload {
            model: Some(ModelInfo {
                id: Some("gemini-pro-orchestrator".to_string()),
                display_name: Some("Orchestrator".to_string()),
                effort: None,
            }),
            quota: Some(base_quota.clone()),
            ..Default::default()
        };
        assert_eq!(resolve_quota_weekly_at(&orch, 0).unwrap().label, "argon");
        assert_eq!(resolve_quota_5h_at(&orch, 0).unwrap().label, "5h");

        // Carbon without carbon-specific buckets falls back cleanly to 7d / 5h.
        let carbon_fallback = StatuslinePayload {
            model: Some(ModelInfo {
                id: Some("m670".to_string()),
                display_name: Some("Gemini 4 Carbon".to_string()),
                effort: None,
            }),
            quota: Some(base_quota.clone()),
            ..Default::default()
        };
        assert_eq!(
            resolve_quota_weekly_at(&carbon_fallback, 0).unwrap().label,
            "7d"
        );
        assert_eq!(
            resolve_quota_5h_at(&carbon_fallback, 0).unwrap().label,
            "5h"
        );

        // Carbon with carbon-specific bucket uses label "carbon".
        let mut carbon_quota = base_quota.clone();
        carbon_quota.insert(
            "gemini-carbon-wtu-weekly".to_string(),
            QuotaBucket {
                remaining_fraction: Some(0.45),
                reset_in_seconds: Some(86_400),
                reset_time: None,
            },
        );
        let carbon_specific = StatuslinePayload {
            model: Some(ModelInfo {
                id: Some("gemini-pro-orchestrator-carbon".to_string()),
                display_name: None,
                effort: None,
            }),
            quota: Some(carbon_quota),
            ..Default::default()
        };
        let cw = resolve_quota_weekly_at(&carbon_specific, 0).unwrap();
        assert_eq!(cw.label, "carbon");
        assert!((cw.remaining_pct - 45.0).abs() < 1e-6);

        // GPT-OSS and expanded 3P needles route to 3p buckets and preserve 0.4%.
        let gpt_oss = StatuslinePayload {
            model: Some(ModelInfo {
                id: Some("gpt-oss-120b".to_string()),
                display_name: Some("GPT-OSS 120B".to_string()),
                effort: None,
            }),
            quota: Some(base_quota.clone()),
            ..Default::default()
        };
        let g5 = resolve_quota_5h_at(&gpt_oss, 0).unwrap();
        assert_eq!(g5.label, "3p 5h");
        assert_eq!(format_quota_pct(g5.remaining_pct), "0.4%");
        assert_eq!(resolve_quota_weekly_at(&gpt_oss, 0).unwrap().label, "3p 7d");

        let codex = StatuslinePayload {
            model: Some(ModelInfo {
                id: Some("openai/o3-codex".to_string()),
                display_name: None,
                effort: None,
            }),
            quota: Some(base_quota),
            ..Default::default()
        };
        assert_eq!(resolve_quota_5h_at(&codex, 0).unwrap().label, "3p 5h");
    }

    #[test]
    fn dynamic_scan_picks_tightest_family_bucket_without_cross_binding() {
        let mut quota = BTreeMap::new();
        quota.insert(
            "gemini-flash-5h".to_string(),
            QuotaBucket {
                remaining_fraction: Some(0.65),
                reset_in_seconds: Some(7_200),
                reset_time: None,
            },
        );
        quota.insert(
            "gemini-pro-5h".to_string(),
            QuotaBucket {
                remaining_fraction: Some(0.12),
                reset_in_seconds: Some(3_600),
                reset_time: None,
            },
        );
        quota.insert(
            "3p-claude-5h".to_string(),
            QuotaBucket {
                remaining_fraction: Some(0.01),
                reset_in_seconds: Some(900),
                reset_time: None,
            },
        );
        quota.insert(
            "3p-openai-weekly".to_string(),
            QuotaBucket {
                remaining_fraction: Some(0.22),
                reset_in_seconds: Some(86_400),
                reset_time: None,
            },
        );
        quota.insert(
            "3p-anthropic-weekly".to_string(),
            QuotaBucket {
                remaining_fraction: Some(0.55),
                reset_in_seconds: Some(86_400),
                reset_time: None,
            },
        );
        quota.insert(
            "gemini-hydrogen-weekly".to_string(),
            QuotaBucket {
                remaining_fraction: Some(0.02),
                reset_in_seconds: Some(86_400),
                reset_time: None,
            },
        );

        let gemini = StatuslinePayload {
            model: Some(ModelInfo {
                id: Some("gemini-4-hydrogen".to_string()),
                ..Default::default()
            }),
            quota: Some(quota.clone()),
            ..Default::default()
        };
        let g5 = resolve_quota_5h_at(&gemini, 0).unwrap();
        assert_eq!(g5.label, "5h");
        assert!((g5.remaining_pct - 12.0).abs() < 1e-6);

        let third_party = StatuslinePayload {
            model: Some(ModelInfo {
                id: Some("anthropic/claude-opus-5-5".to_string()),
                ..Default::default()
            }),
            quota: Some(quota),
            ..Default::default()
        };
        let tw = resolve_quota_weekly_at(&third_party, 0).unwrap();
        assert_eq!(tw.label, "3p 7d");
        assert!((tw.remaining_pct - 22.0).abs() < 1e-6);
    }
}
