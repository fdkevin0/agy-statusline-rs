//! Atomic `quota_snapshot.json` persistence and ring-buffer quota burn-rate tracking.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::statusline::QuotaBucket;
use crate::sys::fmt::{ascii_values_conflict, trimmed_non_empty};
use crate::sys::fs::{
    atomic_write_preserving_mode, is_safe_cache_target, read_bounded_regular_file, BoundedReadError,
};
use crate::sys::time::parse_rfc3339_unix_secs;

/// Filename stored under `$XDG_CACHE_HOME/agy-statusline/`.
pub const QUOTA_SNAPSHOT_FILE: &str = "quota_snapshot.json";
/// Maximum permitted snapshot file size on disk (`64 KiB`).
pub const MAX_QUOTA_SNAPSHOT_BYTES: u64 = 64 * 1024;
/// Maximum number of spaced historical samples retained per quota bucket.
pub const MAX_HISTORY_SAMPLES: usize = 16;
/// Minimum spacing between consecutive recorded samples for the same bucket (seconds).
pub const MIN_SAMPLE_SPACING_SECS: u64 = 60;
/// Reset-time forward jump (seconds) that triggers a window rollover reset.
pub const ROLLOVER_RESET_ADVANCE_SECS: i64 = 120;
/// Remaining-fraction upward jump that triggers a window rollover reset.
pub const ROLLOVER_FRACTION_JUMP: f64 = 0.02;

/// Origin of a persisted [`QuotaSnapshot`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaSource {
    /// Captured from `agy` statusline stdin JSON payload.
    #[default]
    Stdin,
    /// Fetched via loopback `daemon/hub_*.json` Connect RPC.
    HubRpc,
}

impl QuotaSource {
    /// Returns a compact human-readable label for diagnostics.
    pub fn label(self) -> &'static str {
        match self {
            Self::Stdin => "stdin",
            Self::HubRpc => "hub rpc",
        }
    }
}

/// Single timestamped remaining-fraction observation in a bucket's sample ring.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct QuotaSample {
    /// Unix epoch timestamp in seconds when the observation was recorded.
    pub ts: u64,
    /// Clamped remaining fraction (`0.0..=1.0`).
    pub remaining_fraction: f64,
}

/// Projected depletion computed from a bucket's sample history ring.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct BurnProjection {
    /// Consumption rate in percentage points per hour (`%/hr`), when enough history exists.
    pub burn_rate_pct_per_hr: Option<f64>,
    /// Projected seconds until quota reaches `0%` at the observed burn rate.
    pub eta_secs: Option<i64>,
    /// `Some(true/false)` when history is sufficient to project exhaustion against reset;
    /// `None` when callers should fall back to linear `pace_behind`.
    pub will_exhaust_before_reset: Option<bool>,
}

/// Compact ring buffer of spaced remaining-fraction samples for a single quota bucket.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BucketHistory {
    /// Unix epoch timestamp of the active window's reset time, if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reset_unix: Option<i64>,
    /// Spaced historical samples ordered ascending by `ts` (capped at [`MAX_HISTORY_SAMPLES`]).
    pub samples: Vec<QuotaSample>,
}

