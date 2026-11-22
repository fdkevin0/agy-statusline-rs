//! Per-bucket quota token capacity calibration and atomic XDG cache persistence.
//!
//! Learns total token capacity per rolling quota bucket from incremental per-conversation
//! token deltas (`total_input_tokens + total_output_tokens`) paired with quota fraction drops,
//! while guarding against conversation switches, external multi-client consumption, reset
//! cycles, and stale calibrations.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::db::is_absent_or_real_file;
use crate::fsutil::atomic_write;
use crate::metrics::{resolve_quota_views_with_reset_at, QuotaResolution, QuotaView};
use crate::statusline::StatuslinePayload;
use crate::timeutil::SECS_PER_DAY;

pub(crate) const SCHEMA_VERSION: u32 = 3;
pub(crate) const CACHE_FILENAME: &str = "quota_cal.json";
const LOCK_FILENAME: &str = "quota_cal.lock";

const MIN_TTL_SECS: u64 = 7 * (SECS_PER_DAY as u64);

const NEW_CYCLE_FRAC_JUMP: f64 = 0.04;
const NEW_CYCLE_RESET_SHIFT_SECS: u64 = 600;

const MIN_DELTA_FRAC: f64 = 0.02;
const MIN_DELTA_TOKENS: u64 = 2_000;
const MIN_CALIBRATED_TOTAL: f64 = 15_000.0;
const MAX_CALIBRATED_TOTAL: f64 = 1e11;
const MIN_PRIOR_RATIO: f64 = 0.25;
const MAX_PRIOR_RATIO: f64 = 4.5;
const EMA_CLAMP_LOW: f64 = 0.35;
const EMA_CLAMP_HIGH: f64 = 3.0;

const EXT_DIV_FRAC: f64 = 0.03;
const EXT_DIV_MAX_TOKENS: u64 = 500;
const EXT_DIV_LARGE_FRAC: f64 = 0.15;
const EXT_DIV_LARGE_MAX_TOKENS: u64 = 2_000;

/// Calibration state persisted for a single rolling quota bucket.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BucketCalibration {
    /// Calibrated total token budget for a full 100% cycle.
    pub calibrated_total_tokens: Option<f64>,
    /// Unix epoch seconds when `calibrated_total_tokens` was last updated.
    pub calibrated_at_secs: Option<u64>,
    /// Number of valid delta samples incorporated into the EMA.
    pub sample_count: u64,
    /// Conversation ID associated with the current incremental anchor.
    pub last_conversation_id: Option<String>,
    /// Quota fraction (`0.0..=1.0`) at the anchor point.
    pub anchor_fraction: Option<f64>,
    /// Conversation cumulative tokens at the anchor point.
    pub anchor_session_tokens: Option<u64>,
    /// Most recently observed quota fraction (`0.0..=1.0`).
    pub last_fraction: Option<f64>,
    /// Most recently observed bucket reset timestamp (Unix epoch seconds).
    pub last_reset_at_secs: Option<u64>,
    /// True when external consumption or anomalous drift invalidated calibration for this cycle.
    pub external_divergence_in_cycle: bool,
    /// Unix epoch seconds of the most recent calibration state change.
    pub updated_at_secs: u64,
}

/// Normalized, non-PII account and plan scope isolating calibrated quota budgets.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AccountScope {
    /// Normalized lowercase plan tier (e.g. `"pro"`, `"ultra"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_tier: Option<String>,
    /// Deterministic 64-bit FNV-1a hex digest of normalized lowercase user email.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email_hash: Option<String>,
}

fn fnv1a_64_hex(input: &str) -> String {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0100_0000_01b3;
    let mut hash = OFFSET_BASIS;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    format!("{hash:016x}")
}

fn normalize_scope_token(raw: Option<&str>) -> Option<String> {
    raw.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_ascii_lowercase)
}

impl AccountScope {
    fn from_payload(payload: &StatuslinePayload) -> Self {
        Self {
            plan_tier: normalize_scope_token(payload.plan_tier.as_deref()),
            email_hash: normalize_scope_token(payload.email.as_deref())
                .map(|email| fnv1a_64_hex(&email)),
        }
    }

    fn is_empty(&self) -> bool {
        self.plan_tier.is_none() && self.email_hash.is_none()
    }

    fn conflicts_with(&self, incoming: &Self) -> bool {
        let plan_conflict = matches!(
            (&self.plan_tier, &incoming.plan_tier),
            (Some(a), Some(b)) if a != b
        );
        let email_conflict = matches!(
            (&self.email_hash, &incoming.email_hash),
            (Some(a), Some(b)) if a != b
        );
        plan_conflict || email_conflict
    }
}

/// On-disk JSON store (`$XDG_CACHE_HOME/agy-statusline/quota_cal.json`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CalibrationStore {
    /// Store schema version (`3`).
    pub schema_version: u32,
    /// Non-PII account and plan scope for all stored buckets.
    #[serde(default, skip_serializing_if = "AccountScope::is_empty")]
    pub account_scope: AccountScope,
    /// Per-bucket calibration map keyed by raw quota bucket ID.
    pub buckets: BTreeMap<String, BucketCalibration>,
}

impl Default for CalibrationStore {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            account_scope: AccountScope::default(),
            buckets: BTreeMap::new(),
        }
    }
}

impl CalibrationStore {
    fn scope_conflicts(&self, incoming: &AccountScope) -> bool {
        self.account_scope.conflicts_with(incoming)
    }

