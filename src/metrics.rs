//! Pure metric derivation for cache hit rate, context tokens, and dynamic quota pacing.

use std::collections::BTreeSet;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::statusline::{QuotaBucket, StatuslinePayload};

const SECS_PER_HOUR: u64 = 3_600;
const SECS_PER_DAY: u64 = 86_400;
const SECS_PER_WEEK: u64 = 604_800;
const PACE_BEHIND_DELTA_PCT: f64 = 15.0;
const STRUCTURAL_NOISE_TOKENS: &[&str] = &["wtu", "quota", "pool", "limit"];

/// Resolved rate-limit view for `quota_5h` or `quota_weekly` widgets.
#[derive(Debug, Clone, PartialEq)]
pub struct QuotaView {
    /// Dynamic bucket label (e.g. `"5h"`, `"3p 5h"`, `"7d"`, `"3p 7d"`, `"24h"`).
    pub label: String,
    /// Raw bucket identifier key from `payload.quota`.
    pub bucket_key: String,
    /// Parsed rolling window duration in seconds.
    pub window_secs: u64,
    /// Remaining quota percentage (`0.0..=100.0`).
    pub remaining_pct: f64,
    /// Remaining credit units when `remaining_fraction` is absent and `remaining_amount >= 0`.
    pub remaining_amount: Option<i64>,
    /// Compact countdown until reset (e.g. `"3h10m"`), if positive remaining time is known.
    pub reset_compact: Option<String>,
    /// True when remaining quota is lagging behind remaining time by `> 15%`.
    pub pace_behind: bool,
    /// Calibrated estimated remaining tokens for this bucket, when available.
    pub est_remaining_tokens: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowTarget {
    Short,
    MultiDay,
}

impl WindowTarget {
    fn matches(self, window_secs: u64) -> bool {
        match self {
            Self::Short => window_secs < SECS_PER_DAY,
            Self::MultiDay => window_secs >= SECS_PER_DAY,
        }
    }
}

#[derive(Debug, Clone)]
struct BucketDescriptor {
    window_secs: u64,
    win_short: String,
    is_3p: bool,
    provider: Option<String>,
    qualifiers: Vec<String>,
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

fn tokenize_alnum_lower(raw: &str) -> Vec<String> {
    raw.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|tok| !tok.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

fn parse_window_token(token: &str) -> Option<(u64, String)> {
    match token {
        "hourly" | "hour" => return Some((SECS_PER_HOUR, "1h".to_string())),
        "daily" | "day" => return Some((SECS_PER_DAY, "1d".to_string())),
        "weekly" | "week" => return Some((SECS_PER_WEEK, "7d".to_string())),
        _ => {}
    }
    if let Some(num) = token.strip_suffix('h') {
        if let Some(n) = num.parse::<u64>().ok().filter(|&n| n > 0) {
            return n.checked_mul(SECS_PER_HOUR).map(|s| (s, format!("{n}h")));
        }
    }
    if let Some(num) = token.strip_suffix('d') {
        if let Some(n) = num.parse::<u64>().ok().filter(|&n| n > 0) {
            return n.checked_mul(SECS_PER_DAY).map(|s| (s, format!("{n}d")));
        }
    }
    None
}

fn parse_bucket_descriptor(key: &str) -> Option<BucketDescriptor> {
    let mut window_info: Option<(u64, String)> = None;
    let mut is_3p = false;
    let mut non_3p_tokens = Vec::new();

    for tok in tokenize_alnum_lower(key) {
        if let Some(parsed) = parse_window_token(&tok) {
            if window_info.is_none() {
                window_info = Some(parsed);
            }
            continue;
        }
        if STRUCTURAL_NOISE_TOKENS.contains(&tok.as_str()) {
            continue;
        }
        if tok == "3p" {
            is_3p = true;
            continue;
        }
        non_3p_tokens.push(tok);
    }

    let (window_secs, win_short) = window_info?;
    let (provider, qualifiers) = if is_3p {
        (None, non_3p_tokens)
    } else {
        let mut iter = non_3p_tokens.into_iter();
        (iter.next(), iter.collect())
    };
    Some(BucketDescriptor {
        window_secs,
        win_short,
        is_3p,
        provider,
        qualifiers,
    })
}

fn synthesize_label(desc: &BucketDescriptor) -> String {
    if desc.is_3p {
        return format!("3p {}", desc.win_short);
    }
    if !desc.qualifiers.is_empty() {
        let joined = desc.qualifiers.join(" ");
        if desc.window_secs >= SECS_PER_DAY {
            return joined;
        }
        return format!("{joined} {}", desc.win_short);
    }
    desc.win_short.clone()
}

fn collect_model_tokens(payload: &StatuslinePayload) -> BTreeSet<String> {
    let mut tokens = BTreeSet::new();
    let Some(model) = payload.model.as_ref() else {
        return tokens;
    };
    if let Some(id) = model.id.as_deref() {
        tokens.extend(tokenize_alnum_lower(id));
    }
    if let Some(name) = model.display_name.as_deref() {
        tokens.extend(tokenize_alnum_lower(name));
    }
    tokens
}

fn resolve_reset_secs(bucket: &QuotaBucket, now_unix_secs: u64) -> Option<u64> {
    match bucket.reset_in_seconds {
        Some(secs) if secs <= 0 => None,
        Some(secs) => Some(secs as u64),
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
        .map(|s| now_unix_secs.saturating_add(s as u64))
}

fn build_quota_view(
    key: &str,
    desc: &BucketDescriptor,
    bucket: &QuotaBucket,
    now_unix_secs: u64,
) -> Option<(QuotaView, Option<u64>, bool)> {
    let has_frac = bucket.remaining_fraction.is_some_and(f64::is_finite);
    let (remaining_pct, remaining_amount) =
        if let Some(frac) = bucket.remaining_fraction.filter(|f| f.is_finite()) {
            ((frac * 100.0).clamp(0.0, 100.0), None)
        } else {
            let amt = bucket.remaining_amount.filter(|&a| a >= 0)?;
            (0.0, Some(amt))
        };
    let reset_secs = resolve_reset_secs(bucket, now_unix_secs);
    let reset_at_secs = resolve_reset_at_secs(bucket, now_unix_secs);
    let reset_compact = reset_secs.map(format_duration_compact);
    let pace_behind = has_frac
        && reset_secs
            .filter(|&s| s <= desc.window_secs)
            .is_some_and(|s| {
                let time_left_pct = (s as f64) * 100.0 / (desc.window_secs as f64);
                (time_left_pct - remaining_pct) > PACE_BEHIND_DELTA_PCT
            });
    Some((
        QuotaView {
            label: synthesize_label(desc),
            bucket_key: key.to_string(),
            window_secs: desc.window_secs,
            remaining_pct,
            remaining_amount,
            reset_compact,
            pace_behind,
            est_remaining_tokens: None,
        },
        reset_at_secs,
        has_frac,
    ))
}

type AffinityScore = (u8, usize, isize);

fn score_bucket(
    desc: &BucketDescriptor,
    model_tokens: &BTreeSet<String>,
    effective_providers: &BTreeSet<String>,
    has_model_identity: bool,
) -> AffinityScore {
    let provider_matched = desc
        .provider
        .as_ref()
        .is_some_and(|p| effective_providers.contains(p));
    let matched_q = desc
        .qualifiers
        .iter()
        .filter(|q| model_tokens.contains(*q))
        .count();
    let unmatched_q = desc.qualifiers.len() - matched_q;
    let is_3p_model = has_model_identity && effective_providers.is_empty();

    let tier = if desc.is_3p {
        if is_3p_model && unmatched_q == 0 && matched_q > 0 {
            7
        } else if is_3p_model && unmatched_q == 0 {
            6
        } else if is_3p_model {
            5
        } else if unmatched_q == 0 {
            1
        } else {
            0
        }
    } else if provider_matched && unmatched_q == 0 && matched_q > 0 {
        7
    } else if (provider_matched && unmatched_q == 0) || (matched_q > 0 && unmatched_q == 0) {
        6
    } else if provider_matched {
        4
    } else if !has_model_identity && unmatched_q == 0 {
        5
    } else if !has_model_identity {
        3
    } else {
        2
    };

    (tier, matched_q, -(unmatched_q as isize))
}

struct Candidate {
    score: AffinityScore,
    has_frac: bool,
    view: QuotaView,
    reset_at: Option<u64>,
}

fn is_tighter_equal_score(cand: &Candidate, cur: &Candidate) -> bool {
    if cand.has_frac != cur.has_frac {
        return cand.has_frac;
    }
    if cand.has_frac {
        cand.view.remaining_pct < cur.view.remaining_pct
    } else {
        cand.view.remaining_amount < cur.view.remaining_amount
    }
}

fn resolve_quota_for_window(
    payload: &StatuslinePayload,
    target: WindowTarget,
    now_unix_secs: u64,
) -> Option<(QuotaView, Option<u64>)> {
    let quota = payload.quota.as_ref()?;
    let model_tokens = collect_model_tokens(payload);
    let has_model_identity = !model_tokens.is_empty();

    let mut active_descriptors: Vec<(&str, BucketDescriptor, &QuotaBucket)> = Vec::new();
    let mut provider_tokens_in_map = BTreeSet::new();
    for (key, bucket) in quota {
        if bucket.disabled == Some(true) {
            continue;
        }
        let Some(desc) = parse_bucket_descriptor(key) else {
            continue;
        };
        if let Some(ref p) = desc.provider {
            provider_tokens_in_map.insert(p.clone());
        }
        active_descriptors.push((key.as_str(), desc, bucket));
    }

    // Seed effective providers from model tokens directly matching any provider in the quota map,
    // plus providers of buckets whose non-empty qualifiers all appear in the model tokens.
    let mut effective_providers: BTreeSet<String> = provider_tokens_in_map
        .into_iter()
        .filter(|p| model_tokens.contains(p))
        .collect();
    for (_, desc, _) in &active_descriptors {
        if let Some(ref p) = desc.provider {
            if !desc.qualifiers.is_empty()
                && desc.qualifiers.iter().all(|q| model_tokens.contains(q))
            {
                effective_providers.insert(p.clone());
            }
        }
    }

    let mut best: Option<Candidate> = None;
    for (key, desc, bucket) in active_descriptors {
        if !target.matches(desc.window_secs) {
            continue;
        }
        let Some((view, reset_at, has_frac)) = build_quota_view(key, &desc, bucket, now_unix_secs)
        else {
            continue;
        };
        let score = score_bucket(&desc, &model_tokens, &effective_providers, has_model_identity);
        let cand = Candidate {
            score,
            has_frac,
            view,
            reset_at,
        };
        let replace = match &best {
            None => true,
            Some(cur) => cand.score > cur.score || (cand.score == cur.score && is_tighter_equal_score(&cand, cur)),
        };
        if replace {
            best = Some(cand);
        }
    }

    best.map(|c| (c.view, c.reset_at))
}

/// Resolves the active short-window (`< 24h`) quota bucket and its reset epoch timestamp at `now_unix_secs`.
pub fn resolve_quota_5h_with_reset_at(
    payload: &StatuslinePayload,
    now_unix_secs: u64,
) -> Option<(QuotaView, Option<u64>)> {
    resolve_quota_for_window(payload, WindowTarget::Short, now_unix_secs)
}

/// Resolves the active multi-day (`>= 24h`) quota bucket and its reset epoch timestamp at `now_unix_secs`.
pub fn resolve_quota_weekly_with_reset_at(
    payload: &StatuslinePayload,
    now_unix_secs: u64,
) -> Option<(QuotaView, Option<u64>)> {
    resolve_quota_for_window(payload, WindowTarget::MultiDay, now_unix_secs)
}

/// Resolves the active short-window quota bucket at an explicit Unix epoch timestamp.
pub fn resolve_quota_5h_at(payload: &StatuslinePayload, now_unix_secs: u64) -> Option<QuotaView> {
    resolve_quota_5h_with_reset_at(payload, now_unix_secs).map(|(v, _)| v)
}

/// Resolves the active multi-day quota bucket at an explicit Unix epoch timestamp.
pub fn resolve_quota_weekly_at(
    payload: &StatuslinePayload,
    now_unix_secs: u64,
) -> Option<QuotaView> {
    resolve_quota_weekly_with_reset_at(payload, now_unix_secs).map(|(v, _)| v)
}

/// Resolves the active short-window quota bucket dynamically from `payload.quota`.
pub fn resolve_quota_5h(payload: &StatuslinePayload) -> Option<QuotaView> {
    resolve_quota_5h_at(payload, current_unix_secs())
}

/// Resolves the active multi-day quota bucket dynamically from `payload.quota`.
pub fn resolve_quota_weekly(payload: &StatuslinePayload) -> Option<QuotaView> {
    resolve_quota_weekly_at(payload, current_unix_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
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
            "acme-5h".to_string(),
            QuotaBucket {
                remaining_fraction: Some(0.40),
                reset_in_seconds: Some(14_400), // 80% time left vs 40% quota -> behind pace
                ..Default::default()
            },
        );
        quota.insert(
            "3p-5h".to_string(),
            QuotaBucket {
                remaining_fraction: Some(0.90),
                reset_in_seconds: Some(11_400),
                ..Default::default()
            },
        );
        quota.insert(
            "acme-tier1-wtu-weekly".to_string(),
            QuotaBucket {
                remaining_fraction: Some(0.9378),
                reset_in_seconds: Some(560_580),
                ..Default::default()
            },
        );

        let acme_payload = StatuslinePayload {
            model: Some(ModelInfo {
                id: Some("Acme 4 Tier1 (Medium)".to_string()),
                display_name: Some("Acme 4 Tier1 (Medium)".to_string()),
                effort: None,
            }),
            quota: Some(quota.clone()),
            ..Default::default()
        };
        let q5 = resolve_quota_5h(&acme_payload).unwrap();
        assert_eq!(q5.label, "5h");
        assert_eq!(q5.bucket_key, "acme-5h");
        assert_eq!(q5.window_secs, 18_000);
        assert!((q5.remaining_pct - 40.0).abs() < 1e-6);
        assert_eq!(q5.reset_compact.as_deref(), Some("4h"));
        assert!(q5.pace_behind);

        let qw = resolve_quota_weekly(&acme_payload).unwrap();
        assert_eq!(qw.label, "tier1");
        assert_eq!(qw.bucket_key, "acme-tier1-wtu-weekly");
        assert_eq!(qw.window_secs, 604_800);
        assert_eq!(qw.reset_compact.as_deref(), Some("6d11h"));
        assert!(!qw.pace_behind);

        let third_party_payload = StatuslinePayload {
            model: Some(ModelInfo {
                id: Some("vendorx-large-4".to_string()),
                display_name: Some("VendorX Large 4".to_string()),
                effort: None,
            }),
            quota: Some(quota),
            ..Default::default()
        };
        let q3p = resolve_quota_5h(&third_party_payload).unwrap();
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
            "acme-5h".to_string(),
            QuotaBucket {
                remaining_fraction: Some(0.40),
                reset_time: Some("2026-11-18T12:00:00Z".to_string()),
                ..Default::default()
            },
        );
        quota.insert(
            "acme-weekly".to_string(),
            QuotaBucket {
                remaining_fraction: Some(0.80),
                reset_time: Some("2026-11-18T20:00:00+08:00".to_string()),
                ..Default::default()
            },
        );

        let payload = StatuslinePayload {
            model: Some(ModelInfo {
                id: Some("acme-4-base".to_string()),
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
    fn expired_negative_or_zero_reset_emits_no_timer_and_disabled_is_ignored() {
        let now = 1_795_003_200;
        let mut quota = BTreeMap::new();
        quota.insert(
            "acme-5h".to_string(),
            QuotaBucket {
                remaining_fraction: Some(0.10),
                reset_in_seconds: Some(-12),
                reset_time: Some("2026-11-18T12:00:00Z".to_string()),
                ..Default::default()
            },
        );
        quota.insert(
            "acme-weekly".to_string(),
            QuotaBucket {
                remaining_fraction: Some(0.10),
                reset_in_seconds: Some(0),
                reset_time: Some("2099-01-01T00:00:00Z".to_string()),
                ..Default::default()
            },
        );
        quota.insert(
            "acme-disabled-5h".to_string(),
            QuotaBucket {
                remaining_fraction: Some(0.01),
                reset_in_seconds: Some(600),
                disabled: Some(true),
                ..Default::default()
            },
        );

        let payload = StatuslinePayload {
            quota: Some(quota),
            ..Default::default()
        };

        let q5 = resolve_quota_5h_at(&payload, now).unwrap();
        assert_eq!(q5.bucket_key, "acme-5h");
        assert_eq!(q5.reset_compact, None);
        assert!(!q5.pace_behind);

        let qw = resolve_quota_weekly_at(&payload, now).unwrap();
        assert_eq!(qw.reset_compact, None);
        assert!(!qw.pace_behind);
    }

    #[test]
    fn token_affinity_routes_qualifiers_3p_dynamic_windows_and_credits() {
        let mut base_quota = BTreeMap::new();
        base_quota.insert(
            "acme-5h".to_string(),
            QuotaBucket {
                remaining_fraction: Some(0.80),
                reset_in_seconds: Some(3_600),
                ..Default::default()
            },
        );
        base_quota.insert(
            "acme-weekly".to_string(),
            QuotaBucket {
                remaining_fraction: Some(0.70),
                reset_in_seconds: Some(86_400),
                ..Default::default()
            },
        );
        base_quota.insert(
            "acme-tier1-wtu-weekly".to_string(),
            QuotaBucket {
                remaining_fraction: Some(0.60),
                reset_in_seconds: Some(86_400),
                ..Default::default()
            },
        );
        base_quota.insert(
            "3p-5h".to_string(),
            QuotaBucket {
                remaining_fraction: Some(0.004),
                reset_in_seconds: Some(1_800),
                ..Default::default()
            },
        );
        base_quota.insert(
            "3p-weekly".to_string(),
            QuotaBucket {
                remaining_fraction: Some(0.50),
                reset_in_seconds: Some(172_800),
                ..Default::default()
            },
        );

        // Model without tier1 qualifier picks acme-weekly (7d), not tier1.
        let base_model = StatuslinePayload {
            model: Some(ModelInfo {
                id: Some("acme-4-base".to_string()),
                display_name: Some("Acme 4 Base".to_string()),
                effort: None,
            }),
            quota: Some(base_quota.clone()),
            ..Default::default()
        };
        assert_eq!(resolve_quota_weekly_at(&base_model, 0).unwrap().label, "7d");
        assert_eq!(resolve_quota_5h_at(&base_model, 0).unwrap().label, "5h");

        // Model with qualifier token in display_name matches acme-tier1-wtu-weekly dynamically.
        let qualified_display = StatuslinePayload {
            model: Some(ModelInfo {
                id: None,
                display_name: Some("Orchestrator (Tier1)".to_string()),
                effort: None,
            }),
            quota: Some(base_quota.clone()),
            ..Default::default()
        };
        assert_eq!(
            resolve_quota_weekly_at(&qualified_display, 0).unwrap().label,
            "tier1"
        );
        assert_eq!(resolve_quota_5h_at(&qualified_display, 0).unwrap().label, "5h");

        // Arbitrary unknown third-party model picks 3p buckets automatically.
        let unknown_3p = StatuslinePayload {
            model: Some(ModelInfo {
                id: Some("deepseek-r1".to_string()),
                display_name: Some("DeepSeek R1".to_string()),
                effort: None,
            }),
            quota: Some(base_quota),
            ..Default::default()
        };
        let g5 = resolve_quota_5h_at(&unknown_3p, 0).unwrap();
        assert_eq!(g5.label, "3p 5h");
        assert_eq!(format_quota_pct(g5.remaining_pct), "0.4%");
        assert_eq!(resolve_quota_weekly_at(&unknown_3p, 0).unwrap().label, "3p 7d");

        // Custom 24h / 1h / credit-only buckets work dynamically.
        let mut custom_quota = BTreeMap::new();
        custom_quota.insert(
            "acme-hourly".to_string(),
            QuotaBucket {
                remaining_fraction: None,
                remaining_amount: Some(450),
                reset_in_seconds: Some(1_200),
                ..Default::default()
            },
        );
        custom_quota.insert(
            "acme-24h".to_string(),
            QuotaBucket {
                remaining_fraction: Some(0.62),
                reset_in_seconds: Some(43_200),
                ..Default::default()
            },
        );
        let custom_payload = StatuslinePayload {
            model: Some(ModelInfo {
                id: Some("acme-pro".to_string()),
                ..Default::default()
            }),
            quota: Some(custom_quota),
            ..Default::default()
        };
        let c_short = resolve_quota_5h_at(&custom_payload, 0).unwrap();
        assert_eq!(c_short.label, "1h");
        assert_eq!(c_short.window_secs, 3_600);
        assert_eq!(c_short.remaining_amount, Some(450));
        assert!(!c_short.pace_behind);

        let c_day = resolve_quota_weekly_at(&custom_payload, 0).unwrap();
        assert_eq!(c_day.label, "24h");
        assert_eq!(c_day.window_secs, 86_400);
    }

    #[test]
    fn dynamic_scan_picks_tightest_matching_bucket_without_cross_binding() {
        let mut quota = BTreeMap::new();
        quota.insert(
            "acme-fast-5h".to_string(),
            QuotaBucket {
                remaining_fraction: Some(0.65),
                reset_in_seconds: Some(7_200),
                ..Default::default()
            },
        );
        quota.insert(
            "acme-pro-5h".to_string(),
            QuotaBucket {
                remaining_fraction: Some(0.12),
                reset_in_seconds: Some(3_600),
                ..Default::default()
            },
        );
        quota.insert(
            "3p-vendor-5h".to_string(),
            QuotaBucket {
                remaining_fraction: Some(0.01),
                reset_in_seconds: Some(900),
                ..Default::default()
            },
        );
        quota.insert(
            "3p-alpha-weekly".to_string(),
            QuotaBucket {
                remaining_fraction: Some(0.22),
                reset_in_seconds: Some(86_400),
                ..Default::default()
            },
        );
        quota.insert(
            "3p-beta-weekly".to_string(),
            QuotaBucket {
                remaining_fraction: Some(0.55),
                reset_in_seconds: Some(86_400),
                ..Default::default()
            },
        );
        quota.insert(
            "acme-base-weekly".to_string(),
            QuotaBucket {
                remaining_fraction: Some(0.02),
                reset_in_seconds: Some(86_400),
                ..Default::default()
            },
        );

        let acme = StatuslinePayload {
            model: Some(ModelInfo {
                id: Some("acme-4-other".to_string()),
                ..Default::default()
            }),
            quota: Some(quota.clone()),
            ..Default::default()
        };
        let g5 = resolve_quota_5h_at(&acme, 0).unwrap();
        assert_eq!(g5.label, "pro 5h");
        assert!((g5.remaining_pct - 12.0).abs() < 1e-6);

        let third_party = StatuslinePayload {
            model: Some(ModelInfo {
                id: Some("beta/model-5-5".to_string()),
                ..Default::default()
            }),
            quota: Some(quota),
            ..Default::default()
        };
        let tw = resolve_quota_weekly_at(&third_party, 0).unwrap();
        assert_eq!(tw.label, "3p 7d");
        assert_eq!(tw.bucket_key, "3p-beta-weekly");
        assert!((tw.remaining_pct - 55.0).abs() < 1e-6);
    }
}