impl BucketHistory {
    /// Appends a sample to the ring buffer, resetting history on window rollover
    /// (when `reset_unix` advances beyond tolerance or `remaining_fraction` jumps up).
    pub fn record_sample(&mut self, ts: u64, remaining_fraction: f64, reset_unix: Option<i64>) {
        if !remaining_fraction.is_finite() {
            return;
        }
        let clamped = remaining_fraction.clamp(0.0, 1.0);
        let reset_advanced = match (self.reset_unix, reset_unix) {
            (Some(prev), Some(next)) => next.saturating_sub(prev) > ROLLOVER_RESET_ADVANCE_SECS,
            _ => false,
        };
        let fraction_jumped = self
            .samples
            .last()
            .is_some_and(|last| clamped > last.remaining_fraction + ROLLOVER_FRACTION_JUMP);

        if reset_advanced || fraction_jumped {
            self.samples.clear();
            self.reset_unix = reset_unix;
            self.samples.push(QuotaSample {
                ts,
                remaining_fraction: clamped,
            });
            return;
        }

        if let Some(next_reset) = reset_unix {
            if self.reset_unix.is_none() {
                self.reset_unix = Some(next_reset);
            }
        }

        if let Some(last) = self.samples.last() {
            if ts < last.ts || ts.saturating_sub(last.ts) < MIN_SAMPLE_SPACING_SECS {
                return;
            }
        }

        self.samples.push(QuotaSample {
            ts,
            remaining_fraction: clamped,
        });
        if self.samples.len() > MAX_HISTORY_SAMPLES {
            let excess = self.samples.len() - MAX_HISTORY_SAMPLES;
            self.samples.drain(..excess);
        }
    }

    /// Computes average consumption burn rate in percentage points per hour (`%/hr`)
    /// across the retained ring window, returning `None` until at least two spaced samples exist.
    pub fn burn_rate_pct_per_hr(&self) -> Option<f64> {
        let first = self.samples.first()?;
        let last = self.samples.last()?;
        if self.samples.len() < 2 {
            return None;
        }
        let dt_secs = last.ts.saturating_sub(first.ts);
        if dt_secs < MIN_SAMPLE_SPACING_SECS {
            return None;
        }
        let delta_frac = (first.remaining_fraction - last.remaining_fraction).max(0.0);
        Some((delta_frac * 100.0) * 3_600.0 / (dt_secs as f64))
    }

    /// Computes burn rate, depletion ETA, and exhaustion forecast against `reset_remaining_secs`.
    pub fn project_depletion(
        &self,
        current_remaining_pct: f64,
        reset_remaining_secs: Option<u64>,
    ) -> BurnProjection {
        let Some(rate) = self.burn_rate_pct_per_hr() else {
            return BurnProjection::default();
        };
        if current_remaining_pct <= 0.0 {
            return BurnProjection {
                burn_rate_pct_per_hr: Some(rate),
                eta_secs: Some(0),
                will_exhaust_before_reset: Some(reset_remaining_secs.is_some_and(|s| s > 0)),
            };
        }
        if rate <= 1e-9 {
            return BurnProjection {
                burn_rate_pct_per_hr: Some(0.0),
                eta_secs: None,
                will_exhaust_before_reset: Some(false),
            };
        }
        let eta_f = (current_remaining_pct / rate) * 3_600.0;
        let eta_secs = if eta_f.is_finite() {
            Some(eta_f.round().clamp(0.0, i64::MAX as f64) as i64)
        } else {
            None
        };
        let will_exhaust = reset_remaining_secs.is_some_and(|rem| {
            let rem_i64 = rem.min(i64::MAX as u64) as i64;
            eta_secs.is_some_and(|eta| eta < rem_i64)
        });
        BurnProjection {
            burn_rate_pct_per_hr: Some(rate),
            eta_secs,
            will_exhaust_before_reset: Some(will_exhaust),
        }
    }
}

/// Persisted quota, AI credits, tier, and sample history snapshot stored in `quota_snapshot.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct QuotaSnapshot {
    /// Unix epoch timestamp in seconds when this snapshot was last updated.
    pub updated_at: u64,
    /// Data source that produced this snapshot (`Stdin` or `HubRpc`).
    pub source: QuotaSource,
    /// Account plan tier (`"Free"`, `"Pro"`, `"Ultra"`, etc.).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_tier: Option<String>,
    /// Resolved Ultra multiplier label (`"5x"` or `"20x"`) when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ultra_tier_label: Option<String>,
    /// Authenticated user email address, if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// Available `GOOGLE_ONE_AI` / AI credits balance, if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ai_credits: Option<i64>,
    /// Latest normalized quota buckets keyed by bucket identifier.
    pub quota: BTreeMap<String, QuotaBucket>,
    /// Per-bucket ring-buffer sample history for burn-rate and depletion ETA calculation.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub history: BTreeMap<String, BucketHistory>,
}