    /// Reconciles stored scope against `incoming`: clears buckets on conflict,
    /// backfills absent fields without clearing, and preserves stored fields when incoming is absent.
    fn reconcile_scope(&mut self, incoming: &AccountScope) -> bool {
        if self.scope_conflicts(incoming) {
            self.buckets.clear();
            self.account_scope = incoming.clone();
            return true;
        }
        let mut changed = false;
        if self.account_scope.plan_tier.is_none() && incoming.plan_tier.is_some() {
            self.account_scope.plan_tier = incoming.plan_tier.clone();
            changed = true;
        }
        if self.account_scope.email_hash.is_none() && incoming.email_hash.is_some() {
            self.account_scope.email_hash = incoming.email_hash.clone();
            changed = true;
        }
        changed
    }
}

/// Loads [`CalibrationStore`] from `cache_dir/quota_cal.json`, falling back to default on missing/invalid data.
pub fn load_calibration_store(cache_dir: &Path) -> CalibrationStore {
    let path = cache_dir.join(CACHE_FILENAME);
    let Ok(raw) = fs::read_to_string(path) else {
        return CalibrationStore::default();
    };
    let Ok(store) = serde_json::from_str::<CalibrationStore>(&raw) else {
        return CalibrationStore::default();
    };
    if store.schema_version != SCHEMA_VERSION {
        return CalibrationStore::default();
    }
    store
}

fn try_lock_calibration(cache_dir: &Path) -> io::Result<fs::File> {
    fs::create_dir_all(cache_dir)?;
    let lock_path = cache_dir.join(LOCK_FILENAME);
    if !is_absent_or_real_file(&lock_path) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unsafe calibration lock file (symlink/non-regular file)",
        ));
    }
    let lock = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)?;
    lock.try_lock().map_err(io::Error::from)?;
    Ok(lock)
}

fn write_calibration_store(cache_dir: &Path, store: &CalibrationStore) -> io::Result<()> {
    let target = cache_dir.join(CACHE_FILENAME);
    let mut json = serde_json::to_string_pretty(store)
        .map_err(|e| io::Error::other(format!("failed to serialize calibration store: {e}")))?;
    json.push('\n');
    atomic_write(&target, json.as_bytes())
}

fn extract_session_tokens(payload: &StatuslinePayload) -> Option<u64> {
    let cw = payload.context_window.as_ref()?;
    if cw.total_input_tokens.is_none() && cw.total_output_tokens.is_none() {
        return None;
    }
    Some(
        cw.total_input_tokens
            .unwrap_or(0)
            .saturating_add(cw.total_output_tokens.unwrap_or(0)),
    )
}

fn is_valid_calibrated_total(v: f64) -> bool {
    v.is_finite() && (MIN_CALIBRATED_TOTAL..=MAX_CALIBRATED_TOTAL).contains(&v)
}

fn valid_calibrated_total(
    entry: &BucketCalibration,
    window_secs: u64,
    now_unix_secs: u64,
) -> Option<f64> {
    let ttl_secs = window_secs.saturating_mul(3).max(MIN_TTL_SECS);
    entry
        .calibrated_total_tokens
        .filter(|&total| is_valid_calibrated_total(total))
        .zip(entry.calibrated_at_secs.filter(|&at| at > 0))
        .filter(|&(_, at)| now_unix_secs.saturating_sub(at) <= ttl_secs)
        .map(|(total, _)| total)
}

fn remaining_tokens(total: f64, remaining_pct: f64) -> u64 {
    (total * (remaining_pct / 100.0).clamp(0.0, 1.0)).round() as u64
}

