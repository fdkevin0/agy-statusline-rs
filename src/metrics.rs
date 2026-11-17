//! Pure metric derivation for cache hit rate, context tokens, and quota pacing.

use crate::statusline::{QuotaBucket, StatuslinePayload};

const WINDOW_5H_SECS: u64 = 18_000;
const WINDOW_WEEKLY_SECS: u64 = 604_800;
const PACE_BEHIND_DELTA_PCT: f64 = 15.0;

/// Resolved rate-limit view for `quota_5h` or `quota_weekly` widgets.
#[derive(Debug, Clone, PartialEq)]
pub struct QuotaView {
    /// Short bucket label (`"5h"`, `"3p 5h"`, `"7d"`, `"3p 7d"`, `"argon"`).
    pub label: &'static str,
    /// Remaining quota percentage (`0.0..=100.0`).
    pub remaining_pct: f64,
    /// Compact countdown until reset (e.g. `"3h10m"`), if `reset_in_seconds > 0`.
    pub reset_compact: Option<String>,
    /// True when remaining quota is lagging behind remaining time by `> 15%`.
    pub pace_behind: bool,
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

fn is_third_party_model(payload: &StatuslinePayload) -> bool {
    model_name_contains(
        payload,
        &["claude", "gpt", "sonnet", "opus", "haiku", "o3", "o4"],
    )
}

fn is_argon_model(payload: &StatuslinePayload) -> bool {
    model_name_contains(payload, &["argon"])
}

fn build_quota_view(
    bucket: &QuotaBucket,
    label: &'static str,
    window_secs: u64,
) -> Option<QuotaView> {
    let frac = bucket.remaining_fraction.filter(|f| f.is_finite())?;
    let remaining_pct = (frac * 100.0).clamp(0.0, 100.0);
    let reset_secs = bucket.reset_in_seconds.filter(|&s| s > 0);
    let reset_compact = reset_secs.map(format_duration_compact);
    let pace_behind = reset_secs
        .filter(|&s| s <= window_secs)
        .is_some_and(|s| {
            let time_left_pct = (s as f64) * 100.0 / (window_secs as f64);
            (time_left_pct - remaining_pct) > PACE_BEHIND_DELTA_PCT
        });
    Some(QuotaView {
        label,
        remaining_pct,
        reset_compact,
        pace_behind,
    })
}

/// Resolves the active 5-hour quota bucket (`gemini-5h` or `3p-5h`).
pub fn resolve_quota_5h(payload: &StatuslinePayload) -> Option<QuotaView> {
    let quota = payload.quota.as_ref()?;
    let third_party = is_third_party_model(payload);
    let candidates: [(&str, &str); 2] = if third_party {
        [("3p-5h", "3p 5h"), ("gemini-5h", "5h")]
    } else {
        [("gemini-5h", "5h"), ("3p-5h", "3p 5h")]
    };
    for (key, label) in candidates {
        if let Some(bucket) = quota.get(key) {
            if let Some(view) = build_quota_view(bucket, label, WINDOW_5H_SECS) {
                return Some(view);
            }
        }
    }
    None
}

/// Resolves the active weekly quota bucket (`gemini-weekly`, `3p-weekly`, or `gemini-argon-wtu-weekly`).
pub fn resolve_quota_weekly(payload: &StatuslinePayload) -> Option<QuotaView> {
    let quota = payload.quota.as_ref()?;
    let candidates: [(&str, &str); 3] = if is_argon_model(payload) {
        [
            ("gemini-argon-wtu-weekly", "argon"),
            ("gemini-weekly", "7d"),
            ("3p-weekly", "3p 7d"),
        ]
    } else if is_third_party_model(payload) {
        [
            ("3p-weekly", "3p 7d"),
            ("gemini-weekly", "7d"),
            ("gemini-argon-wtu-weekly", "argon"),
        ]
    } else {
        [
            ("gemini-weekly", "7d"),
            ("gemini-argon-wtu-weekly", "argon"),
            ("3p-weekly", "3p 7d"),
        ]
    };
    for (key, label) in candidates {
        if let Some(bucket) = quota.get(key) {
            if let Some(view) = build_quota_view(bucket, label, WINDOW_WEEKLY_SECS) {
                return Some(view);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::statusline::{ContextWindow, JsonUsage, ModelInfo};
    use std::collections::BTreeMap;

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
}