impl QuotaSnapshot {
    /// Formats a human-readable tier label combining `plan_tier` and `ultra_tier_label`
    /// (e.g. `"Ultra 20x"`, `"Ultra 5x"`, `"Pro"`).
    pub fn display_plan_tier(&self) -> Option<String> {
        let tier = trimmed_non_empty(self.plan_tier.as_deref());
        let ultra = trimmed_non_empty(self.ultra_tier_label.as_deref());
        match (tier, ultra) {
            (Some(t), Some(u)) if t.to_ascii_lowercase().contains(&u.to_ascii_lowercase()) => {
                Some(t.to_string())
            }
            (Some(t), Some(u)) => Some(format!("{t} {u}")),
            (Some(t), None) => Some(t.to_string()),
            (None, Some(u)) => Some(format!("Ultra {u}")),
            (None, None) => None,
        }
    }

    /// Records the current `quota` map into `history` ring buffers at `now_unix_secs`
    /// and prunes history keys that no longer exist in `quota`.
    pub fn record_bucket_samples(&mut self, now_unix_secs: u64) {
        self.history.retain(|k, _| self.quota.contains_key(k));
        for (key, bucket) in &self.quota {
            if let Some((frac, reset_unix)) = effective_bucket_fraction(bucket, now_unix_secs) {
                self.history.entry(key.clone()).or_default().record_sample(
                    now_unix_secs,
                    frac,
                    reset_unix,
                );
            }
        }
    }

    /// Merges an incoming observation into this snapshot, clearing history on account change
    /// and advancing per-bucket ring buffers.
    pub fn merge_observation(&mut self, incoming: QuotaSnapshot, now_unix_secs: u64) {
        if ascii_values_conflict(self.email.as_deref(), incoming.email.as_deref()) {
            self.history.clear();
            self.quota.clear();
            self.plan_tier = None;
            self.ultra_tier_label = None;
            self.ai_credits = None;
        }

        self.updated_at = now_unix_secs;
        self.source = incoming.source;
        if incoming.email.is_some() {
            self.email = incoming.email;
        }
        if incoming.plan_tier.is_some() {
            self.plan_tier = incoming.plan_tier;
            self.ultra_tier_label = incoming.ultra_tier_label;
        } else if incoming.ultra_tier_label.is_some() {
            self.ultra_tier_label = incoming.ultra_tier_label;
        }
        if incoming.ai_credits.is_some() {
            self.ai_credits = incoming.ai_credits;
        }
        if !incoming.quota.is_empty() {
            self.quota = incoming.quota;
        }
        self.record_bucket_samples(now_unix_secs);
    }
}

/// Resolves a bucket's effective remaining fraction and reset timestamp for sample history,
/// accounting for Proto3 `0%` zero-value omission when an active future reset exists.
pub(crate) fn effective_bucket_fraction(
    bucket: &QuotaBucket,
    now_unix_secs: u64,
) -> Option<(f64, Option<i64>)> {
    if bucket.disabled == Some(true) {
        return None;
    }
    let reset_unix = resolve_bucket_reset_unix(bucket, now_unix_secs);
    if let Some(frac) = bucket.remaining_fraction.filter(|f| f.is_finite()) {
        return Some((frac.clamp(0.0, 1.0), reset_unix));
    }
    if bucket.remaining_amount.is_none() {
        let has_future_reset = reset_unix.is_some_and(|r| r > now_unix_secs as i64);
        if has_future_reset {
            return Some((0.0, reset_unix));
        }
    }
    None
}