fn calibrate_bucket(
    entry: &mut BucketCalibration,
    mut view: QuotaView,
    reset_at_secs: Option<u64>,
    conv_id: Option<&str>,
    session_tokens: Option<u64>,
    now_unix_secs: u64,
) -> QuotaView {
    let frac = (view.remaining_pct / 100.0).clamp(0.0, 1.0);

    // Expire invalid or TTL-aged prior calibration before evaluating the current sample.
    if valid_calibrated_total(entry, view.window_secs, now_unix_secs).is_none() {
        entry.calibrated_total_tokens = None;
        entry.calibrated_at_secs = None;
        entry.sample_count = 0;
    }

    let reset_shifted_forward = match (reset_at_secs, entry.last_reset_at_secs) {
        (Some(cur), Some(prev)) => cur > prev.saturating_add(NEW_CYCLE_RESET_SHIFT_SECS),
        _ => false,
    };
    let jumped_up = entry
        .last_fraction
        .is_some_and(|prev_f| (frac - prev_f) >= NEW_CYCLE_FRAC_JUMP);
    let is_new_cycle = reset_shifted_forward || jumped_up;

    if is_new_cycle {
        entry.external_divergence_in_cycle = false;
        entry.anchor_fraction = Some(frac);
        entry.anchor_session_tokens = session_tokens;
        entry.last_conversation_id = conv_id.map(str::to_string);
    } else {
        let same_conv = match (conv_id, entry.last_conversation_id.as_deref()) {
            (Some(c), Some(prev_c)) => c == prev_c,
            _ => false,
        };
        if !same_conv || entry.anchor_fraction.is_none() || entry.anchor_session_tokens.is_none() {
            // Re-anchor cleanly when switching conversations within the same cycle.
            entry.anchor_fraction = Some(frac);
            entry.anchor_session_tokens = session_tokens;
            entry.last_conversation_id = conv_id.map(str::to_string);
        } else if let (Some(anchor_f), Some(anchor_tok), Some(cur_tok)) = (
            entry.anchor_fraction,
            entry.anchor_session_tokens,
            session_tokens,
        ) {
            let minor_upward_jitter =
                frac > anchor_f && (frac - anchor_f) < NEW_CYCLE_FRAC_JUMP;
            if cur_tok < anchor_tok || minor_upward_jitter {
                entry.anchor_fraction = Some(frac);
                entry.anchor_session_tokens = Some(cur_tok);
            } else if !entry.external_divergence_in_cycle {
                let delta_f = anchor_f - frac;
                let delta_tok = cur_tok - anchor_tok;

                if delta_f >= MIN_DELTA_FRAC && delta_tok >= MIN_DELTA_TOKENS {
                    let sample_total = (delta_tok as f64) / delta_f;
                    let within_bounds = is_valid_calibrated_total(sample_total);
                    let within_prior = entry.calibrated_total_tokens.is_none_or(|prior| {
                        sample_total >= prior * MIN_PRIOR_RATIO
                            && sample_total <= prior * MAX_PRIOR_RATIO
                    });
                    if within_bounds && within_prior {
                        let next_total = match entry.calibrated_total_tokens {
                            Some(prior) if entry.sample_count >= 1 => {
                                let clamped = sample_total
                                    .clamp(prior * EMA_CLAMP_LOW, prior * EMA_CLAMP_HIGH);
                                if entry.sample_count >= 2 {
                                    prior * 0.6 + clamped * 0.4
                                } else {
                                    prior * 0.4 + clamped * 0.6
                                }
                            }
                            _ => sample_total,
                        };
                        entry.calibrated_total_tokens = Some(next_total);
                        entry.calibrated_at_secs = Some(now_unix_secs);
                        entry.sample_count += 1;
                        entry.anchor_fraction = Some(frac);
                        entry.anchor_session_tokens = Some(cur_tok);
                    } else {
                        entry.external_divergence_in_cycle = true;
                    }
                } else if (delta_f >= EXT_DIV_FRAC && delta_tok < EXT_DIV_MAX_TOKENS)
                    || (delta_f >= EXT_DIV_LARGE_FRAC && delta_tok < EXT_DIV_LARGE_MAX_TOKENS)
                {
                    entry.external_divergence_in_cycle = true;
                }
            }
        }
    }

    entry.last_fraction = Some(frac);
    if is_new_cycle {
        // Clear stale reset_at when a new cycle starts at untouched 100% (suppressed reset_at),
        // preventing the first consumption tick from falsely tripping `reset_shifted_forward`.
        entry.last_reset_at_secs = reset_at_secs;
    } else if reset_at_secs.is_some() {
        entry.last_reset_at_secs = reset_at_secs;
    }

    view.est_remaining_tokens = entry
        .calibrated_total_tokens
        .map(|total| remaining_tokens(total, view.remaining_pct));
    view
}

fn opt_f64_differs(a: Option<f64>, b: Option<f64>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => !(x.is_finite() && y.is_finite()) || (x - y).abs() > 1e-9,
        (None, None) => false,
        _ => true,
    }
}

fn opt_reset_at_differs(a: Option<u64>, b: Option<u64>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => x.abs_diff(y) > 2,
        (None, None) => false,
        _ => true,
    }
}

fn bucket_state_changed(before: Option<&BucketCalibration>, after: &BucketCalibration) -> bool {
    let Some(prev) = before else {
        return true;
    };
    opt_f64_differs(prev.calibrated_total_tokens, after.calibrated_total_tokens)
        || prev.calibrated_at_secs != after.calibrated_at_secs
        || prev.sample_count != after.sample_count
        || prev.last_conversation_id != after.last_conversation_id
        || opt_f64_differs(prev.anchor_fraction, after.anchor_fraction)
        || opt_f64_differs(prev.last_fraction, after.last_fraction)
        || prev.anchor_session_tokens != after.anchor_session_tokens
        || opt_reset_at_differs(prev.last_reset_at_secs, after.last_reset_at_secs)
        || prev.external_divergence_in_cycle != after.external_divergence_in_cycle
}

type ResolvedViews = [Option<QuotaResolution>; 2];

fn resolve_views(payload: &StatuslinePayload, now_unix_secs: u64) -> ResolvedViews {
    let (short, weekly) = resolve_quota_views_with_reset_at(payload, now_unix_secs);
    [short, weekly]
}

fn calibrate_resolved(
    store: &mut CalibrationStore,
    payload: &StatuslinePayload,
    resolved: ResolvedViews,
    now_unix_secs: u64,
) -> ([Option<QuotaView>; 2], bool) {
    let incoming_scope = AccountScope::from_payload(payload);
    let mut changed = store.reconcile_scope(&incoming_scope);

    let conv_id = payload
        .conversation_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let session_tokens = extract_session_tokens(payload);
    let views = resolved.map(|slot| {
        let (view, reset_at) = slot?;
        if view.remaining_amount.is_some() {
            return Some(view);
        }
        let prev = store.buckets.get(&view.bucket_key).cloned();
        let entry = store.buckets.entry(view.bucket_key.clone()).or_default();
        let view = calibrate_bucket(
            entry,
            view,
            reset_at,
            conv_id,
            session_tokens,
            now_unix_secs,
        );
        if bucket_state_changed(prev.as_ref(), entry) {
            entry.updated_at_secs = now_unix_secs;
            changed = true;
        }
        Some(view)
    });
    (views, changed)
}

