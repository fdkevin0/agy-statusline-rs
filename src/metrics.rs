//! Pure metric derivation for cache hit rate, context tokens, and dynamic quota pacing.

use std::collections::BTreeSet;

use crate::statusline::{QuotaBucket, StatuslinePayload};
use crate::timeutil::{parse_rfc3339_unix_secs, SECS_PER_DAY as SECS_PER_DAY_I64};

const SECS_PER_HOUR: u64 = 3_600;
const SECS_PER_DAY: u64 = SECS_PER_DAY_I64 as u64;
const SECS_PER_WEEK: u64 = 7 * SECS_PER_DAY;
const SECS_PER_MONTH: u64 = 30 * SECS_PER_DAY;
const PACE_BEHIND_DELTA_PCT: f64 = 15.0;
const UNTOUCHED_WINDOW_TOLERANCE_SECS: u64 = 60;
const UNTOUCHED_FULL_FRAC_MIN: f64 = 1.0 - 1e-6;
const STRUCTURAL_NOISE_TOKENS: &[&str] = &["wtu", "quota", "pool", "limit"];

/// Resolved rate-limit view for `quota_5h` or `quota_weekly` widgets.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct QuotaView {
    /// Dynamic bucket label (e.g. `"5h"`, `"3p 5h"`, `"7d"`, `"3p 7d"`, `"24h"`).
    pub(crate) label: String,
    /// Remaining quota percentage (`0.0..=100.0`).
    pub(crate) remaining_pct: f64,
    /// Remaining credit units when `remaining_fraction` is absent and `remaining_amount >= 0`.
    pub(crate) remaining_amount: Option<i64>,
    /// Compact countdown until reset (e.g. `"3h10m"`), if positive remaining time is known.
    pub(crate) reset_compact: Option<String>,
    /// True when remaining quota is lagging behind remaining time by `> 15%`.
    pub(crate) pace_behind: bool,
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
pub(crate) fn valid_cache_usage(payload: &StatuslinePayload) -> Option<(f64, u64)> {
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
pub(crate) fn current_context_tokens(payload: &StatuslinePayload) -> Option<u64> {
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
    Some(inp.saturating_add(read).saturating_add(create))
}

/// Formats a duration in seconds into a compact human-readable string (`45s`, `12m`, `3h10m`, `5d12h`).
fn format_duration_compact(secs: u64) -> String {
    if secs < 60 {
        return format!("{secs}s");
    }
    if secs < SECS_PER_HOUR {
        return format!("{}m", secs / 60);
    }
    if secs < SECS_PER_DAY {
        let h = secs / SECS_PER_HOUR;
        let m = (secs % SECS_PER_HOUR) / 60;
        return if m == 0 {
            format!("{h}h")
        } else {
            format!("{h}h{m}m")
        };
    }
    let d = secs / SECS_PER_DAY;
    let h = (secs % SECS_PER_DAY) / SECS_PER_HOUR;
    if h == 0 {
        format!("{d}d")
    } else {
        format!("{d}d{h}h")
    }
}

/// Formats a validated remaining quota percentage (`0.0..=100.0`) without rounding sub-1% values to `0%`.
///
/// Returns `"0%"` strictly when `remaining_pct <= 0.0`, `"<0.1%"`
/// when `0.0 < pct < 0.1`, `"{:.1}%"` when `0.1 <= pct < 1.0`, and rounded `"{:.0}%"`
/// when `pct >= 1.0`.
pub(crate) fn format_quota_pct(remaining_pct: f64) -> String {
    if remaining_pct <= 0.0 {
        "0%".to_string()
    } else if remaining_pct < 0.1 {
        "<0.1%".to_string()
    } else if remaining_pct < 1.0 {
        format!("{remaining_pct:.1}%")
    } else {
        format!("{:.0}%", remaining_pct.round())
    }
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
        "monthly" | "month" => return Some((SECS_PER_MONTH, "mo".to_string())),
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

/// Resolves remaining seconds until quota reset, suppressing rolling `now + window_secs`
/// timestamps on untouched 100% buckets so idle ticks do not render a synthetic full-window countdown.
fn resolve_remaining_reset_secs(
    bucket: &QuotaBucket,
    now_unix_secs: u64,
    remaining_frac: Option<f64>,
    window_secs: u64,
) -> Option<u64> {
    let remaining_secs = match bucket.reset_in_seconds {
        Some(secs) if secs <= 0 => None,
        Some(secs) => Some(secs as u64),
        None => bucket
            .reset_time
            .as_deref()
            .and_then(parse_rfc3339_unix_secs)
            .and_then(|reset_unix| {
                let diff = i128::from(reset_unix) - i128::from(now_unix_secs);
                (diff > 0).then(|| diff.min(i128::from(u64::MAX)) as u64)
            }),
    };
    let is_untouched_full = remaining_frac.is_some_and(|frac| frac >= UNTOUCHED_FULL_FRAC_MIN)
        && remaining_secs
            .is_some_and(|s| s.abs_diff(window_secs) <= UNTOUCHED_WINDOW_TOLERANCE_SECS);
    if is_untouched_full {
        None
    } else {
        remaining_secs
    }
}

fn build_quota_view(
    desc: &BucketDescriptor,
    bucket: &QuotaBucket,
    now_unix_secs: u64,
) -> Option<(QuotaView, bool)> {
    let (remaining_frac, remaining_pct, remaining_amount) =
        if let Some(frac) = bucket.remaining_fraction.filter(|f| f.is_finite()) {
            let clamped = frac.clamp(0.0, 1.0);
            (Some(clamped), clamped * 100.0, None)
        } else {
            let amt = bucket.remaining_amount.filter(|&a| a >= 0)?;
            (None, 0.0, Some(amt))
        };
    let has_frac = remaining_frac.is_some();
    let remaining_secs =
        resolve_remaining_reset_secs(bucket, now_unix_secs, remaining_frac, desc.window_secs);
    let reset_compact = remaining_secs.map(format_duration_compact);
    let pace_behind = has_frac
        && remaining_secs
            .filter(|&s| s <= desc.window_secs)
            .is_some_and(|s| {
                let time_left_pct = (s as f64) * 100.0 / (desc.window_secs as f64);
                (time_left_pct - remaining_pct) > PACE_BEHIND_DELTA_PCT
            });
    Some((
        QuotaView {
            label: synthesize_label(desc),
            remaining_pct,
            remaining_amount,
            reset_compact,
            pace_behind,
        },
        has_frac,
    ))
}

/// Model-to-provider affinity derived across all quota descriptors (including disabled buckets).
#[derive(Debug, Clone, PartialEq, Eq)]
enum ModelAffinity {
    /// Model matched one or more 1p provider namespaces present in the quota map.
    KnownProvider(BTreeSet<String>),
    /// Model identity is known, but does not match any 1p provider in the quota map.
    ThirdParty,
    /// Payload has no model identity (`id` and `display_name` absent or empty).
    Unknown,
}

fn resolve_model_affinity(
    model_tokens: &BTreeSet<String>,
    all_descriptors: &[(BucketDescriptor, &QuotaBucket)],
) -> ModelAffinity {
    if model_tokens.is_empty() {
        return ModelAffinity::Unknown;
    }

    // Infer 1p provider affinity across both active and disabled buckets so disabling all
    // 1p buckets for a model still identifies it as 1p instead of falling back to `3p-*`.
    let mut effective_providers = BTreeSet::new();
    for (desc, _) in all_descriptors {
        let Some(ref p) = desc.provider else {
            continue;
        };
        let direct_match = model_tokens.contains(p);
        let qualifier_match =
            !desc.qualifiers.is_empty() && desc.qualifiers.iter().all(|q| model_tokens.contains(q));
        if direct_match || qualifier_match {
            effective_providers.insert(p.clone());
        }
    }

    if effective_providers.is_empty() {
        ModelAffinity::ThirdParty
    } else {
        ModelAffinity::KnownProvider(effective_providers)
    }
}

/// Ordered bucket-to-model affinity precedence (higher variants win).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum BucketMatchTier {
    /// 3p bucket when model identity is absent and qualifiers are unmatched.
    ThirdPartyMismatch,
    /// 3p bucket when model identity is absent and qualifiers are empty.
    ThirdPartyFallback,
    /// 1p bucket from an unrelated provider when model identity is known.
    UnrelatedProvider,
    /// Qualified 1p bucket when model identity is absent.
    NoModelQualified,
    /// 1p bucket matching provider with unmatched qualifiers.
    ProviderPartialQualifier,
    /// Unqualified 1p bucket with no model identity, or 3p bucket for a 3p model with unmatched qualifiers.
    GenericOrThirdPartyPartial,
    /// Unqualified match on active provider, exact qualifier-only match, or unqualified 3p match for a 3p model.
    ProviderOrThirdPartyExact,
    /// Exact qualified match on active provider (or qualified 3p match for a 3p model).
    ExactQualifiedMatch,
}

type AffinityScore = (BucketMatchTier, usize, isize);

fn score_bucket(
    desc: &BucketDescriptor,
    model_tokens: &BTreeSet<String>,
    affinity: &ModelAffinity,
) -> Option<AffinityScore> {
    let matched_q = desc
        .qualifiers
        .iter()
        .filter(|q| model_tokens.contains(*q))
        .count();
    let unmatched_q = desc.qualifiers.len() - matched_q;

    let tier = match (desc.is_3p, affinity) {
        // Known 1p models never cross-match 3p pools when their 1p buckets are disabled.
        (true, ModelAffinity::KnownProvider(_)) => return None,
        (true, ModelAffinity::ThirdParty) => {
            if unmatched_q == 0 && matched_q > 0 {
                BucketMatchTier::ExactQualifiedMatch
            } else if unmatched_q == 0 {
                BucketMatchTier::ProviderOrThirdPartyExact
            } else {
                BucketMatchTier::GenericOrThirdPartyPartial
            }
        }
        (true, ModelAffinity::Unknown) => {
            if unmatched_q == 0 {
                BucketMatchTier::ThirdPartyFallback
            } else {
                BucketMatchTier::ThirdPartyMismatch
            }
        }
        (false, ModelAffinity::KnownProvider(providers)) => {
            let provider_matched = desc
                .provider
                .as_ref()
                .is_some_and(|p| providers.contains(p));
            if provider_matched && unmatched_q == 0 && matched_q > 0 {
                BucketMatchTier::ExactQualifiedMatch
            } else if (provider_matched && unmatched_q == 0) || (matched_q > 0 && unmatched_q == 0)
            {
                BucketMatchTier::ProviderOrThirdPartyExact
            } else if provider_matched {
                BucketMatchTier::ProviderPartialQualifier
            } else {
                BucketMatchTier::UnrelatedProvider
            }
        }
        (false, ModelAffinity::ThirdParty) => BucketMatchTier::UnrelatedProvider,
        (false, ModelAffinity::Unknown) => {
            if unmatched_q == 0 {
                BucketMatchTier::GenericOrThirdPartyPartial
            } else {
                BucketMatchTier::NoModelQualified
            }
        }
    };

    Some((tier, matched_q, -(unmatched_q as isize)))
}

struct Candidate {
    score: AffinityScore,
    has_frac: bool,
    view: QuotaView,
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

fn update_best_candidate(slot: &mut Option<Candidate>, cand: Candidate) {
    let replace = match slot {
        None => true,
        Some(cur) => {
            cand.score > cur.score
                || (cand.score == cur.score && is_tighter_equal_score(&cand, cur))
        }
    };
    if replace {
        *slot = Some(cand);
    }
}

/// Resolves both the active short-window (`< 24h`) and multi-day (`>= 24h`) quota buckets
/// in a single pass over `payload.quota`.
pub(crate) fn resolve_quota_views(
    payload: &StatuslinePayload,
    now_unix_secs: u64,
) -> (Option<QuotaView>, Option<QuotaView>) {
    let Some(quota) = payload.quota.as_ref() else {
        return (None, None);
    };
    let model_tokens = collect_model_tokens(payload);

    let all_descriptors: Vec<(BucketDescriptor, &QuotaBucket)> = quota
        .iter()
        .filter_map(|(key, bucket)| parse_bucket_descriptor(key).map(|desc| (desc, bucket)))
        .collect();
    let affinity = resolve_model_affinity(&model_tokens, &all_descriptors);

    let mut best_short: Option<Candidate> = None;
    let mut best_multi: Option<Candidate> = None;
    for (desc, bucket) in all_descriptors {
        if bucket.disabled == Some(true) {
            continue;
        }
        let Some(score) = score_bucket(&desc, &model_tokens, &affinity) else {
            continue;
        };
        let Some((view, has_frac)) = build_quota_view(&desc, bucket, now_unix_secs) else {
            continue;
        };
        let cand = Candidate {
            score,
            has_frac,
            view,
        };
        if desc.window_secs < SECS_PER_DAY {
            update_best_candidate(&mut best_short, cand);
        } else {
            update_best_candidate(&mut best_multi, cand);
        }
    }

    (best_short.map(|c| c.view), best_multi.map(|c| c.view))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::statusline::{ContextWindow, JsonUsage, ModelInfo};
    use std::collections::BTreeMap;

    fn resolve_quota_5h_at(payload: &StatuslinePayload, now_unix_secs: u64) -> Option<QuotaView> {
        resolve_quota_views(payload, now_unix_secs).0
    }

    fn resolve_quota_weekly_at(
        payload: &StatuslinePayload,
        now_unix_secs: u64,
    ) -> Option<QuotaView> {
        resolve_quota_views(payload, now_unix_secs).1
    }

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
        let q5 = resolve_quota_5h_at(&acme_payload, 0).unwrap();
        assert_eq!(q5.label, "5h");
        assert!((q5.remaining_pct - 40.0).abs() < 1e-6);
        assert_eq!(q5.reset_compact.as_deref(), Some("4h"));
        assert!(q5.pace_behind);

        let qw = resolve_quota_weekly_at(&acme_payload, 0).unwrap();
        assert_eq!(qw.label, "tier1");
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
        let q3p = resolve_quota_5h_at(&third_party_payload, 0).unwrap();
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
        assert_eq!(q5.label, "5h");
        assert!((q5.remaining_pct - 10.0).abs() < 1e-6);
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
        assert_eq!(c_short.remaining_amount, Some(450));
        assert!(!c_short.pace_behind);

        let c_day = resolve_quota_weekly_at(&custom_payload, 0).unwrap();
        assert_eq!(c_day.label, "24h");
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
        assert!((tw.remaining_pct - 55.0).abs() < 1e-6);
    }

    #[test]
    fn current_context_tokens_saturates_on_i64_max() {
        let payload = StatuslinePayload {
            context_window: Some(ContextWindow {
                current_usage: Some(JsonUsage {
                    input_tokens: Some(i64::MAX),
                    output_tokens: None,
                    cache_read_input_tokens: Some(i64::MAX),
                    cache_creation_input_tokens: Some(i64::MAX),
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(current_context_tokens(&payload), Some(u64::MAX));
    }

    #[test]
    fn untouched_full_window_suppresses_rolling_reset_time_and_keeps_active_windows() {
        // 2026-11-18T12:00:00Z is 1_795_003_200; 5h earlier is 1_794_985_200.
        let now = 1_795_003_200 - 18_000;

        let untouched_exact = StatuslinePayload {
            model: Some(ModelInfo {
                id: Some("acme-4-base".to_string()),
                ..Default::default()
            }),
            quota: Some(BTreeMap::from([
                (
                    "acme-5h".to_string(),
                    QuotaBucket {
                        remaining_fraction: Some(1.0),
                        reset_in_seconds: Some(18_000),
                        ..Default::default()
                    },
                ),
                (
                    "acme-weekly".to_string(),
                    QuotaBucket {
                        remaining_fraction: Some(1.0),
                        reset_in_seconds: Some(604_800 + 60),
                        ..Default::default()
                    },
                ),
            ])),
            ..Default::default()
        };
        let (short_res, weekly_res) = resolve_quota_views(&untouched_exact, now);
        let q5 = short_res.unwrap();
        assert_eq!(q5.reset_compact, None);
        assert!(!q5.pace_behind);
        let qw = weekly_res.unwrap();
        assert_eq!(qw.reset_compact, None);
        assert!(!qw.pace_behind);

        // Lower -60s boundary and RFC3339 reset_time fallback are also suppressed at 100%.
        let untouched_rfc3339 = StatuslinePayload {
            model: Some(ModelInfo {
                id: Some("acme-4-base".to_string()),
                ..Default::default()
            }),
            quota: Some(BTreeMap::from([
                (
                    "acme-5h".to_string(),
                    QuotaBucket {
                        remaining_fraction: Some(1.0),
                        reset_time: Some("2026-11-18T12:00:00Z".to_string()),
                        ..Default::default()
                    },
                ),
                (
                    "acme-weekly".to_string(),
                    QuotaBucket {
                        remaining_fraction: Some(1.0),
                        reset_in_seconds: Some(604_800 - 60),
                        ..Default::default()
                    },
                ),
            ])),
            ..Default::default()
        };
        let (short_rfc, weekly_low) = resolve_quota_views(&untouched_rfc3339, now);
        let q5_rfc = short_rfc.unwrap();
        assert_eq!(q5_rfc.reset_compact, None);
        assert!(!q5_rfc.pace_behind);
        let qw_low = weekly_low.unwrap();
        assert_eq!(qw_low.reset_compact, None);
        assert!(!qw_low.pace_behind);

        // 100% bucket whose window started (> 60s elapsed) keeps reset_compact.
        let started_full = StatuslinePayload {
            model: Some(ModelInfo {
                id: Some("acme-4-base".to_string()),
                ..Default::default()
            }),
            quota: Some(BTreeMap::from([(
                "acme-5h".to_string(),
                QuotaBucket {
                    remaining_fraction: Some(1.0),
                    reset_in_seconds: Some(18_000 - 120),
                    ..Default::default()
                },
            )])),
            ..Default::default()
        };
        let q5_started = resolve_quota_views(&started_full, now).0.unwrap();
        assert_eq!(q5_started.reset_compact.as_deref(), Some("4h58m"));
        assert!(!q5_started.pace_behind);

        // 99% bucket keeps reset_compact even when reset_in_seconds == window_secs.
        let consumed_99 = StatuslinePayload {
            model: Some(ModelInfo {
                id: Some("acme-4-base".to_string()),
                ..Default::default()
            }),
            quota: Some(BTreeMap::from([(
                "acme-5h".to_string(),
                QuotaBucket {
                    remaining_fraction: Some(0.99),
                    reset_in_seconds: Some(18_000),
                    ..Default::default()
                },
            )])),
            ..Default::default()
        };
        let q5_99 = resolve_quota_views(&consumed_99, now).0.unwrap();
        assert_eq!(q5_99.reset_compact.as_deref(), Some("5h"));
        assert!(!q5_99.pace_behind);
    }

    #[test]
    fn parses_monthly_and_month_windows() {
        assert_eq!(
            parse_window_token("monthly"),
            Some((30 * SECS_PER_DAY, "mo".to_string()))
        );
        assert_eq!(
            parse_window_token("month"),
            Some((30 * SECS_PER_DAY, "mo".to_string()))
        );

        let acme_monthly = StatuslinePayload {
            model: Some(ModelInfo {
                id: Some("acme-pro".to_string()),
                ..Default::default()
            }),
            quota: Some(BTreeMap::from([(
                "acme-monthly".to_string(),
                QuotaBucket {
                    remaining_fraction: Some(0.75),
                    reset_in_seconds: Some(15 * SECS_PER_DAY as i64),
                    ..Default::default()
                },
            )])),
            ..Default::default()
        };
        let qm = resolve_quota_weekly_at(&acme_monthly, 0).unwrap();
        assert_eq!(qm.label, "mo");
        assert!((qm.remaining_pct - 75.0).abs() < 1e-6);

        let third_party_month = StatuslinePayload {
            model: Some(ModelInfo {
                id: Some("vendorx-pro".to_string()),
                ..Default::default()
            }),
            quota: Some(BTreeMap::from([(
                "3p-month".to_string(),
                QuotaBucket {
                    remaining_fraction: Some(0.50),
                    reset_in_seconds: Some(10 * SECS_PER_DAY as i64),
                    ..Default::default()
                },
            )])),
            ..Default::default()
        };
        let q3m = resolve_quota_weekly_at(&third_party_month, 0).unwrap();
        assert_eq!(q3m.label, "3p mo");
        assert!((q3m.remaining_pct - 50.0).abs() < 1e-6);
    }

    #[test]
    fn disabled_first_party_buckets_never_cross_match_third_party() {
        let quota = BTreeMap::from([
            (
                "gemini-5h".to_string(),
                QuotaBucket {
                    remaining_fraction: Some(0.50),
                    reset_in_seconds: Some(3_600),
                    disabled: Some(true),
                    ..Default::default()
                },
            ),
            (
                "3p-5h".to_string(),
                QuotaBucket {
                    remaining_fraction: Some(0.80),
                    reset_in_seconds: Some(7_200),
                    ..Default::default()
                },
            ),
            (
                "acme-tier1-weekly".to_string(),
                QuotaBucket {
                    remaining_fraction: Some(0.60),
                    reset_in_seconds: Some(86_400),
                    disabled: Some(true),
                    ..Default::default()
                },
            ),
            (
                "3p-weekly".to_string(),
                QuotaBucket {
                    remaining_fraction: Some(0.75),
                    reset_in_seconds: Some(172_800),
                    ..Default::default()
                },
            ),
        ]);

        // Gemini model sees disabled `gemini-5h` -> identified as KnownProvider("gemini"), rejects `3p-*`.
        let gemini = StatuslinePayload {
            model: Some(ModelInfo {
                id: Some("gemini-4-pro".to_string()),
                ..Default::default()
            }),
            quota: Some(quota.clone()),
            ..Default::default()
        };
        assert_eq!(resolve_quota_5h_at(&gemini, 0), None);
        assert_eq!(resolve_quota_weekly_at(&gemini, 0), None);

        // Qualifier-matched model sees disabled `acme-tier1-weekly` -> identified as KnownProvider("acme"), rejects `3p-*`.
        let qualified = StatuslinePayload {
            model: Some(ModelInfo {
                id: None,
                display_name: Some("Orchestrator (Tier1)".to_string()),
                ..Default::default()
            }),
            quota: Some(quota.clone()),
            ..Default::default()
        };
        assert_eq!(resolve_quota_weekly_at(&qualified, 0), None);
        assert_eq!(resolve_quota_5h_at(&qualified, 0), None);

        // Third-party model still matches active `3p-5h` and `3p-weekly`.
        let third_party = StatuslinePayload {
            model: Some(ModelInfo {
                id: Some("claude-sonnet-4".to_string()),
                ..Default::default()
            }),
            quota: Some(quota),
            ..Default::default()
        };
        let q5 = resolve_quota_5h_at(&third_party, 0).unwrap();
        assert_eq!(q5.label, "3p 5h");
        assert!((q5.remaining_pct - 80.0).abs() < 1e-6);
        let qw = resolve_quota_weekly_at(&third_party, 0).unwrap();
        assert_eq!(qw.label, "3p 7d");
        assert!((qw.remaining_pct - 75.0).abs() < 1e-6);
    }
}