fn resolve_bucket_reset_unix(bucket: &QuotaBucket, now_unix_secs: u64) -> Option<i64> {
    if let Some(ts) = bucket
        .reset_time
        .as_deref()
        .and_then(parse_rfc3339_unix_secs)
    {
        return Some(ts);
    }
    bucket
        .reset_in_seconds
        .filter(|&s| s > 0)
        .and_then(|s| (now_unix_secs as i64).checked_add(s))
}

/// Diagnostic inspection result for `quota_snapshot.json` on disk.
#[derive(Debug, Clone)]
pub enum SnapshotFileState {
    /// Snapshot file does not exist.
    Missing,
    /// Snapshot path is a symlink and was rejected.
    Symlink,
    /// Snapshot path is not a regular file.
    NotAFile,
    /// Snapshot file exceeds [`MAX_QUOTA_SNAPSHOT_BYTES`].
    Oversized(u64),
    /// Snapshot could not be read or parsed.
    Invalid(String),
    /// Valid snapshot loaded from disk.
    Loaded(QuotaSnapshot),
}

/// Returns `<cache_dir>/quota_snapshot.json`.
pub fn quota_snapshot_path(cache_dir: &Path) -> PathBuf {
    cache_dir.join(QUOTA_SNAPSHOT_FILE)
}

/// Inspects and loads `quota_snapshot.json` from `cache_dir` with symlink and size-cap guards.
pub fn inspect_quota_snapshot_file(cache_dir: &Path) -> SnapshotFileState {
    let path = quota_snapshot_path(cache_dir);
    let (bytes, _) = match read_bounded_regular_file(&path, MAX_QUOTA_SNAPSHOT_BYTES) {
        Ok(read) => read,
        Err(BoundedReadError::Missing) => return SnapshotFileState::Missing,
        Err(BoundedReadError::Symlink) => return SnapshotFileState::Symlink,
        Err(BoundedReadError::NotAFile) => return SnapshotFileState::NotAFile,
        Err(BoundedReadError::Oversized(size)) => return SnapshotFileState::Oversized(size),
        Err(BoundedReadError::Io(e)) => return SnapshotFileState::Invalid(e.to_string()),
    };
    match serde_json::from_slice::<QuotaSnapshot>(&bytes) {
        Ok(snapshot) => SnapshotFileState::Loaded(snapshot),
        Err(e) => SnapshotFileState::Invalid(e.to_string()),
    }
}

/// Loads `quota_snapshot.json` from `cache_dir`, returning `Ok(None)` if absent and rejecting
/// symlinks, non-regular files, and files exceeding `64 KiB`.
pub fn load_quota_snapshot(cache_dir: &Path) -> io::Result<Option<QuotaSnapshot>> {
    match inspect_quota_snapshot_file(cache_dir) {
        SnapshotFileState::Missing => Ok(None),
        SnapshotFileState::Loaded(snap) => Ok(Some(snap)),
        SnapshotFileState::Symlink => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to read symlinked quota_snapshot.json",
        )),
        SnapshotFileState::NotAFile => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "quota_snapshot.json is not a regular file",
        )),
        SnapshotFileState::Oversized(size) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("quota_snapshot.json exceeds {MAX_QUOTA_SNAPSHOT_BYTES} bytes ({size})"),
        )),
        SnapshotFileState::Invalid(msg) => Err(io::Error::new(io::ErrorKind::InvalidData, msg)),
    }
}