/// Pure calibration step updating `store` in-memory for both `5h` and `weekly` quota buckets.
#[cfg(test)]
fn calibrate_store_at(
    store: &mut CalibrationStore,
    payload: &StatuslinePayload,
    now_unix_secs: u64,
) -> (Option<QuotaView>, Option<QuotaView>) {
    let ([short, weekly], _) = calibrate_resolved(
        store,
        payload,
        resolve_views(payload, now_unix_secs),
        now_unix_secs,
    );
    (short, weekly)
}

/// Calibrates fractional quota views under a nonblocking cache lock. If the lock is
/// unavailable, uses any valid persisted estimates without recording a sample.
pub(crate) fn calibrate_quota_views_at(
    payload: &StatuslinePayload,
    cache_dir: Option<&Path>,
    now_unix_secs: u64,
) -> (Option<QuotaView>, Option<QuotaView>) {
    let resolved = resolve_views(payload, now_unix_secs);
    let dir = cache_dir.filter(|_| {
        resolved
            .iter()
            .flatten()
            .any(|(view, _)| view.remaining_amount.is_none())
    });

    let [short, weekly] = if let Some(dir) = dir {
        // Lock the sibling file so replacing the JSON cannot invalidate the lock.
        let lock = try_lock_calibration(dir);
        let mut store = load_calibration_store(dir);
        if let Ok(_lock) = lock {
            let (views, changed) = calibrate_resolved(&mut store, payload, resolved, now_unix_secs);
            if changed {
                let _ = write_calibration_store(dir, &store);
            }
            views
        } else {
            let incoming_scope = AccountScope::from_payload(payload);
            let allow_estimates = !store.scope_conflicts(&incoming_scope);
            resolved.map(|slot| {
                let (mut view, _) = slot?;
                if allow_estimates && view.remaining_amount.is_none() {
                    view.est_remaining_tokens = store
                        .buckets
                        .get(&view.bucket_key)
                        .and_then(|entry| {
                            valid_calibrated_total(entry, view.window_secs, now_unix_secs)
                        })
                        .map(|total| remaining_tokens(total, view.remaining_pct));
                }
                Some(view)
            })
        }
    } else {
        resolved.map(|slot| slot.map(|(view, _)| view))
    };
    (short, weekly)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::statusline::{ContextWindow, ModelInfo, QuotaBucket};

    fn make_payload(
        conv_id: &str,
        total_in: u64,
        total_out: u64,
        frac_5h: f64,
        reset_5h: u64,
        frac_weekly: f64,
        reset_weekly: u64,
    ) -> StatuslinePayload {
        let mut quota = BTreeMap::new();
        quota.insert(
            "acme-5h".to_string(),
            QuotaBucket {
                remaining_fraction: Some(frac_5h),
                reset_in_seconds: Some(reset_5h as i64),
                ..Default::default()
            },
        );
        quota.insert(
            "acme-weekly".to_string(),
            QuotaBucket {
                remaining_fraction: Some(frac_weekly),
                reset_in_seconds: Some(reset_weekly as i64),
                ..Default::default()
            },
        );
        StatuslinePayload {
            conversation_id: Some(conv_id.to_string()),
            model: Some(ModelInfo {
                id: Some("acme-4-base".to_string()),
                ..Default::default()
            }),
            context_window: Some(ContextWindow {
                total_input_tokens: Some(total_in),
                total_output_tokens: Some(total_out),
                ..Default::default()
            }),
            quota: Some(quota),
            ..Default::default()
        }
    }

    fn with_scope(
        mut payload: StatuslinePayload,
        plan_tier: Option<&str>,
        email: Option<&str>,
    ) -> StatuslinePayload {
        payload.plan_tier = plan_tier.map(str::to_string);
        payload.email = email.map(str::to_string);
        payload
    }

    #[test]
    fn calibrates_incrementally_and_blends_ema_within_conversation() {
        let mut store = CalibrationStore::default();
        let t0 = 1_795_000_000;

        // Tick 1: establishes anchor at 0.90, 10K tokens -> uncalibrated yet.
        let p1 = make_payload("conv-a", 8_000, 2_000, 0.90, 14_000, 0.95, 500_000);
        let (q5_1, _) = calibrate_store_at(&mut store, &p1, t0);
        assert_eq!(q5_1.unwrap().est_remaining_tokens, None);

        // Tick 2: +200K tokens for -0.10 quota => sample_total = 2.0M, est = 1.6M at 0.80.
        let p2 = make_payload("conv-a", 200_000, 10_000, 0.80, 13_900, 0.94, 499_900);
        let (q5_2, _) = calibrate_store_at(&mut store, &p2, t0 + 100);
        assert_eq!(q5_2.unwrap().est_remaining_tokens, Some(1_600_000));
        assert_eq!(store.buckets["acme-5h"].sample_count, 1);

        // Tick 3: +300K tokens for -0.10 quota => sample_total = 3.0M.
        // Second sample blends 0.4 * 2.0M + 0.6 * 3.0M = 2.6M; remaining at 0.70 = 1.82M.
        let p3 = make_payload("conv-a", 490_000, 20_000, 0.70, 13_800, 0.93, 499_800);
        let (q5_3, _) = calibrate_store_at(&mut store, &p3, t0 + 200);
        assert_eq!(q5_3.unwrap().est_remaining_tokens, Some(1_820_000));
        assert_eq!(store.buckets["acme-5h"].sample_count, 2);

        // Tick 4: +100K tokens for -0.05 quota => sample_total = 2.0M.
        // Third+ sample blends 0.6 * 2.6M + 0.4 * 2.0M = 2.36M; remaining at 0.65 = 1.534M.
        let p4 = make_payload("conv-a", 580_000, 30_000, 0.65, 13_700, 0.92, 499_700);
        let (q5_4, _) = calibrate_store_at(&mut store, &p4, t0 + 300);
        assert_eq!(q5_4.unwrap().est_remaining_tokens, Some(1_534_000));
        assert_eq!(store.buckets["acme-5h"].sample_count, 3);
    }

    #[test]
    fn conversation_switch_reanchors_while_preserving_prior_calibration() {
        let mut store = CalibrationStore::default();
        let t0 = 1_795_000_000;

        let p1 = make_payload("conv-1", 10_000, 0, 0.90, 14_000, 0.98, 500_000);
        calibrate_store_at(&mut store, &p1, t0);
        let p2 = make_payload("conv-1", 210_000, 0, 0.80, 13_900, 0.97, 499_900);
        calibrate_store_at(&mut store, &p2, t0 + 100);
        assert!((store.buckets["acme-5h"].calibrated_total_tokens.unwrap() - 2_000_000.0).abs() < 1e-3);

        // Switch to conv-2 (smaller per-conversation total 5K, quota moved to 0.70):
        // Must preserve 2.0M calibration -> est 1.4M, and re-anchor to (0.70, 5K).
        let p3 = make_payload("conv-2", 5_000, 0, 0.70, 13_800, 0.96, 499_800);
        let (q5_3, _) = calibrate_store_at(&mut store, &p3, t0 + 200);
        assert_eq!(q5_3.unwrap().est_remaining_tokens, Some(1_400_000));
        assert_eq!(store.buckets["acme-5h"].anchor_session_tokens, Some(5_000));
        assert!(!store.buckets["acme-5h"].external_divergence_in_cycle);
        assert_eq!(store.buckets["acme-5h"].sample_count, 1);
    }

    #[test]
    fn guards_external_divergence_zero_fraction_and_ttl_expiry() {
        let mut store = CalibrationStore::default();
        let t0 = 1_795_000_000;

        // Uncalibrated bucket drops by 5% with only 100 local tokens -> external divergence locked for cycle.
        let p1 = make_payload("conv-div", 1_000, 0, 0.90, 10_000, 0.90, 500_000);
        calibrate_store_at(&mut store, &p1, t0);
        let p2 = make_payload("conv-div", 1_100, 0, 0.85, 9_900, 0.90, 499_900);
        let (q5_2, _) = calibrate_store_at(&mut store, &p2, t0 + 100);
        assert_eq!(q5_2.unwrap().est_remaining_tokens, None);
        assert!(store.buckets["acme-5h"].external_divergence_in_cycle);

        // Subsequent large local delta in the SAME cycle remains blocked until reset.
        let p3 = make_payload("conv-div", 500_000, 0, 0.60, 9_800, 0.90, 499_800);
        let (q5_3, _) = calibrate_store_at(&mut store, &p3, t0 + 200);
        assert_eq!(q5_3.unwrap().est_remaining_tokens, None);

        // New cycle (frac jumps >= 0.04 & reset_at shifts > 600s) clears external divergence.
        let p4 = make_payload("conv-div", 500_000, 0, 1.00, 18_000, 0.90, 499_700);
        calibrate_store_at(&mut store, &p4, t0 + 300);
        assert!(!store.buckets["acme-5h"].external_divergence_in_cycle);

        // Calibrate 10M pool in new cycle, then hit frac = 0.0 -> Some(0).
        let p5 = make_payload("conv-div", 1_500_000, 0, 0.90, 17_900, 0.90, 499_600);
        calibrate_store_at(&mut store, &p5, t0 + 400);
        let p_zero = make_payload("conv-div", 1_500_000, 0, 0.00, 17_800, 0.90, 499_500);
        let (q5_zero, _) = calibrate_store_at(&mut store, &p_zero, t0 + 500);
        assert_eq!(q5_zero.unwrap().est_remaining_tokens, Some(0));

        // Advance past 7d TTL (MIN_TTL_SECS + 1) -> 5h calibration expires while weekly (21d TTL) would not.
        let p_expired = make_payload(
            "conv-div",
            1_500_000,
            0,
            0.50,
            17_000,
            0.85,
            400_000,
        );
        let (q5_exp, _) = calibrate_store_at(&mut store, &p_expired, t0 + 400 + MIN_TTL_SECS + 1);
        assert_eq!(q5_exp.unwrap().est_remaining_tokens, None);
        assert_eq!(store.buckets["acme-5h"].calibrated_total_tokens, None);
    }

    #[test]
    fn persists_atomically_to_isolated_cache_dir() {
        let dir = tempfile::tempdir().unwrap();
        let t0 = 1_795_000_000;

        let p1 = make_payload("conv-disk", 10_000, 0, 0.90, 14_000, 0.98, 500_000);
        let (q1, _) = calibrate_quota_views_at(&p1, Some(dir.path()), t0);
        assert_eq!(q1.unwrap().est_remaining_tokens, None);
        assert!(dir.path().join(CACHE_FILENAME).is_file());

        let p2 = make_payload("conv-disk", 210_000, 0, 0.80, 13_900, 0.97, 499_900);
        let (q2, _) = calibrate_quota_views_at(&p2, Some(dir.path()), t0 + 60);
        assert_eq!(q2.unwrap().est_remaining_tokens, Some(1_600_000));

        let reloaded = load_calibration_store(dir.path());
        assert_eq!(reloaded.buckets["acme-5h"].sample_count, 1);
    }

    #[test]
    fn idle_ticks_do_not_rewrite_quota_cal_json() {
        let dir = tempfile::tempdir().unwrap();
        let t0 = 1_795_000_000;

        let p1 = make_payload("conv-idle", 10_000, 0, 0.90, 14_000, 0.98, 500_000);
        calibrate_quota_views_at(&p1, Some(dir.path()), t0);
        let cal_path = dir.path().join(CACHE_FILENAME);
        let before_raw = fs::read_to_string(&cal_path).unwrap();

        // Idle tick 60s later: reset_in_seconds counts down by 60 (same reset_at), all meaningful fields unchanged.
        let p_idle = make_payload("conv-idle", 10_000, 0, 0.90, 13_940, 0.98, 499_940);
        calibrate_quota_views_at(&p_idle, Some(dir.path()), t0 + 60);
        let after_raw = fs::read_to_string(&cal_path).unwrap();
        assert_eq!(after_raw, before_raw);
        let reloaded = load_calibration_store(dir.path());
        assert_eq!(reloaded.buckets["acme-5h"].updated_at_secs, t0);
    }

    #[test]
    fn calibrate_quota_views_falls_back_to_read_only_cache_when_lock_held() {
        let dir = tempfile::tempdir().unwrap();
        let t0 = 1_795_000_000;
        let seeded = CalibrationStore {
            schema_version: SCHEMA_VERSION,
            buckets: BTreeMap::from([(
                "acme-5h".to_string(),
                BucketCalibration {
                    calibrated_total_tokens: Some(1_000_000.0),
                    calibrated_at_secs: Some(t0),
                    sample_count: 1,
                    updated_at_secs: t0,
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };
        write_calibration_store(dir.path(), &seeded).unwrap();
        let cal_path = dir.path().join(CACHE_FILENAME);
        let before = fs::read_to_string(&cal_path).unwrap();

        let _lock = try_lock_calibration(dir.path()).expect("acquire calibration lock");
        let payload = make_payload("conv-contended", 500_000, 0, 0.50, 12_000, 0.90, 400_000);
        let (q5, q_weekly) = calibrate_quota_views_at(&payload, Some(dir.path()), t0 + 60);

        assert_eq!(q5.unwrap().est_remaining_tokens, Some(500_000));
        assert_eq!(q_weekly.unwrap().est_remaining_tokens, None);
        assert_eq!(fs::read_to_string(&cal_path).unwrap(), before);
    }

    #[test]
    fn schema_version_mismatch_clears_stale_buckets() {
        let dir = tempfile::tempdir().unwrap();
        let stale = CalibrationStore {
            schema_version: SCHEMA_VERSION - 1,
            buckets: BTreeMap::from([(
                "acme-5h".to_string(),
                BucketCalibration {
                    calibrated_total_tokens: Some(1_000_000.0),
                    calibrated_at_secs: Some(1_795_000_000),
                    sample_count: 3,
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };
        write_calibration_store(dir.path(), &stale).unwrap();

        let loaded = load_calibration_store(dir.path());
        assert_eq!(loaded.schema_version, SCHEMA_VERSION);
        assert!(loaded.buckets.is_empty());
    }

    #[test]
    fn reset_at_jitter_within_two_seconds_does_not_rewrite_disk() {
        let dir = tempfile::tempdir().unwrap();
        let t0 = 1_795_000_000;

        let p1 = make_payload("conv-jitter", 10_000, 0, 0.90, 14_000, 0.98, 500_000);
        calibrate_quota_views_at(&p1, Some(dir.path()), t0);
        let cal_path = dir.path().join(CACHE_FILENAME);
        let before_raw = fs::read_to_string(&cal_path).unwrap();

        // +1s jitter in computed reset_at (t0 + 10 + 13_991 = t0 + 14_001 vs t0 + 14_000)
        let p_jitter_plus1 = make_payload("conv-jitter", 10_000, 0, 0.90, 13_991, 0.98, 499_989);
        calibrate_quota_views_at(&p_jitter_plus1, Some(dir.path()), t0 + 10);
        assert_eq!(fs::read_to_string(&cal_path).unwrap(), before_raw);

        // -1s jitter in computed reset_at
        let p_jitter_minus1 = make_payload("conv-jitter", 10_000, 0, 0.90, 13_979, 0.98, 499_981);
        calibrate_quota_views_at(&p_jitter_minus1, Some(dir.path()), t0 + 20);
        assert_eq!(fs::read_to_string(&cal_path).unwrap(), before_raw);
    }

    #[test]
    fn untouched_full_window_rolling_reset_does_not_rewrite_disk_or_trigger_new_cycle() {
        let dir = tempfile::tempdir().unwrap();
        let t0 = 1_795_000_000;
        let cal_path = dir.path().join(CACHE_FILENAME);

        // Rolling reset_time = now + window across 0s, +30s, +700s (> NEW_CYCLE_RESET_SHIFT_SECS).
        let p0 = make_payload("conv-full", 10_000, 0, 1.0, 18_000, 1.0, 604_800);
        let (q5_0, qw_0) = calibrate_quota_views_at(&p0, Some(dir.path()), t0);
        assert_eq!(q5_0.unwrap().reset_compact, None);
        assert_eq!(qw_0.unwrap().reset_compact, None);
        let before_raw = fs::read_to_string(&cal_path).unwrap();
        let loaded = load_calibration_store(dir.path());
        assert_eq!(loaded.buckets["acme-5h"].last_reset_at_secs, None);
        assert_eq!(loaded.buckets["acme-5h"].updated_at_secs, t0);

        let p1 = make_payload("conv-full", 10_000, 0, 1.0, 18_000, 1.0, 604_800);
        calibrate_quota_views_at(&p1, Some(dir.path()), t0 + 30);
        assert_eq!(fs::read_to_string(&cal_path).unwrap(), before_raw);

        let p2 = make_payload("conv-full", 10_000, 0, 1.0, 18_000, 1.0, 604_800);
        calibrate_quota_views_at(&p2, Some(dir.path()), t0 + 700);
        assert_eq!(fs::read_to_string(&cal_path).unwrap(), before_raw);

        // Also confirm in-memory that +30s and +700s rolling ticks never clear divergence via false is_new_cycle.
        let mut mem = CalibrationStore::default();
        calibrate_store_at(&mut mem, &p0, t0);
        mem.buckets
            .get_mut("acme-5h")
            .unwrap()
            .external_divergence_in_cycle = true;
        let (_, changed_30) =
            calibrate_resolved(&mut mem, &p1, resolve_views(&p1, t0 + 30), t0 + 30);
        assert!(!changed_30);
        assert!(mem.buckets["acme-5h"].external_divergence_in_cycle);
        let (_, changed_700) =
            calibrate_resolved(&mut mem, &p2, resolve_views(&p2, t0 + 700), t0 + 700);
        assert!(!changed_700);
        assert!(mem.buckets["acme-5h"].external_divergence_in_cycle);
    }

    #[test]
    fn cycle_reset_to_untouched_full_then_delayed_consumption_calibrates_cleanly() {
        let mut store = CalibrationStore::default();
        let t0 = 1_795_000_000;

        let p_old = make_payload("conv-cycle", 100_000, 0, 0.50, 10_000, 0.90, 500_000);
        calibrate_store_at(&mut store, &p_old, t0);
        assert_eq!(
            store.buckets["acme-5h"].last_reset_at_secs,
            Some(t0 + 10_000)
        );

        // Cycle resets to untouched 100% -> clears stale last_reset_at_secs to None.
        let t1 = t0 + 10_100;
        let p_reset = make_payload("conv-cycle", 100_000, 0, 1.00, 18_000, 0.90, 489_900);
        let (q5_reset, _) = calibrate_store_at(&mut store, &p_reset, t1);
        assert_eq!(q5_reset.unwrap().reset_compact, None);
        assert_eq!(store.buckets["acme-5h"].last_reset_at_secs, None);
        assert_eq!(store.buckets["acme-5h"].anchor_fraction, Some(1.0));

        // 15 minutes later: +200K tokens drops quota to 90% -> calibrates 2.0M pool without false shift.
        let t2 = t1 + 900;
        let p_use = make_payload("conv-cycle", 300_000, 0, 0.90, 17_100, 0.89, 489_000);
        let (q5_use, _) = calibrate_store_at(&mut store, &p_use, t2);
        assert_eq!(q5_use.unwrap().est_remaining_tokens, Some(1_800_000));
        assert_eq!(store.buckets["acme-5h"].sample_count, 1);
        assert_eq!(
            store.buckets["acme-5h"].last_reset_at_secs,
            Some(t2 + 17_100)
        );
    }

    #[test]
    fn account_and_plan_scope_isolates_calibration_and_lock_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let cal_path = dir.path().join(CACHE_FILENAME);
        let t0 = 1_795_000_000;

        // Calibrate 2.0M pool for ("Pro", "Alice@Example.COM").
        let p1 = with_scope(
            make_payload("conv-scope", 10_000, 0, 0.90, 14_000, 0.98, 500_000),
            Some("Pro"),
            Some("Alice@Example.COM"),
        );
        calibrate_quota_views_at(&p1, Some(dir.path()), t0);
        let p2 = with_scope(
            make_payload("conv-scope", 210_000, 0, 0.80, 13_940, 0.97, 499_940),
            Some("Pro"),
            Some("Alice@Example.COM"),
        );
        let (q5_2, _) = calibrate_quota_views_at(&p2, Some(dir.path()), t0 + 60);
        assert_eq!(q5_2.unwrap().est_remaining_tokens, Some(1_600_000));

        // Disk cache stores normalized plan and hashed email, never raw email PII.
        let raw_after_p2 = fs::read_to_string(&cal_path).unwrap();
        assert!(!raw_after_p2.contains("Alice"));
        assert!(!raw_after_p2.contains("alice"));
        assert!(!raw_after_p2.contains("example.com"));
        assert!(raw_after_p2.contains("\"plan_tier\": \"pro\""));
        assert!(raw_after_p2.contains("\"email_hash\""));

        // Missing email/plan_tier preserves existing calibration and stored scope without rewriting disk.
        let p_missing = with_scope(
            make_payload("conv-scope", 210_000, 0, 0.80, 13_880, 0.97, 499_880),
            None,
            None,
        );
        let (q5_missing, _) = calibrate_quota_views_at(&p_missing, Some(dir.path()), t0 + 120);
        assert_eq!(q5_missing.unwrap().est_remaining_tokens, Some(1_600_000));
        assert_eq!(fs::read_to_string(&cal_path).unwrap(), raw_after_p2);

        // Equivalent casing/whitespace preserves calibration without rewriting disk.
        let p_norm = with_scope(
            make_payload("conv-scope", 210_000, 0, 0.80, 13_820, 0.97, 499_820),
            Some("  PRO "),
            Some(" alice@example.com "),
        );
        let (q5_norm, _) = calibrate_quota_views_at(&p_norm, Some(dir.path()), t0 + 180);
        assert_eq!(q5_norm.unwrap().est_remaining_tokens, Some(1_600_000));
        assert_eq!(fs::read_to_string(&cal_path).unwrap(), raw_after_p2);

        // Switching plan_tier clears prior calibrated buckets and anchors.
        let p_ultra = with_scope(
            make_payload("conv-scope", 210_000, 0, 0.80, 13_760, 0.97, 499_760),
            Some("Ultra"),
            Some("alice@example.com"),
        );
        let (q5_ultra, _) = calibrate_quota_views_at(&p_ultra, Some(dir.path()), t0 + 240);
        assert_eq!(q5_ultra.unwrap().est_remaining_tokens, None);
        let after_plan_switch = load_calibration_store(dir.path());
        assert_eq!(
            after_plan_switch.account_scope.plan_tier.as_deref(),
            Some("ultra")
        );
        assert_eq!(after_plan_switch.buckets["acme-5h"].sample_count, 0);
        assert_eq!(
            after_plan_switch.buckets["acme-5h"].calibrated_total_tokens,
            None
        );

        // Calibrate 4.0M pool under ("Ultra", "alice@example.com").
        let p_ultra_cal = with_scope(
            make_payload("conv-scope", 610_000, 0, 0.70, 13_700, 0.96, 499_700),
            Some("Ultra"),
            Some("alice@example.com"),
        );
        let (q5_ultra_cal, _) = calibrate_quota_views_at(&p_ultra_cal, Some(dir.path()), t0 + 300);
        assert_eq!(q5_ultra_cal.unwrap().est_remaining_tokens, Some(2_800_000));

        // Lock-contention read-only path hides estimates on conflicting scope, keeps on matching/missing.
        let before_lock = fs::read_to_string(&cal_path).unwrap();
        let lock = try_lock_calibration(dir.path()).expect("acquire lock");
        let p_lock_plan_conflict = with_scope(
            make_payload("conv-scope", 610_000, 0, 0.70, 13_640, 0.96, 499_640),
            Some("Pro"),
            Some("alice@example.com"),
        );
        let (q5_lock_plan, _) =
            calibrate_quota_views_at(&p_lock_plan_conflict, Some(dir.path()), t0 + 360);
        assert_eq!(q5_lock_plan.unwrap().est_remaining_tokens, None);

        let p_lock_email_conflict = with_scope(
            make_payload("conv-scope", 610_000, 0, 0.70, 13_640, 0.96, 499_640),
            Some("Ultra"),
            Some("bob@example.com"),
        );
        let (q5_lock_email, _) =
            calibrate_quota_views_at(&p_lock_email_conflict, Some(dir.path()), t0 + 360);
        assert_eq!(q5_lock_email.unwrap().est_remaining_tokens, None);

        let p_lock_match = with_scope(
            make_payload("conv-scope", 610_000, 0, 0.70, 13_640, 0.96, 499_640),
            Some("Ultra"),
            Some("alice@example.com"),
        );
        let (q5_lock_match, _) = calibrate_quota_views_at(&p_lock_match, Some(dir.path()), t0 + 360);
        assert_eq!(q5_lock_match.unwrap().est_remaining_tokens, Some(2_800_000));
        assert_eq!(fs::read_to_string(&cal_path).unwrap(), before_lock);
        drop(lock);

        // Switching email clears prior calibrated buckets and records the new email hash.
        let p_bob = with_scope(
            make_payload("conv-scope", 610_000, 0, 0.70, 13_580, 0.96, 499_580),
            Some("Ultra"),
            Some("bob@example.com"),
        );
        let (q5_bob, _) = calibrate_quota_views_at(&p_bob, Some(dir.path()), t0 + 420);
        assert_eq!(q5_bob.unwrap().est_remaining_tokens, None);
        let after_bob = load_calibration_store(dir.path());
        assert_eq!(
            after_bob.account_scope.email_hash.as_deref(),
            Some(fnv1a_64_hex("bob@example.com").as_str())
        );
        assert_eq!(after_bob.buckets["acme-5h"].sample_count, 0);

        // Backfilling a previously absent scope field preserves calibrated buckets without clearing.
        let mut partial = CalibrationStore::default();
        let p_partial_1 = with_scope(
            make_payload("conv-part", 10_000, 0, 0.90, 14_000, 0.98, 500_000),
            Some("Pro"),
            None,
        );
        calibrate_store_at(&mut partial, &p_partial_1, t0);
        let p_partial_2 = with_scope(
            make_payload("conv-part", 210_000, 0, 0.80, 13_940, 0.97, 499_940),
            Some("Pro"),
            None,
        );
        calibrate_store_at(&mut partial, &p_partial_2, t0 + 60);
        let p_backfill = with_scope(
            make_payload("conv-part", 210_000, 0, 0.80, 13_880, 0.97, 499_880),
            Some("Pro"),
            Some("carol@example.com"),
        );
        let ([q5_backfill, _], backfill_changed) = calibrate_resolved(
            &mut partial,
            &p_backfill,
            resolve_views(&p_backfill, t0 + 120),
            t0 + 120,
        );
        assert!(backfill_changed);
        assert_eq!(q5_backfill.unwrap().est_remaining_tokens, Some(1_600_000));
        assert_eq!(
            partial.account_scope.email_hash.as_deref(),
            Some(fnv1a_64_hex("carol@example.com").as_str())
        );
    }
}