/// Atomically persists `snapshot` to `<cache_dir>/quota_snapshot.json` via
/// `atomic_write_preserving_mode`, rejecting symlinks/non-regular targets and payloads `> 64 KiB`.
pub fn save_quota_snapshot(cache_dir: &Path, snapshot: &QuotaSnapshot) -> io::Result<()> {
    let path = quota_snapshot_path(cache_dir);
    if !is_safe_cache_target(&path) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to overwrite symlink or non-regular quota_snapshot.json",
        ));
    }

    let mut bytes = serde_json::to_vec(snapshot)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    if bytes.len() as u64 > MAX_QUOTA_SNAPSHOT_BYTES {
        let mut trimmed = snapshot.clone();
        trimmed.history.clear();
        bytes = serde_json::to_vec(&trimmed)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        if bytes.len() as u64 > MAX_QUOTA_SNAPSHOT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "serialized quota_snapshot.json exceeds {MAX_QUOTA_SNAPSHOT_BYTES} bytes ({})",
                    bytes.len()
                ),
            ));
        }
    }

    atomic_write_preserving_mode(&path, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn bucket_history_computes_burn_rate_and_eta_projection() {
        let mut hist = BucketHistory::default();
        hist.record_sample(1_000, 0.80, Some(19_000));
        assert_eq!(hist.burn_rate_pct_per_hr(), None);

        // Sample within MIN_SAMPLE_SPACING_SECS is ignored so the ring stays spaced.
        hist.record_sample(1_030, 0.78, Some(19_000));
        assert_eq!(hist.samples.len(), 1);

        // 1 hour later, 20% consumed -> 20%/hr burn rate; 60% remaining -> 3h (10_800s) ETA.
        hist.record_sample(4_600, 0.60, Some(19_000));
        let rate = hist.burn_rate_pct_per_hr().unwrap();
        assert!((rate - 20.0).abs() < 1e-6);

        let will_exhaust = hist.project_depletion(60.0, Some(14_400));
        assert_eq!(will_exhaust.eta_secs, Some(10_800));
        assert_eq!(will_exhaust.will_exhaust_before_reset, Some(true));

        let safe = hist.project_depletion(60.0, Some(7_200));
        assert_eq!(safe.eta_secs, Some(10_800));
        assert_eq!(safe.will_exhaust_before_reset, Some(false));
    }

    #[test]
    fn bucket_history_resets_on_rollover_and_caps_ring_at_16_points() {
        let mut hist = BucketHistory::default();
        let reset = Some(19_000);
        hist.record_sample(1_000, 1.00, reset);
        hist.record_sample(1_120, 0.98, reset);
        hist.record_sample(1_240, 0.96, reset);
        hist.record_sample(1_360, 0.94, reset);
        hist.record_sample(1_480, 0.92, reset);
        hist.record_sample(1_600, 0.90, reset);
        hist.record_sample(1_720, 0.88, reset);
        hist.record_sample(1_840, 0.86, reset);
        hist.record_sample(1_960, 0.84, reset);
        hist.record_sample(2_080, 0.82, reset);
        hist.record_sample(2_200, 0.80, reset);
        hist.record_sample(2_320, 0.78, reset);
        hist.record_sample(2_440, 0.76, reset);
        hist.record_sample(2_560, 0.74, reset);
        hist.record_sample(2_680, 0.72, reset);
        hist.record_sample(2_800, 0.70, reset);
        hist.record_sample(2_920, 0.68, reset);
        hist.record_sample(3_040, 0.66, reset);
        assert_eq!(hist.samples.len(), MAX_HISTORY_SAMPLES);
        assert_eq!(hist.samples.first().unwrap().ts, 1_240);
        assert_eq!(hist.samples.last().unwrap().ts, 3_040);

        // Reset time advances significantly -> clears ring.
        hist.record_sample(5_000, 0.95, Some(37_000));
        assert_eq!(hist.samples.len(), 1);
        assert_eq!(hist.reset_unix, Some(37_000));
        assert!((hist.samples[0].remaining_fraction - 0.95).abs() < 1e-9);

        // Record downward step, then upward fraction jump -> clears ring.
        hist.record_sample(5_600, 0.70, Some(37_000));
        assert_eq!(hist.samples.len(), 2);
        hist.record_sample(6_200, 0.90, Some(37_000));
        assert_eq!(hist.samples.len(), 1);
        assert!((hist.samples[0].remaining_fraction - 0.90).abs() < 1e-9);
    }

    #[test]
    fn merge_observation_clears_ultra_label_on_tier_downgrade_and_preserves_when_absent() {
        let mut snap = QuotaSnapshot {
            updated_at: 1_000,
            source: QuotaSource::HubRpc,
            plan_tier: Some("Ultra".to_string()),
            ultra_tier_label: Some("20x".to_string()),
            email: Some("dev@example.com".to_string()),
            ..Default::default()
        };

        // Update without plan_tier preserves existing tier and ultra label.
        let partial = QuotaSnapshot {
            updated_at: 1_100,
            source: QuotaSource::Stdin,
            plan_tier: None,
            ultra_tier_label: None,
            email: Some("dev@example.com".to_string()),
            ..Default::default()
        };
        snap.merge_observation(partial, 1_100);
        assert_eq!(snap.plan_tier.as_deref(), Some("Ultra"));
        assert_eq!(snap.ultra_tier_label.as_deref(), Some("20x"));
        assert_eq!(snap.display_plan_tier().as_deref(), Some("Ultra 20x"));

        // Tier downgrade to Pro with ultra_tier_label=None clears stale "20x".
        let downgrade = QuotaSnapshot {
            updated_at: 1_200,
            source: QuotaSource::HubRpc,
            plan_tier: Some("Pro".to_string()),
            ultra_tier_label: None,
            email: Some("dev@example.com".to_string()),
            ..Default::default()
        };
        snap.merge_observation(downgrade, 1_200);
        assert_eq!(snap.plan_tier.as_deref(), Some("Pro"));
        assert_eq!(snap.ultra_tier_label, None);
        assert_eq!(snap.display_plan_tier().as_deref(), Some("Pro"));
    }

    #[test]
    fn snapshot_roundtrip_persists_fields_and_rejects_oversized_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut snap = QuotaSnapshot {
            updated_at: 1_700_000_000,
            source: QuotaSource::HubRpc,
            plan_tier: Some("Ultra".to_string()),
            ultra_tier_label: Some("20x".to_string()),
            email: Some("dev@example.com".to_string()),
            ai_credits: Some(420),
            quota: BTreeMap::from([(
                "gemini-5h".to_string(),
                QuotaBucket {
                    remaining_fraction: Some(0.75),
                    reset_in_seconds: Some(12_000),
                    ..Default::default()
                },
            )]),
            history: BTreeMap::new(),
        };
        snap.record_bucket_samples(1_700_000_000);
        assert_eq!(snap.display_plan_tier().as_deref(), Some("Ultra 20x"));

        save_quota_snapshot(dir.path(), &snap).unwrap();
        let loaded = load_quota_snapshot(dir.path()).unwrap().unwrap();
        assert_eq!(loaded.updated_at, 1_700_000_000);
        assert_eq!(loaded.source, QuotaSource::HubRpc);
        assert_eq!(loaded.plan_tier.as_deref(), Some("Ultra"));
        assert_eq!(loaded.ultra_tier_label.as_deref(), Some("20x"));
        assert_eq!(loaded.email.as_deref(), Some("dev@example.com"));
        assert_eq!(loaded.ai_credits, Some(420));
        assert_eq!(loaded.quota["gemini-5h"].remaining_fraction, Some(0.75));
        assert_eq!(loaded.history["gemini-5h"].samples.len(), 1);

        // Oversized snapshot (> 64 KiB) is rejected on load.
        fs::write(
            quota_snapshot_path(dir.path()),
            vec![b' '; (MAX_QUOTA_SNAPSHOT_BYTES + 1) as usize],
        )
        .unwrap();
        assert_eq!(
            load_quota_snapshot(dir.path()).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_load_and_save_reject_symlinks() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("outside.json");
        fs::write(&target, r#"{"updated_at":100}"#).unwrap();
        symlink(&target, quota_snapshot_path(dir.path())).unwrap();

        assert_eq!(
            load_quota_snapshot(dir.path()).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            save_quota_snapshot(dir.path(), &QuotaSnapshot::default())
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }
}
