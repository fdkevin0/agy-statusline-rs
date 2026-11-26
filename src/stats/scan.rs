//! Read-only SQLite `gen_metadata` and `steps.metadata` scanner.

mod conversation;
mod cost_cache;
mod global_cache;

#[allow(unused_imports)]
pub(crate) use conversation::collect_conversation_stats_with_diagnostics;
pub(in crate::stats) use conversation::{
    collect_conversation_stats_report, conversation_watch_fingerprint,
    conversations_list_fingerprint, list_conversations, ConversationWatchFingerprint,
    ConversationsListFingerprint,
};
pub(crate) use cost_cache::{
    estimate_conversation_cost_usd_with_parent_conn, lookup_cached_conversation_cost,
};
pub(crate) use global_cache::STATS_SCAN_CACHE_FILE;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::pricing::{is_effort_token, normalize_model_key, round_usd_6, PricingCatalog};
use super::SqliteStats;
use crate::agy::db::{
    compute_median, conversation_id_from_db_path, is_absent_or_real_file, is_real_dir,
    is_safe_sqlite_file, is_valid_uuid, open_readonly_db, safe_sqlite_metadata, sidecar_path,
};
use crate::agy::protobuf::{
    extract_gen_metadata_record, extract_step_metadata_record, extract_trajectory_created_at_secs,
    is_foreign_cascade_id, RetryAttemptRecord, RetryStatusClass, UsageAttemptRecord, MAX_TTFT_SECS,
    MAX_TURN_SPEED_TPS,
};
use crate::sys::fs::modified_timestamp;
use crate::sys::time::TimeZoneSpec;

const STEP_TYPE_ERROR_MESSAGE: i64 = 17;
const STEP_TYPE_CHECKPOINT: i64 = 23;

/// Aggregated retry, error-step, and compaction-checkpoint diagnostics extracted from SQLite DBs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct ScanDiagnostics {
    /// Total actionable retry attempts (excluding user cancellations).
    #[serde(default)]
    pub retry_count: u64,
    /// Actionable retries classified as HTTP 429 / rate limit / quota exhaustion.
    #[serde(default)]
    pub retries_429: u64,
    /// Actionable retries classified as HTTP 503 / capacity / overloaded.
    #[serde(default)]
    pub retries_503: u64,
    /// Actionable retries classified as HTTP 500/502/504 upstream errors.
    #[serde(default)]
    pub retries_5xx: u64,
    /// Actionable retries classified as timeouts.
    #[serde(default)]
    pub retries_timeout: u64,
    /// All other actionable retries (disconnect, model output, unclassified).
    #[serde(default)]
    pub retries_other: u64,
    /// Cumulative retry backoff delay in seconds.
    #[serde(default)]
    pub total_retry_delay_secs: f64,
    /// Count of own error steps (`steps.step_type == 17`).
    #[serde(default)]
    pub error_steps: u64,
    /// Count of own compaction checkpoint steps (`steps.step_type == 23`).
    #[serde(default)]
    pub compaction_steps: u64,
}

impl ScanDiagnostics {
    /// Records one decoded [`RetryAttemptRecord`] if it represents an actionable anomaly.
    pub fn record_retry(&mut self, attempt: &RetryAttemptRecord) {
        if !attempt.is_anomaly() {
            return;
        }
        self.retry_count = self.retry_count.saturating_add(1);
        match attempt.status_class {
            RetryStatusClass::RateLimit429 => {
                self.retries_429 = self.retries_429.saturating_add(1);
            }
            RetryStatusClass::Capacity503 => {
                self.retries_503 = self.retries_503.saturating_add(1);
            }
            RetryStatusClass::Upstream5xx => {
                self.retries_5xx = self.retries_5xx.saturating_add(1);
            }
            RetryStatusClass::Timeout => {
                self.retries_timeout = self.retries_timeout.saturating_add(1);
            }
            RetryStatusClass::Canceled => {}
            RetryStatusClass::Disconnect
            | RetryStatusClass::ModelOutput
            | RetryStatusClass::Other => {
                self.retries_other = self.retries_other.saturating_add(1);
            }
        }
        if let Some(delay) = attempt
            .retry_delay_secs
            .filter(|d| d.is_finite() && *d > 0.0)
        {
            self.total_retry_delay_secs += delay;
        }
    }

    /// Merges another [`ScanDiagnostics`] accumulator into `self`.
    pub fn merge(&mut self, other: &Self) {
        self.retry_count = self.retry_count.saturating_add(other.retry_count);
        self.retries_429 = self.retries_429.saturating_add(other.retries_429);
        self.retries_503 = self.retries_503.saturating_add(other.retries_503);
        self.retries_5xx = self.retries_5xx.saturating_add(other.retries_5xx);
        self.retries_timeout = self.retries_timeout.saturating_add(other.retries_timeout);
        self.retries_other = self.retries_other.saturating_add(other.retries_other);
        if other.total_retry_delay_secs.is_finite() && other.total_retry_delay_secs > 0.0 {
            self.total_retry_delay_secs += other.total_retry_delay_secs;
        }
        self.error_steps = self.error_steps.saturating_add(other.error_steps);
        self.compaction_steps = self.compaction_steps.saturating_add(other.compaction_steps);
    }

    /// Count of non-429, non-503 actionable retries (`5xx + timeout + other`).
    pub fn retries_non_429_503(&self) -> u64 {
        self.retries_5xx
            .saturating_add(self.retries_timeout)
            .saturating_add(self.retries_other)
    }

    /// Returns `true` when no retries, error steps, or compaction checkpoints were recorded.
    pub fn is_empty(&self) -> bool {
        self.retry_count == 0 && self.error_steps == 0 && self.compaction_steps == 0
    }
}

/// Raw unpriced scan output for a single conversation SQLite database.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(super) struct ScannedDbData {
    #[serde(default)]
    pub(super) turns: Vec<RawTurn>,
    #[serde(default)]
    pub(super) diagnostics: ScanDiagnostics,
}

/// Full output of a global SQLite scan including diagnostics and per-DB cache stats.
#[derive(Debug, Clone)]
pub(crate) struct GlobalScanResult {
    pub(crate) sqlite: SqliteStats,
    pub(crate) events: Vec<UsageEvent>,
    pub(crate) diagnostics: ScanDiagnostics,
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) cache_hits: usize,
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) cache_misses: usize,
}

/// File-level change fingerprint for a SQLite conversation DB and its `-wal` sidecar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct DbFileFingerprint {
    pub(super) db_mtime_secs: i64,
    pub(super) db_mtime_nanos: u32,
    pub(super) db_ctime_secs: i64,
    pub(super) db_ctime_nanos: u32,
    pub(super) db_size: u64,
    /// Normalized to `0` when `wal_size == 0` so AGY startup touching idle 0-byte
    /// `-wal` files does not trigger redundant re-scans.
    pub(super) wal_mtime_secs: i64,
    pub(super) wal_mtime_nanos: u32,
    pub(super) wal_ctime_secs: i64,
    pub(super) wal_ctime_nanos: u32,
    pub(super) wal_size: u64,
}

#[cfg(unix)]
fn status_change_timestamp(meta: &fs::Metadata) -> (i64, u32) {
    use std::os::unix::fs::MetadataExt;
    let secs = meta.ctime();
    let nanos = meta.ctime_nsec().clamp(0, 999_999_999) as u32;
    (secs, nanos)
}

#[cfg(not(unix))]
fn status_change_timestamp(_meta: &fs::Metadata) -> (i64, u32) {
    (0, 0)
}

pub(super) fn db_file_fingerprint(db_path: &Path) -> Option<DbFileFingerprint> {
    let db_meta = fs::symlink_metadata(db_path).ok()?;
    let db_ft = db_meta.file_type();
    if db_ft.is_symlink() || !db_ft.is_file() {
        return None;
    }
    if !is_absent_or_real_file(&sidecar_path(db_path, "-shm")) {
        return None;
    }
    let (db_mtime_secs, db_mtime_nanos) = modified_timestamp(&db_meta);
    let (db_ctime_secs, db_ctime_nanos) = status_change_timestamp(&db_meta);
    let db_size = db_meta.len();

    let (wal_mtime_secs, wal_mtime_nanos, wal_ctime_secs, wal_ctime_nanos, wal_size) =
        match fs::symlink_metadata(sidecar_path(db_path, "-wal")) {
            Ok(wal_meta) => {
                let wal_ft = wal_meta.file_type();
                if wal_ft.is_symlink() || !wal_ft.is_file() {
                    return None;
                }
                if wal_meta.len() > 0 {
                    let (mtime_secs, mtime_nanos) = modified_timestamp(&wal_meta);
                    let (ctime_secs, ctime_nanos) = status_change_timestamp(&wal_meta);
                    (
                        mtime_secs,
                        mtime_nanos,
                        ctime_secs,
                        ctime_nanos,
                        wal_meta.len(),
                    )
                } else {
                    (0, 0, 0, 0, 0)
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => (0, 0, 0, 0, 0),
            Err(_) => return None,
        };

    Some(DbFileFingerprint {
        db_mtime_secs,
        db_mtime_nanos,
        db_ctime_secs,
        db_ctime_nanos,
        db_size,
        wal_mtime_secs,
        wal_mtime_nanos,
        wal_ctime_secs,
        wal_ctime_nanos,
        wal_size,
    })
}

fn is_effort_qualifier(tag: &str) -> bool {
    let norm: String = tag
        .trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|c| !matches!(c, ' ' | '-' | '_'))
        .collect();
    is_effort_token(&norm)
}

/// Derives a clean display model group name strictly from AGY metadata without hardcoded models.
///
/// Strips trailing parenthetical effort tiers (e.g. `(Low)`, `(Medium)`, `(High)`) while
/// preserving non-effort qualifiers (e.g. `(Orchestrator)` or tier/backend labels).
fn derive_model_name(
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

/// Origin of a scanned usage event (`antigravity-cli` vs desktop `antigravity`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum UsageSource {
    Cli,
    Desktop,
}

impl UsageSource {
    fn from_root_label(label: &str) -> Self {
        if label == "desktop" {
            Self::Desktop
        } else {
            Self::Cli
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct UsageEvent {
    pub(super) source: UsageSource,
    pub(super) local_day: i64,
    pub(super) model_group: String,
    pub(super) pricing_key: Option<String>,
    pub(super) input_tokens: u64,
    pub(super) cache_read_tokens: u64,
    pub(super) cache_write_tokens: u64,
    pub(super) output_tokens: u64,
    pub(super) thinking_tokens: u64,
    pub(super) total_tokens: u64,
    pub(super) cost_usd: Option<f64>,
    pub(super) decode_speed: Option<f64>,
    pub(super) ttft_secs: Option<f64>,
}

fn canonical_or_self(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn root_path_key(root: &Path) -> String {
    canonical_or_self(root).to_string_lossy().into_owned()
}

#[derive(Debug, Clone)]
struct DbTarget {
    source: &'static str,
    root: PathBuf,
    conversation_id: String,
    path: PathBuf,
    db_mtime_secs: Option<i64>,
    db_size_bytes: u64,
}

const TS_QUALITY_NONE: u8 = 0;
const TS_QUALITY_DB_MTIME: u8 = 1;
const TS_QUALITY_TRAJECTORY: u8 = 2;
const TS_QUALITY_SECONDARY: u8 = 3;
const TS_QUALITY_STEP_PRIMARY: u8 = 4;

fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

fn is_zero_u8(v: &u8) -> bool {
    *v == 0
}

/// Timezone- and pricing-independent turn record extracted from a SQLite conversation DB.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct RawTurn {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) ts_secs: Option<i64>,
    #[serde(default, skip_serializing_if = "is_zero_u8")]
    pub(super) ts_quality: u8,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub(super) model_enum: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) response_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) model_display_name: Option<String>,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub(super) input_tokens: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub(super) cache_read_tokens: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub(super) cache_write_tokens: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub(super) output_tokens: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub(super) thinking_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) native_cost_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) decode_speed: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) ttft_secs: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) id_11: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) id_12: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) id_7: Option<String>,
}

impl RawTurn {
    fn has_billable_usage(&self) -> bool {
        self.input_tokens > 0
            || self.output_tokens > 0
            || self.cache_read_tokens > 0
            || self.cache_write_tokens > 0
            || self.native_cost_usd.is_some()
    }

    fn has_signal(&self) -> bool {
        self.decode_speed.is_some() || self.ttft_secs.is_some() || self.has_billable_usage()
    }

    fn has_any_identity(&self) -> bool {
        self.id_11.is_some() || self.id_12.is_some() || self.id_7.is_some()
    }

    fn identity_keys(&self) -> [(u8, Option<&str>); 3] {
        [
            (11, self.id_11.as_deref()),
            (12, self.id_12.as_deref()),
            (7, self.id_7.as_deref()),
        ]
    }
}

struct UnionFind {
    parent: Vec<usize>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
        }
    }

    fn find(&mut self, mut x: usize) -> usize {
        while self.parent[x] != x {
            let p = self.parent[x];
            self.parent[x] = self.parent[p];
            x = p;
        }
        x
    }

    fn union(&mut self, a: usize, b: usize) {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra != rb {
            // Attach larger root to smaller root so component order stays deterministic.
            if ra < rb {
                self.parent[rb] = ra;
            } else {
                self.parent[ra] = rb;
            }
        }
    }
}

fn display_name_rank(disp: Option<&str>) -> u8 {
    match disp.map(str::trim).filter(|s| !s.is_empty()) {
        None => 0,
        Some(s) if PricingCatalog::has_specific_model_qualifier(None, Some(s)) => 2,
        Some(_) => 1,
    }
}

fn response_model_rank(resp: Option<&str>) -> u8 {
    match resp.map(str::trim).filter(|s| !s.is_empty()) {
        None => 0,
        Some(s) if PricingCatalog::has_specific_model_qualifier(Some(s), None) => 2,
        Some(_) => 1,
    }
}

fn valid_decode_speed(speed: Option<f64>) -> Option<f64> {
    speed.filter(|s| s.is_finite() && *s > 0.0 && *s < MAX_TURN_SPEED_TPS)
}

fn valid_ttft_secs(ttft: Option<f64>) -> Option<f64> {
    ttft.filter(|t| t.is_finite() && *t > 0.0 && *t < MAX_TTFT_SECS)
}

fn valid_native_cost(cost: Option<f64>) -> Option<f64> {
    cost.filter(|c| c.is_finite() && *c > 0.0)
}

fn merge_raw_turn(dst: &mut RawTurn, other: RawTurn) {
    dst.input_tokens = dst.input_tokens.max(other.input_tokens);
    dst.cache_read_tokens = dst.cache_read_tokens.max(other.cache_read_tokens);
    dst.cache_write_tokens = dst.cache_write_tokens.max(other.cache_write_tokens);
    dst.output_tokens = dst.output_tokens.max(other.output_tokens);
    dst.thinking_tokens = dst
        .output_tokens
        .min(dst.thinking_tokens.max(other.thinking_tokens));

    if response_model_rank(other.response_model.as_deref())
        > response_model_rank(dst.response_model.as_deref())
    {
        dst.response_model = other.response_model;
    }
    if display_name_rank(other.model_display_name.as_deref())
        > display_name_rank(dst.model_display_name.as_deref())
    {
        dst.model_display_name = other.model_display_name;
    }
    if dst.model_enum == 0 && other.model_enum != 0 {
        dst.model_enum = other.model_enum;
    }

    match (dst.ts_secs, other.ts_secs) {
        (None, Some(ts)) => {
            dst.ts_secs = Some(ts);
            dst.ts_quality = other.ts_quality;
        }
        (Some(dst_ts), Some(other_ts))
            if other.ts_quality > dst.ts_quality
                || (other.ts_quality == dst.ts_quality && other_ts < dst_ts) =>
        {
            dst.ts_secs = Some(other_ts);
            dst.ts_quality = other.ts_quality;
        }
        _ => {}
    }

    dst.native_cost_usd = match (
        valid_native_cost(dst.native_cost_usd),
        valid_native_cost(other.native_cost_usd),
    ) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    };
    dst.decode_speed =
        valid_decode_speed(dst.decode_speed).or_else(|| valid_decode_speed(other.decode_speed));
    dst.ttft_secs = valid_ttft_secs(dst.ttft_secs).or_else(|| valid_ttft_secs(other.ttft_secs));

    if dst.id_11.is_none() {
        dst.id_11 = other.id_11;
    }
    if dst.id_12.is_none() {
        dst.id_12 = other.id_12;
    }
    if dst.id_7.is_none() {
        dst.id_7 = other.id_7;
    }
}

fn build_turn_union_find<'a>(
    turns: impl IntoIterator<Item = &'a RawTurn>,
    len: usize,
) -> UnionFind {
    let mut uf = UnionFind::new(len);
    let mut key_owner: HashMap<(u8, &'a str), usize> = HashMap::new();
    for (idx, turn) in turns.into_iter().enumerate() {
        for (tag, val_opt) in turn.identity_keys() {
            let Some(val) = val_opt.map(str::trim).filter(|s| !s.is_empty()) else {
                continue;
            };
            if let Some(&prev) = key_owner.get(&(tag, val)) {
                uf.union(prev, idx);
            } else {
                key_owner.insert((tag, val), idx);
            }
        }
    }
    uf
}

/// Deduplicates items sharing turn identities via Union-Find.
/// When `should_replace(existing, incoming)` is true, `incoming` becomes the primary entry
/// and the prior primary is merged into it; otherwise `incoming` is merged into `existing`.
fn deduplicate_turns_by<T>(
    items: Vec<T>,
    turn_ref: impl Fn(&T) -> &RawTurn,
    should_replace: impl Fn(&T, &T) -> bool,
    mut merge_into: impl FnMut(&mut T, T),
) -> Vec<T> {
    if items.len() <= 1 {
        return items;
    }
    let mut uf = build_turn_union_find(items.iter().map(&turn_ref), items.len());
    let mut root_to_slot: HashMap<usize, usize> = HashMap::new();
    let mut deduped: Vec<T> = Vec::with_capacity(items.len());
    for (idx, item) in items.into_iter().enumerate() {
        let root = uf.find(idx);
        if let Some(&slot) = root_to_slot.get(&root) {
            let existing = &mut deduped[slot];
            if should_replace(existing, &item) {
                let prev = std::mem::replace(existing, item);
                merge_into(existing, prev);
            } else {
                merge_into(existing, item);
            }
        } else {
            root_to_slot.insert(root, deduped.len());
            deduped.push(item);
        }
    }
    deduped
}

impl RawTurn {
    fn from_attempt(
        attempt: UsageAttemptRecord,
        ts: (Option<i64>, u8),
        outer_model_enum: u64,
        outer_response_model: Option<&str>,
        outer_display_name: Option<&str>,
        inherit_when_outer_zero: bool,
    ) -> Self {
        // When a retry attempt ran on a different model_enum than the outer record,
        // omit outer strings so the scan-time model_enum learning table resolves it.
        let inherits_outer = attempt.model_enum == 0
            || attempt.model_enum == outer_model_enum
            || (inherit_when_outer_zero && outer_model_enum == 0);
        let (response_model, model_display_name) = if inherits_outer {
            (
                outer_response_model.map(str::to_string),
                outer_display_name.map(str::to_string),
            )
        } else {
            (None, None)
        };
        let model_enum = if attempt.model_enum != 0 {
            attempt.model_enum
        } else {
            outer_model_enum
        };
        Self {
            ts_secs: ts.0,
            ts_quality: ts.1,
            model_enum,
            response_model,
            model_display_name,
            input_tokens: attempt.input_tokens,
            cache_read_tokens: attempt.cache_read_tokens,
            cache_write_tokens: attempt.cache_write_tokens,
            output_tokens: attempt.output_tokens,
            thinking_tokens: attempt.thinking_output_tokens,
            native_cost_usd: None,
            decode_speed: None,
            ttft_secs: None,
            id_11: attempt.identity.id_11,
            id_12: attempt.identity.id_12,
            id_7: attempt.identity.id_7,
        }
    }
}

fn discover_sqlite_db_targets(roots: &[(&'static str, PathBuf)]) -> Vec<DbTarget> {
    let mut seen_dbs = HashSet::new();
    let mut seen_ids = HashSet::new();
    let mut targets = Vec::new();

    for (source, root) in roots {
        let conv_dir = root.join("conversations");
        if !is_real_dir(&conv_dir) {
            continue;
        }
        let Ok(entries) = fs::read_dir(&conv_dir) else {
            continue;
        };
        let mut sorted: Vec<_> = entries.filter_map(Result::ok).collect();
        sorted.sort_by_key(|e| e.file_name());
        for entry in sorted {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            let Some(stem) = name_str.strip_suffix(".db") else {
                continue;
            };
            if !is_valid_uuid(stem) {
                continue;
            }
            let path = entry.path();
            let Some(meta) = safe_sqlite_metadata(&path) else {
                continue;
            };
            let real = canonical_or_self(&path);
            let conversation_id = stem.to_ascii_lowercase();
            if !seen_dbs.insert(real) || !seen_ids.insert(conversation_id.clone()) {
                continue;
            }
            let db_mtime_secs = Some(modified_timestamp(&meta).0).filter(|&secs| secs > 0);
            targets.push(DbTarget {
                source,
                root: root.clone(),
                conversation_id,
                path,
                db_mtime_secs,
                db_size_bytes: meta.len(),
            });
        }
    }

    targets
}

fn fallback_timestamp(trajectory_ts: Option<i64>, db_mtime_secs: Option<i64>) -> (Option<i64>, u8) {
    if let Some(ts) = trajectory_ts {
        (Some(ts), TS_QUALITY_TRAJECTORY)
    } else if let Some(ts) = db_mtime_secs {
        (Some(ts), TS_QUALITY_DB_MTIME)
    } else {
        (None, TS_QUALITY_NONE)
    }
}

fn steps_table_columns(conn: &rusqlite::Connection) -> (bool, bool, bool) {
    let mut has_table = false;
    let mut has_metadata = false;
    let mut has_step_type = false;
    if let Ok(mut pragma) = conn.prepare("PRAGMA table_info(steps)") {
        if let Ok(mut rows) = pragma.query([]) {
            while let Ok(Some(row)) = rows.next() {
                has_table = true;
                if let Ok(name) = row.get::<_, String>(1) {
                    if name.eq_ignore_ascii_case("metadata") {
                        has_metadata = true;
                    } else if name.eq_ignore_ascii_case("step_type") {
                        has_step_type = true;
                    }
                }
            }
        }
    }
    (has_table, has_metadata, has_step_type)
}

/// Extracts and intra-DB deduplicates all turns plus retry/error/compaction diagnostics from one
/// SQLite conversation DB across `gen_metadata` (main + `f17` retries), `steps` (`step_type`,
/// `metadata` `f1`/`f8` timestamps, `f9` usage, `f20` origin cascade ID, `f24` model info, `f28`
/// retries), and `trajectory_metadata_blob` (`f2` fallback timestamp).
///
/// When `expected_conversation_id` is `Some`, rows whose decoded `origin_cascade_id` is non-empty
/// and differs (ASCII case-insensitively) are skipped to prevent double-counting forked or
/// side-question cloned turns.
///
/// Returns `None` only when neither `gen_metadata` nor `steps` exists in the database.
pub(super) fn extract_sqlite_db_data(
    conn: &rusqlite::Connection,
    db_mtime_secs: Option<i64>,
    expected_conversation_id: Option<&str>,
) -> Option<ScannedDbData> {
    let trajectory_ts = conn
        .prepare("SELECT data FROM trajectory_metadata_blob WHERE data IS NOT NULL LIMIT 1")
        .ok()
        .and_then(|mut stmt| stmt.query_row([], |row| row.get::<_, Vec<u8>>(0)).ok())
        .and_then(|blob| extract_trajectory_created_at_secs(&blob));

    let mut diagnostics = ScanDiagnostics::default();
    let (steps_exists, steps_has_metadata, steps_has_step_type) = steps_table_columns(conn);
    let mut has_steps_table = false;
    let mut step_timestamps: HashMap<u64, (i64, u8)> = HashMap::new();
    let mut step_candidates: Vec<(u64, RawTurn)> = Vec::new();
    let mut step_retry_candidates: Vec<(u64, Vec<RetryAttemptRecord>)> = Vec::new();
    if steps_exists && (steps_has_metadata || steps_has_step_type) {
        let step_sql = match (steps_has_step_type, steps_has_metadata) {
            (true, true) => {
                "SELECT idx, step_type, metadata FROM steps WHERE metadata IS NOT NULL OR step_type IN (17, 23) ORDER BY idx ASC"
            }
            (false, true) => {
                "SELECT idx, NULL, metadata FROM steps WHERE metadata IS NOT NULL ORDER BY idx ASC"
            }
            (true, false) => {
                "SELECT idx, step_type, NULL FROM steps WHERE step_type IN (17, 23) ORDER BY idx ASC"
            }
            (false, false) => unreachable!(),
        };
        if let Ok(mut step_stmt) = conn.prepare(step_sql) {
            if let Ok(mut step_rows) = step_stmt.query([]) {
                has_steps_table = true;
                while let Ok(Some(row)) = step_rows.next() {
                    let Ok(idx) = row.get::<_, i64>(0) else {
                        continue;
                    };
                    if idx < 0 {
                        continue;
                    }
                    let step_idx = idx as u64;
                    let step_type = row.get::<_, Option<i64>>(1).ok().flatten();
                    let meta_blob = row.get::<_, Option<Vec<u8>>>(2).ok().flatten();
                    let step_rec = meta_blob
                        .as_deref()
                        .filter(|b| !b.is_empty())
                        .and_then(extract_step_metadata_record);

                    if let Some(ref rec) = step_rec {
                        if is_foreign_cascade_id(
                            rec.origin_cascade_id.as_deref(),
                            expected_conversation_id,
                        ) {
                            continue;
                        }
                    }

                    match step_type {
                        Some(STEP_TYPE_ERROR_MESSAGE) => {
                            diagnostics.error_steps = diagnostics.error_steps.saturating_add(1);
                        }
                        Some(STEP_TYPE_CHECKPOINT) => {
                            diagnostics.compaction_steps =
                                diagnostics.compaction_steps.saturating_add(1);
                        }
                        _ => {}
                    }

                    let Some(step_rec) = step_rec else {
                        continue;
                    };

                    if !step_rec.retry_attempts.is_empty() {
                        step_retry_candidates.push((step_idx, step_rec.retry_attempts));
                    }

                    let step_ts = if let Some(ts) = step_rec.created_at_secs {
                        step_timestamps.insert(step_idx, (ts, TS_QUALITY_STEP_PRIMARY));
                        (Some(ts), TS_QUALITY_STEP_PRIMARY)
                    } else if let Some(ts) = step_rec.secondary_created_at_secs {
                        step_timestamps.insert(step_idx, (ts, TS_QUALITY_SECONDARY));
                        (Some(ts), TS_QUALITY_SECONDARY)
                    } else {
                        fallback_timestamp(trajectory_ts, db_mtime_secs)
                    };

                    if let Some(usage) = step_rec.usage {
                        let turn = RawTurn::from_attempt(
                            usage,
                            step_ts,
                            step_rec.model_enum,
                            step_rec.response_model.as_deref(),
                            step_rec.model_display_name.as_deref(),
                            true,
                        );
                        if turn.has_signal() {
                            step_candidates.push((step_idx, turn));
                        }
                    }

                    for retry in step_rec.retry_usages {
                        let turn = RawTurn::from_attempt(
                            retry,
                            step_ts,
                            step_rec.model_enum,
                            step_rec.response_model.as_deref(),
                            step_rec.model_display_name.as_deref(),
                            false,
                        );
                        if turn.has_signal() {
                            step_candidates.push((step_idx, turn));
                        }
                    }
                }
            }
        }
    }

    let mut has_gen_table = false;
    let mut covered_step_indices: HashSet<u64> = HashSet::new();
    let mut gen_retry_covered_steps: HashSet<u64> = HashSet::new();
    let mut turns = Vec::new();
    if let Ok(mut stmt) =
        conn.prepare("SELECT data FROM gen_metadata WHERE data IS NOT NULL ORDER BY idx ASC")
    {
        if let Ok(mut rows) = stmt.query([]) {
            has_gen_table = true;
            while let Ok(Some(row)) = rows.next() {
                let Ok(blob) = row.get::<_, Vec<u8>>(0) else {
                    continue;
                };
                let Some(rec) = extract_gen_metadata_record(&blob) else {
                    continue;
                };
                if is_foreign_cascade_id(rec.origin_cascade_id.as_deref(), expected_conversation_id)
                {
                    continue;
                }
                let mut recorded_gen_retry = false;
                for attempt in &rec.retry_attempts {
                    if attempt.is_anomaly() {
                        recorded_gen_retry = true;
                        diagnostics.record_retry(attempt);
                    }
                }
                if recorded_gen_retry {
                    gen_retry_covered_steps.extend(rec.step_indices.iter().copied());
                }
                covered_step_indices.extend(rec.step_indices.iter().copied());

                let (ts_secs, ts_quality) = rec
                    .step_indices
                    .iter()
                    .find_map(|idx| step_timestamps.get(idx).copied())
                    .map(|(ts, q)| (Some(ts), q))
                    .or_else(|| {
                        rec.chat_start_created_at_secs
                            .map(|ts| (Some(ts), TS_QUALITY_SECONDARY))
                    })
                    .unwrap_or_else(|| fallback_timestamp(trajectory_ts, db_mtime_secs));

                let decode_speed = rec.decode_speed_tps();
                let main_turn = RawTurn {
                    ts_secs,
                    ts_quality,
                    model_enum: rec.model_enum,
                    response_model: rec.response_model.clone(),
                    model_display_name: rec.model_display_name.clone(),
                    input_tokens: rec.input_tokens,
                    cache_read_tokens: rec.cache_read_tokens,
                    cache_write_tokens: rec.cache_write_tokens,
                    output_tokens: rec.output_tokens,
                    thinking_tokens: rec.thinking_output_tokens,
                    native_cost_usd: rec.model_cost_usd,
                    decode_speed,
                    ttft_secs: rec.ttft_secs,
                    id_11: rec.identity.id_11,
                    id_12: rec.identity.id_12,
                    id_7: rec.identity.id_7,
                };
                if main_turn.has_signal() {
                    turns.push(main_turn);
                }

                for retry in rec.retry_usages {
                    let retry_turn = RawTurn::from_attempt(
                        retry,
                        (ts_secs, ts_quality),
                        rec.model_enum,
                        rec.response_model.as_deref(),
                        rec.model_display_name.as_deref(),
                        false,
                    );
                    if retry_turn.has_signal() {
                        turns.push(retry_turn);
                    }
                }
            }
        }
    }

    if !has_gen_table && !has_steps_table {
        return None;
    }

    // Avoid double-counting retries mirrored onto both `gen_metadata.chat(17)` and
    // `steps.metadata(28)` for the same step while retaining uncovered/step-only retries.
    for (step_idx, attempts) in step_retry_candidates {
        if gen_retry_covered_steps.contains(&step_idx) {
            continue;
        }
        for attempt in &attempts {
            diagnostics.record_retry(attempt);
        }
    }

    for (step_idx, turn) in step_candidates {
        if !turn.has_any_identity() && covered_step_indices.contains(&step_idx) {
            continue;
        }
        turns.push(turn);
    }

    Some(ScannedDbData {
        turns: deduplicate_turns_by(turns, |t| t, |_, _| false, merge_raw_turn),
        diagnostics,
    })
}

pub(super) fn extract_sqlite_db_turns(
    conn: &rusqlite::Connection,
    db_mtime_secs: Option<i64>,
    expected_conversation_id: Option<&str>,
) -> Option<Vec<RawTurn>> {
    extract_sqlite_db_data(conn, db_mtime_secs, expected_conversation_id).map(|data| data.turns)
}

fn scan_single_sqlite_db_raw(target: &DbTarget) -> Option<ScannedDbData> {
    let conn = open_readonly_db(&target.path).ok()?;
    extract_sqlite_db_data(&conn, target.db_mtime_secs, Some(&target.conversation_id))
}

fn scan_missed_targets_parallel(
    misses: &[(usize, String, Option<DbFileFingerprint>, &DbTarget)],
) -> Vec<(
    usize,
    String,
    Option<DbFileFingerprint>,
    Option<ScannedDbData>,
)> {
    let workers = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .clamp(1, 8);
    if misses.len() <= 8 || workers == 1 {
        misses
            .iter()
            .map(|(slot, key, fp, target)| {
                (*slot, key.clone(), *fp, scan_single_sqlite_db_raw(target))
            })
            .collect()
    } else {
        let chunk_size = misses.len().div_ceil(workers);
        std::thread::scope(|scope| {
            let handles: Vec<_> = misses
                .chunks(chunk_size)
                .map(|chunk| {
                    scope.spawn(move || {
                        chunk
                            .iter()
                            .map(|(slot, key, fp, target)| {
                                (*slot, key.clone(), *fp, scan_single_sqlite_db_raw(target))
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            let mut merged = Vec::with_capacity(misses.len());
            for h in handles {
                if let Ok(part) = h.join() {
                    merged.extend(part);
                }
            }
            merged
        })
    }
}

fn scan_sqlite_targets_with_cache(
    targets: &[DbTarget],
    cache_path: Option<&Path>,
) -> (Vec<(&'static str, ScannedDbData)>, usize, usize) {
    let mut cache = cache_path.map(global_cache::GlobalStatsCache::load);
    let mut slots: Vec<Option<(&'static str, ScannedDbData)>> = vec![None; targets.len()];
    let mut live_keys: HashSet<String> = HashSet::with_capacity(targets.len());
    let mut misses: Vec<(usize, String, Option<DbFileFingerprint>, &DbTarget)> = Vec::new();
    let mut cache_hits = 0usize;

    for (idx, target) in targets.iter().enumerate() {
        let key = canonical_or_self(&target.path)
            .to_string_lossy()
            .into_owned();
        live_keys.insert(key.clone());
        // Stat before opening so concurrent SQLite commits invalidate on the next scan.
        let fp = db_file_fingerprint(&target.path);
        if let (Some(c), Some(fp_val)) = (cache.as_ref(), fp) {
            if let Some(hit) = c.get_matching(&key, fp_val) {
                slots[idx] = Some((target.source, hit.clone()));
                cache_hits += 1;
                continue;
            }
        }
        misses.push((idx, key, fp, target));
    }

    let cache_misses = misses.len();
    let scanned_misses = scan_missed_targets_parallel(&misses);
    for (slot_idx, key, fp, data_opt) in scanned_misses {
        if let Some(data) = data_opt {
            if let (Some(c), Some(fp_val)) = (cache.as_mut(), fp) {
                c.upsert(key, fp_val, data.clone());
            }
            slots[slot_idx] = Some((targets[slot_idx].source, data));
        }
    }

    if let (Some(mut c), Some(path)) = (cache, cache_path) {
        c.prune_missing(&live_keys);
        c.save_if_dirty(path);
    }

    (
        slots.into_iter().flatten().collect(),
        cache_hits,
        cache_misses,
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ModelEnumIdentity {
    CatalogEntry(usize),
    NormalizedName(String),
}

enum LearnedModelEnum<'b> {
    Generic {
        response_model: Option<&'b str>,
        model_display_name: Option<&'b str>,
        quality: (u8, u8),
    },
    Concrete {
        identity: ModelEnumIdentity,
        response_model: Option<&'b str>,
        model_display_name: Option<&'b str>,
        quality: (u8, u8),
    },
    Poisoned,
}

fn normalized_fallback_model_identity(group: &str) -> String {
    let norm = normalize_model_key(group);
    let mut words: Vec<&str> = norm.split_whitespace().collect();
    while words.len() > 1 && matches!(words.last().copied(), Some("trial" | "preview" | "latest")) {
        words.pop();
    }
    words.join(" ")
}

struct ResolvedTurnPricing {
    model_group: String,
    pricing_key: Option<String>,
    cost_usd: Option<f64>,
}

type ModelLookupKey<'b> = (Option<&'b str>, Option<&'b str>, u64);
type ResolvedModelInfo = (String, Option<usize>, bool);

/// Shared scan-time model-enum learner, memoized catalog resolver, and dynamic
/// `response_model` pricing inferrer used by both `stats` projection and statusline cost.
struct TurnPricer<'a, 'b> {
    pricing: &'a PricingCatalog,
    learned_enums: HashMap<u64, (Option<&'b str>, Option<&'b str>)>,
    resolution_cache: HashMap<ModelLookupKey<'b>, ResolvedModelInfo>,
    inferred_response_models: HashMap<&'b str, Option<usize>>,
}

impl<'a, 'b> TurnPricer<'a, 'b> {
    fn resolve_uncached(
        pricing: &PricingCatalog,
        resp: Option<&str>,
        disp: Option<&str>,
        model_enum: u64,
    ) -> ResolvedModelInfo {
        let group = derive_model_name(disp, resp, model_enum);
        let active: Vec<&str> = [resp, disp, Some(group.as_str())]
            .into_iter()
            .flatten()
            .collect();
        let entry_idx = pricing.resolve_entry_idx(&active);
        let specific = PricingCatalog::has_specific_model_qualifier(resp, disp);
        (group, entry_idx, specific)
    }

    fn build(pricing: &'a PricingCatalog, turns: impl IntoIterator<Item = &'b RawTurn>) -> Self {
        let mut resolution_cache: HashMap<ModelLookupKey<'b>, ResolvedModelInfo> = HashMap::new();
        let mut enum_states: HashMap<u64, LearnedModelEnum<'b>> = HashMap::new();

        for turn in turns {
            let resp = turn
                .response_model
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let disp = turn
                .model_display_name
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty());

            let (group, entry_idx, _) = resolution_cache
                .entry((resp, disp, turn.model_enum))
                .or_insert_with(|| Self::resolve_uncached(pricing, resp, disp, turn.model_enum));

            if turn.model_enum == 0 || (resp.is_none() && disp.is_none()) {
                continue;
            }

            let disp_rank = display_name_rank(disp);
            let resp_rank = response_model_rank(resp);
            let quality = (disp_rank, resp_rank);
            let is_concrete = entry_idx.is_some()
                || disp_rank == 2
                || PricingCatalog::has_specific_model_qualifier(None, resp);

            let new_identity = if is_concrete {
                Some(match *entry_idx {
                    Some(idx) => ModelEnumIdentity::CatalogEntry(idx),
                    None => {
                        ModelEnumIdentity::NormalizedName(normalized_fallback_model_identity(group))
                    }
                })
            } else {
                None
            };

            match enum_states.get_mut(&turn.model_enum) {
                None => {
                    let state = match new_identity {
                        Some(identity) => LearnedModelEnum::Concrete {
                            identity,
                            response_model: resp,
                            model_display_name: disp,
                            quality,
                        },
                        None => LearnedModelEnum::Generic {
                            response_model: resp,
                            model_display_name: disp,
                            quality,
                        },
                    };
                    enum_states.insert(turn.model_enum, state);
                }
                Some(LearnedModelEnum::Poisoned) => {}
                Some(slot @ LearnedModelEnum::Generic { .. }) => {
                    if let Some(identity) = new_identity {
                        *slot = LearnedModelEnum::Concrete {
                            identity,
                            response_model: resp,
                            model_display_name: disp,
                            quality,
                        };
                    } else if let LearnedModelEnum::Generic {
                        response_model,
                        model_display_name,
                        quality: prev_q,
                    } = slot
                    {
                        if quality > *prev_q {
                            *response_model = resp;
                            *model_display_name = disp;
                            *prev_q = quality;
                        }
                    }
                }
                Some(slot @ LearnedModelEnum::Concrete { .. }) => {
                    // Generic-only observations (e.g. legacy "Gemini Flash") never poison a concrete enum.
                    let Some(identity) = new_identity else {
                        continue;
                    };
                    let same_identity = matches!(
                        slot,
                        LearnedModelEnum::Concrete {
                            identity: prev_id,
                            ..
                        } if *prev_id == identity
                    );
                    if !same_identity {
                        *slot = LearnedModelEnum::Poisoned;
                    } else if let LearnedModelEnum::Concrete {
                        response_model,
                        model_display_name,
                        quality: prev_q,
                        ..
                    } = slot
                    {
                        if quality > *prev_q {
                            if resp.is_some() {
                                *response_model = resp;
                            }
                            if disp_rank >= display_name_rank(*model_display_name) && disp.is_some()
                            {
                                *model_display_name = disp;
                            }
                            *prev_q = (
                                display_name_rank(*model_display_name),
                                response_model_rank(*response_model),
                            );
                        }
                    }
                }
            }
        }

        let mut learned_enums = HashMap::new();
        for (model_enum, state) in enum_states {
            match state {
                LearnedModelEnum::Concrete {
                    response_model,
                    model_display_name,
                    ..
                }
                | LearnedModelEnum::Generic {
                    response_model,
                    model_display_name,
                    ..
                } => {
                    learned_enums.insert(model_enum, (response_model, model_display_name));
                    resolution_cache
                        .entry((response_model, model_display_name, model_enum))
                        .or_insert_with(|| {
                            Self::resolve_uncached(
                                pricing,
                                response_model,
                                model_display_name,
                                model_enum,
                            )
                        });
                }
                LearnedModelEnum::Poisoned => {}
            }
        }

        // Round 1 dynamic inference: map response_models that uniquely resolved to one
        // catalog entry elsewhere in the dataset, poisoning ambiguous or explicitly-qualified
        // unpriced variants.
        let mut inferred_response_models: HashMap<&'b str, Option<usize>> = HashMap::new();
        for ((resp_opt, _, _), (_, entry_idx, has_specific_qualifier)) in &resolution_cache {
            let Some(resp) = *resp_opt else {
                continue;
            };
            match (*entry_idx, *has_specific_qualifier) {
                (Some(idx), _) => {
                    inferred_response_models
                        .entry(resp)
                        .and_modify(|slot| {
                            if *slot != Some(idx) {
                                *slot = None;
                            }
                        })
                        .or_insert(Some(idx));
                }
                (None, true) => {
                    inferred_response_models.insert(resp, None);
                }
                (None, false) => {}
            }
        }

        Self {
            pricing,
            learned_enums,
            resolution_cache,
            inferred_response_models,
        }
    }

    fn price_turn(&self, turn: &RawTurn) -> ResolvedTurnPricing {
        let raw_resp = turn
            .response_model
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let raw_disp = turn
            .model_display_name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());

        let (eff_resp, eff_disp) =
            if raw_resp.is_none() && raw_disp.is_none() && turn.model_enum > 0 {
                self.learned_enums
                    .get(&turn.model_enum)
                    .copied()
                    .unwrap_or((None, None))
            } else {
                (raw_resp, raw_disp)
            };

        let cached = self
            .resolution_cache
            .get(&(eff_resp, eff_disp, turn.model_enum));
        let owned_fallback;
        let (model_group, direct_entry_idx, has_specific_qualifier) = match cached {
            Some(entry) => (&entry.0, entry.1, entry.2),
            None => {
                owned_fallback =
                    Self::resolve_uncached(self.pricing, eff_resp, eff_disp, turn.model_enum);
                (&owned_fallback.0, owned_fallback.1, owned_fallback.2)
            }
        };

        let (pricing_key, cost_usd) = if let Some(usd) = valid_native_cost(turn.native_cost_usd) {
            (Some("native".to_string()), Some(round_usd_6(usd)))
        } else {
            let resolved_idx = direct_entry_idx.or_else(|| {
                (!has_specific_qualifier)
                    .then(|| {
                        eff_resp
                            .and_then(|resp| self.inferred_response_models.get(resp).copied())
                            .flatten()
                    })
                    .flatten()
            });
            resolved_idx
                .and_then(|idx| {
                    self.pricing.estimate_cost_for_entry(
                        idx,
                        turn.input_tokens,
                        turn.cache_read_tokens,
                        turn.cache_write_tokens,
                        turn.output_tokens,
                    )
                })
                .unzip()
        };

        ResolvedTurnPricing {
            model_group: model_group.clone(),
            pricing_key,
            cost_usd,
        }
    }
}

struct SourcedTurn {
    source: UsageSource,
    turn: RawTurn,
}

fn project_sqlite_conversations<'a>(
    conversations: impl IntoIterator<Item = (&'static str, &'a [RawTurn])>,
    tz: &TimeZoneSpec,
    pricing: &PricingCatalog,
) -> (SqliteStats, Vec<UsageEvent>) {
    let mut stats = SqliteStats::default();
    let mut raw_items: Vec<SourcedTurn> = Vec::new();

    for (source, turns) in conversations {
        stats.db_files += 1;
        let usage_source = UsageSource::from_root_label(source);
        for turn in turns {
            raw_items.push(SourcedTurn {
                source: usage_source,
                turn: turn.clone(),
            });
        }
    }

    let merged_turns = deduplicate_turns_by(
        raw_items,
        |i| &i.turn,
        // Enforce deterministic UsageSource priority (Cli > Desktop).
        |existing, incoming| {
            existing.source == UsageSource::Desktop && incoming.source == UsageSource::Cli
        },
        |dst, other| merge_raw_turn(&mut dst.turn, other.turn),
    );
    let pricer = TurnPricer::build(pricing, merged_turns.iter().map(|m| &m.turn));

    let mut speed_samples = Vec::new();
    let mut ttft_samples = Vec::new();
    let mut events = Vec::new();

    for entry in &merged_turns {
        let turn = &entry.turn;
        if let Some(speed) = valid_decode_speed(turn.decode_speed) {
            stats.completed_turns += 1;
            stats.total_output_tokens =
                stats.total_output_tokens.saturating_add(turn.output_tokens);
            speed_samples.push(speed);
        }
        if let Some(ttft) = valid_ttft_secs(turn.ttft_secs) {
            ttft_samples.push(ttft);
        }
        if !turn.has_billable_usage() {
            continue;
        }
        let Some(ts_secs) = turn.ts_secs else {
            continue;
        };

        let total_tokens = turn
            .input_tokens
            .saturating_add(turn.cache_read_tokens)
            .saturating_add(turn.cache_write_tokens)
            .saturating_add(turn.output_tokens);
        let local_day = tz.local_day(ts_secs);
        let resolved = pricer.price_turn(turn);

        events.push(UsageEvent {
            source: entry.source,
            local_day,
            model_group: resolved.model_group,
            pricing_key: resolved.pricing_key,
            input_tokens: turn.input_tokens,
            cache_read_tokens: turn.cache_read_tokens,
            cache_write_tokens: turn.cache_write_tokens,
            output_tokens: turn.output_tokens,
            thinking_tokens: turn.thinking_tokens,
            total_tokens,
            cost_usd: resolved.cost_usd,
            decode_speed: valid_decode_speed(turn.decode_speed),
            ttft_secs: valid_ttft_secs(turn.ttft_secs),
        });
    }

    stats.ttft_samples = ttft_samples.len();
    stats.median_tok_per_sec = compute_median(&speed_samples);
    stats.median_ttft_secs = compute_median(&ttft_samples);
    (stats, events)
}

/// Scans all SQLite conversation DBs across `roots` using an optional explicit per-DB cache path,
/// returning projected usage events, SQLite speed/TTFT stats, retry/error/compaction diagnostics,
/// and per-DB cache hit/miss counts.
pub(crate) fn scan_sqlite_conversations_with_cache(
    roots: &[(&'static str, PathBuf)],
    tz: &TimeZoneSpec,
    pricing: &PricingCatalog,
    cache_path: Option<&Path>,
) -> GlobalScanResult {
    let targets = discover_sqlite_db_targets(roots);
    let (scanned, cache_hits, cache_misses) = scan_sqlite_targets_with_cache(&targets, cache_path);
    let mut diagnostics = ScanDiagnostics::default();
    for (_, data) in &scanned {
        diagnostics.merge(&data.diagnostics);
    }
    let (sqlite, events) = project_sqlite_conversations(
        scanned
            .iter()
            .map(|(source, data)| (*source, data.turns.as_slice())),
        tz,
        pricing,
    );
    GlobalScanResult {
        sqlite,
        events,
        diagnostics,
        cache_hits,
        cache_misses,
    }
}

fn resolve_descendant_conversation_ids(cli_dir: &Path, root_id: &str) -> Vec<String> {
    if !is_valid_uuid(root_id) {
        return Vec::new();
    }
    let summaries_path = cli_dir.join("conversation_summaries.db");
    if !is_safe_sqlite_file(&summaries_path) {
        return Vec::new();
    }
    let Ok(conn) = open_readonly_db(&summaries_path) else {
        return Vec::new();
    };
    let Ok(mut stmt) = conn.prepare(
        "WITH RECURSIVE descendants(conversation_id, depth) AS (
             SELECT conversation_id, 1
             FROM conversation_summaries
             WHERE parent_conversation_id = ?1 COLLATE NOCASE
             UNION
             SELECT cs.conversation_id, d.depth + 1
             FROM conversation_summaries cs
             JOIN descendants d ON cs.parent_conversation_id = d.conversation_id COLLATE NOCASE
             WHERE d.depth < 8
         )
         SELECT DISTINCT conversation_id FROM descendants",
    ) else {
        return Vec::new();
    };
    let Ok(mut rows) = stmt.query([root_id]) else {
        return Vec::new();
    };

    let mut seen = HashSet::new();
    let mut ids = Vec::new();
    while let Ok(Some(row)) = rows.next() {
        if let Ok(id) = row.get::<_, String>(0) {
            if is_valid_uuid(&id)
                && !id.eq_ignore_ascii_case(root_id)
                && seen.insert(id.to_ascii_lowercase())
            {
                ids.push(id);
            }
        }
    }
    ids
}

fn read_single_db_turns(
    db_path: &Path,
    expected_conversation_id: Option<&str>,
) -> Option<Vec<RawTurn>> {
    let conn = open_readonly_db(db_path).ok()?;
    let inferred = expected_conversation_id
        .map(str::to_string)
        .or_else(|| conversation_id_from_db_path(db_path));
    extract_sqlite_db_turns(&conn, None, inferred.as_deref())
}

/// Deduplicates tagged turns across parent + descendant DBs via Union-Find, preferring
/// parent (`is_subagent == false`) attribution when the same turn appears in both.
fn deduplicate_parent_priority_turns<T>(
    items: Vec<T>,
    turn_ref: impl Fn(&T) -> &RawTurn,
    is_sub: impl Fn(&T) -> bool,
    merge_into: impl FnMut(&mut T, T),
) -> Vec<T> {
    deduplicate_turns_by(
        items,
        turn_ref,
        |existing, incoming| is_sub(existing) && !is_sub(incoming),
        merge_into,
    )
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::*;
    use crate::stats::collect_stats_with_tz;
    use crate::stats::metric::StatsMetric;
    use crate::stats::render::render_stats_report_with_metric;
    use crate::stats::test_helpers::build_gen_blob;
    use crate::sys::time::parse_rfc3339_unix_secs;

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
                Some("Gemini 3.5 Flash (Extra Low)"),
                Some("gemini-3.5-flash-extra-low"),
                1527
            ),
            "Gemini 3.5 Flash"
        );
        assert_eq!(
            derive_model_name(
                Some("Gemini 3.5 Flash (xLow)"),
                Some("gemini-3.5-flash-xlow"),
                1528
            ),
            "Gemini 3.5 Flash"
        );
        assert!(is_effort_token("extralow"));
        assert!(is_effort_token("xlow"));
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
    fn scans_empty_home_and_falls_back_to_db_mtime_when_step_missing() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let now_secs = parse_rfc3339_unix_secs("2026-11-18T12:00:00Z").unwrap() as u64;

        let empty = collect_stats_with_tz(home, None, now_secs, &TimeZoneSpec::utc());
        assert_eq!(empty.totals.requests, 0);
        assert_eq!(empty.sqlite.db_files, 0);
        let mut empty_out = Vec::new();
        render_stats_report_with_metric(
            &empty,
            StatsMetric::Total,
            None,
            true,
            true,
            &mut empty_out,
        )
        .unwrap();
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

        let populated = collect_stats_with_tz(home, None, now_secs, &TimeZoneSpec::utc());
        assert_eq!(populated.sqlite.db_files, 1);
        assert_eq!(populated.sqlite.completed_turns, 1);
        assert_eq!(populated.windows.all_time.requests, 1);
        assert_eq!(populated.models[0].model, "tiny-model");
    }

    #[test]
    fn project_sqlite_infers_unambiguous_response_model_without_crossing_ambiguous_or_explicit_tiers(
    ) {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().join("cache");
        fs::create_dir_all(&cache_dir).unwrap();
        fs::write(
            cache_dir.join("models_dev_pricing.json"),
            r#"{
              "fetched_at": 1000,
              "models": {
                "gemini-4-hydrogen": { "input_per_mtok": 1.5, "output_per_mtok": 7.5 },
                "gemini-4-carbon": { "input_per_mtok": 2.0, "output_per_mtok": 10.0 },
                "gemini-4-argon": { "input_per_mtok": 4.0, "output_per_mtok": 20.0 }
              }
            }"#,
        )
        .unwrap();
        let pricing = PricingCatalog::load_effective_with(
            Some(&cache_dir),
            crate::stats::SyncMode::Offline,
            1000,
            86_400,
            |_| unreachable!(),
        );

        let ts = Some(1_700_000_000_i64);
        let mk_turn = |resp: &str, disp: Option<&str>| RawTurn {
            ts_secs: ts,
            ts_quality: TS_QUALITY_STEP_PRIMARY,
            model_enum: 0,
            response_model: Some(resp.to_string()),
            model_display_name: disp.map(str::to_string),
            input_tokens: 1_000_000,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            output_tokens: 0,
            thinking_tokens: 0,
            native_cost_usd: None,
            decode_speed: None,
            ttft_secs: None,
            id_11: None,
            id_12: None,
            id_7: None,
        };

        let turns = vec![
            // Unambiguous worker routing ID: turn 0 has full display name, turn 1 has legacy generic name.
            mk_turn(
                "gemini-flash-worker-agy",
                Some("Gemini 4 Hydrogen (Orchestrator)"),
            ),
            mk_turn("gemini-flash-worker-agy", Some("Gemini Flash")),
            // Ambiguous routing ID observed with two distinct models: bare turn 4 must stay unpriced.
            mk_turn("gemini-pro-orchestrator", Some("Orchestrator (Carbon)")),
            mk_turn("gemini-pro-orchestrator", Some("Orchestrator (Argon)")),
            mk_turn("gemini-pro-orchestrator", None),
            // Routing ID with an explicit unpriced codename: turn 6 and bare turn 7 must stay unpriced.
            mk_turn("custom-orchestrator", Some("Orchestrator (Carbon)")),
            mk_turn("custom-orchestrator", Some("Orchestrator (Xenon)")),
            mk_turn("custom-orchestrator", None),
            // Shared worker routing ID upgraded to a newer unpriced model whose name precedes
            // `(Orchestrator)`: turn 9 and generic turn 10 must stay unpriced (never mispriced as Hydrogen).
            mk_turn("shared-worker-id", Some("Gemini 4 Hydrogen (Orchestrator)")),
            mk_turn("shared-worker-id", Some("Gemini 5 Plasma (Orchestrator)")),
            mk_turn("shared-worker-id", Some("Gemini Flash")),
        ];

        let (_, events) = project_sqlite_conversations(
            [("cli", turns.as_slice())],
            &TimeZoneSpec::utc(),
            &pricing,
        );
        assert_eq!(events.len(), 11);
        assert_eq!(events[0].pricing_key.as_deref(), Some("gemini-4-hydrogen"));
        assert_eq!(events[0].cost_usd, Some(1.5));
        assert_eq!(events[1].pricing_key.as_deref(), Some("gemini-4-hydrogen"));
        assert_eq!(events[1].cost_usd, Some(1.5));

        assert_eq!(events[2].pricing_key.as_deref(), Some("gemini-4-carbon"));
        assert_eq!(events[3].pricing_key.as_deref(), Some("gemini-4-argon"));
        assert_eq!(events[4].pricing_key, None);
        assert_eq!(events[4].cost_usd, None);

        assert_eq!(events[5].pricing_key.as_deref(), Some("gemini-4-carbon"));
        assert_eq!(events[6].pricing_key, None);
        assert_eq!(events[6].cost_usd, None);
        assert_eq!(events[7].pricing_key, None);
        assert_eq!(events[7].cost_usd, None);

        assert_eq!(events[8].pricing_key.as_deref(), Some("gemini-4-hydrogen"));
        assert_eq!(events[9].pricing_key, None);
        assert_eq!(events[9].cost_usd, None);
        assert_eq!(events[10].pricing_key, None);
        assert_eq!(events[10].cost_usd, None);
    }

    pub(super) fn test_pricing_catalog(cache_dir: &Path) -> PricingCatalog {
        fs::create_dir_all(cache_dir).unwrap();
        fs::write(
            cache_dir.join("models_dev_pricing.json"),
            r#"{
              "fetched_at": 1000,
              "models": {
                "gemini-4-hydrogen": { "input_per_mtok": 1.5, "output_per_mtok": 7.5 },
                "gemini-4-carbon": { "input_per_mtok": 2.0, "output_per_mtok": 10.0 },
                "gemini-4-argon": { "input_per_mtok": 4.0, "output_per_mtok": 20.0 }
              }
            }"#,
        )
        .unwrap();
        PricingCatalog::load_effective_with(
            Some(cache_dir),
            crate::stats::SyncMode::Offline,
            1000,
            86_400,
            |_| unreachable!(),
        )
    }

    #[test]
    fn extract_sqlite_db_turns_merges_steps_f9_f28_f17_and_suppresses_keyless_covered_steps() {
        use crate::agy::protobuf::{
            encode_gen_blob, encode_step_blob, encode_trajectory_meta_blob, GenBlobSpec,
            StepBlobSpec, UsageSpec,
        };

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("conv.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE trajectory_metadata_blob (data BLOB);
             CREATE TABLE steps (idx INTEGER PRIMARY KEY, metadata BLOB);
             CREATE TABLE gen_metadata (idx INTEGER PRIMARY KEY, data BLOB);",
        )
        .unwrap();

        conn.execute(
            "INSERT INTO trajectory_metadata_blob (data) VALUES (?1)",
            [encode_trajectory_meta_blob(1_795_000_000)],
        )
        .unwrap();

        // Step 1 is covered by gen_metadata(1): keyless f9 must be suppressed, while f28 retry
        // sharing id_11="r1" merges into gen_metadata(1) and lifts output_tokens to 300.
        conn.execute(
            "INSERT INTO steps (idx, metadata) VALUES (1, ?1)",
            [encode_step_blob(StepBlobSpec {
                created_at_secs: Some(1_795_000_100),
                usage: Some(UsageSpec {
                    model_enum: 1669,
                    input_tok: 99_999,
                    output_tok: 999,
                    ..Default::default()
                }),
                retries: vec![UsageSpec {
                    model_enum: 1669,
                    input_tok: 1_000,
                    output_tok: 300,
                    id_11: Some("r1"),
                    id_7: Some("c1"),
                    ..Default::default()
                }],
                ..Default::default()
            })],
        )
        .unwrap();

        // Step 2 is NOT covered by gen_metadata (e.g. step_type = 23 compaction turn) with f8 ts.
        conn.execute(
            "INSERT INTO steps (idx, metadata) VALUES (2, ?1)",
            [encode_step_blob(StepBlobSpec {
                secondary_created_at_secs: Some(1_795_000_200),
                usage: Some(UsageSpec {
                    model_enum: 1669,
                    input_tok: 4_000,
                    output_tok: 80,
                    ..Default::default()
                }),
                ..Default::default()
            })],
        )
        .unwrap();

        // gen_metadata(1): covers step 1; has main turn id_11="r1", plus f17 retry on different
        // model_enum=1418 (must NOT inherit outer response_model/display_name).
        conn.execute(
            "INSERT INTO gen_metadata (idx, data) VALUES (1, ?1)",
            [encode_gen_blob(GenBlobSpec {
                step_indices: vec![1],
                model_enum: 1669,
                resp_model: Some("gemini-flash-worker-agy"),
                disp_name: Some("Gemini 4 Hydrogen (Orchestrator)"),
                input_tok: 1_200,
                output_tok: 200,
                id_11: Some("r1"),
                dur_secs: 2,
                ttft_nanos: 500_000_000,
                retries: vec![UsageSpec {
                    model_enum: 1418,
                    input_tok: 600,
                    output_tok: 40,
                    id_11: Some("r-retry-other-model"),
                    ..Default::default()
                }],
                ..Default::default()
            })],
        )
        .unwrap();

        // gen_metadata(2): no step timestamp and no chat_start -> falls back to trajectory_metadata_blob.f2.
        conn.execute(
            "INSERT INTO gen_metadata (idx, data) VALUES (2, ?1)",
            [encode_gen_blob(GenBlobSpec {
                step_indices: vec![99],
                model_enum: 1418,
                resp_model: Some("gemini-4-argon"),
                disp_name: Some("Gemini 4 Argon"),
                input_tok: 500,
                output_tok: 50,
                id_11: Some("r-traj-fallback"),
                ..Default::default()
            })],
        )
        .unwrap();

        let turns = extract_sqlite_db_turns(&conn, Some(1_795_000_999), None).unwrap();
        assert_eq!(turns.len(), 4);

        // Turn 0: merged gen_metadata(1) + steps(1).f28 retry via id_11="r1".
        assert_eq!(turns[0].ts_secs, Some(1_795_000_100));
        assert_eq!(turns[0].ts_quality, TS_QUALITY_STEP_PRIMARY);
        assert_eq!(turns[0].input_tokens, 1_200);
        assert_eq!(turns[0].output_tokens, 300);
        assert_eq!(turns[0].id_11.as_deref(), Some("r1"));
        assert_eq!(turns[0].id_7.as_deref(), Some("c1"));
        assert_eq!(
            turns[0].model_display_name.as_deref(),
            Some("Gemini 4 Hydrogen (Orchestrator)")
        );
        assert_eq!(turns[0].decode_speed, Some(100.0));
        assert_eq!(turns[0].ttft_secs, Some(0.5));

        // Turn 1: f17 retry on model_enum=1418 did NOT inherit outer strings.
        assert_eq!(turns[1].model_enum, 1418);
        assert_eq!(turns[1].response_model, None);
        assert_eq!(turns[1].model_display_name, None);
        assert_eq!(turns[1].input_tokens, 600);

        // Turn 2: gen_metadata(2) fell back to trajectory_metadata_blob.f2 (q=2) before db_mtime.
        assert_eq!(turns[2].ts_secs, Some(1_795_000_000));
        assert_eq!(turns[2].ts_quality, TS_QUALITY_TRAJECTORY);

        // Turn 3: uncovered compaction step(2).f9 with f8 timestamp (q=3).
        assert_eq!(turns[3].ts_secs, Some(1_795_000_200));
        assert_eq!(turns[3].ts_quality, TS_QUALITY_SECONDARY);
        assert_eq!(turns[3].model_enum, 1669);
        assert_eq!(turns[3].input_tokens, 4_000);
        assert_eq!(turns[3].output_tokens, 80);
    }

    #[test]
    fn project_sqlite_union_find_transitively_dedups_three_records_across_roots() {
        let dir = tempfile::tempdir().unwrap();
        let pricing = test_pricing_catalog(&dir.path().join("cache"));

        let desk_a = vec![RawTurn {
            ts_secs: Some(1_795_002_000),
            ts_quality: TS_QUALITY_DB_MTIME,
            model_enum: 1669,
            response_model: Some("gemini-flash-worker-agy".to_string()),
            model_display_name: Some("Gemini Flash".to_string()),
            input_tokens: 1_000,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            output_tokens: 100,
            thinking_tokens: 20,
            native_cost_usd: None,
            decode_speed: None,
            ttft_secs: None,
            id_11: Some("r-shared".to_string()),
            id_12: None,
            id_7: None,
        }];
        let cli_b = vec![RawTurn {
            ts_secs: Some(1_795_001_500),
            ts_quality: TS_QUALITY_STEP_PRIMARY,
            model_enum: 1669,
            response_model: Some("gemini-flash-worker-agy".to_string()),
            model_display_name: Some("Gemini 4 Hydrogen (Orchestrator)".to_string()),
            input_tokens: 2_000,
            cache_read_tokens: 500,
            cache_write_tokens: 0,
            output_tokens: 200,
            thinking_tokens: 50,
            native_cost_usd: None,
            decode_speed: Some(100.0),
            ttft_secs: Some(0.5),
            id_11: Some("r-shared".to_string()),
            id_12: None,
            id_7: Some("c-shared".to_string()),
        }];
        let desk_c = vec![RawTurn {
            ts_secs: Some(1_795_001_400),
            ts_quality: TS_QUALITY_STEP_PRIMARY,
            model_enum: 0,
            response_model: None,
            model_display_name: None,
            input_tokens: 1_500,
            cache_read_tokens: 0,
            cache_write_tokens: 100,
            output_tokens: 500,
            thinking_tokens: 120,
            native_cost_usd: None,
            decode_speed: Some(80.0),
            ttft_secs: Some(0.9),
            id_11: None,
            id_12: Some("m-shared".to_string()),
            id_7: Some("c-shared".to_string()),
        }];

        let (stats, events) = project_sqlite_conversations(
            [
                ("desktop", desk_a.as_slice()),
                ("cli", cli_b.as_slice()),
                ("desktop", desk_c.as_slice()),
            ],
            &TimeZoneSpec::utc(),
            &pricing,
        );

        assert_eq!(stats.db_files, 3);
        assert_eq!(stats.completed_turns, 1);
        assert_eq!(stats.ttft_samples, 1);
        assert_eq!(stats.total_output_tokens, 500);
        assert_eq!(stats.median_tok_per_sec, Some(100.0));
        assert_eq!(stats.median_ttft_secs, Some(0.5));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].source, UsageSource::Cli);
        assert_eq!(events[0].input_tokens, 2_000);
        assert_eq!(events[0].cache_read_tokens, 500);
        assert_eq!(events[0].cache_write_tokens, 100);
        assert_eq!(events[0].output_tokens, 500);
        assert_eq!(events[0].thinking_tokens, 120);
        assert_eq!(events[0].model_group, "Gemini 4 Hydrogen (Orchestrator)");
        assert_eq!(events[0].pricing_key.as_deref(), Some("gemini-4-hydrogen"));
    }

    #[test]
    fn turn_pricer_learns_model_enum_without_generic_poisoning_and_poisons_conflicts() {
        let dir = tempfile::tempdir().unwrap();
        let pricing = test_pricing_catalog(&dir.path().join("cache"));
        let ts = Some(1_795_000_000_i64);
        let mk = |model_enum: u64, resp: Option<&str>, disp: Option<&str>| RawTurn {
            ts_secs: ts,
            ts_quality: TS_QUALITY_STEP_PRIMARY,
            model_enum,
            response_model: resp.map(str::to_string),
            model_display_name: disp.map(str::to_string),
            input_tokens: 1_000_000,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            output_tokens: 0,
            thinking_tokens: 0,
            native_cost_usd: None,
            decode_speed: None,
            ttft_secs: None,
            id_11: None,
            id_12: None,
            id_7: None,
        };

        let turns = vec![
            // Enum 1669: generic "Gemini Flash" + concrete Hydrogen must NOT poison 1669.
            mk(1669, Some("gemini-flash-worker-agy"), Some("Gemini Flash")),
            mk(
                1669,
                Some("gemini-flash-worker-agy"),
                Some("Gemini 4 Hydrogen (Orchestrator)"),
            ),
            mk(1669, None, None),
            // Enum 1418: "gemini-4-argon" + "gemini-4-argon-trial" resolve identically -> NOT poisoned.
            mk(1418, Some("gemini-4-argon"), Some("Gemini 4 Argon (High)")),
            mk(1418, Some("gemini-4-argon-trial"), None),
            mk(1418, None, None),
            // Enum 9999: genuine conflict Carbon vs Argon -> POISONED.
            mk(9999, Some("gemini-4-carbon"), Some("Gemini 4 Carbon")),
            mk(9999, Some("gemini-4-argon"), Some("Gemini 4 Argon")),
            mk(9999, None, None),
        ];

        let (_, events) = project_sqlite_conversations(
            [("cli", turns.as_slice())],
            &TimeZoneSpec::utc(),
            &pricing,
        );
        assert_eq!(events.len(), 9);

        // Bare enum 1669 compaction turn learns Hydrogen.
        assert_eq!(events[2].model_group, "Gemini 4 Hydrogen (Orchestrator)");
        assert_eq!(events[2].pricing_key.as_deref(), Some("gemini-4-hydrogen"));
        assert_eq!(events[2].cost_usd, Some(1.5));

        // Bare enum 1418 turn learns Argon.
        assert_eq!(events[5].model_group, "Gemini 4 Argon");
        assert_eq!(events[5].pricing_key.as_deref(), Some("gemini-4-argon"));
        assert_eq!(events[5].cost_usd, Some(4.0));

        // Poisoned enum 9999 bare turn remains unresolved and unpriced.
        assert_eq!(events[8].model_group, "model#9999");
        assert_eq!(events[8].pricing_key, None);
        assert_eq!(events[8].cost_usd, None);
    }

    #[test]
    fn global_scan_filters_forks_aggregates_diagnostics_and_caches_with_wal_invalidation() {
        use crate::agy::protobuf::{
            encode_gen_blob, encode_step_blob, GenBlobSpec, RetrySpec, StepBlobSpec, UsageSpec,
        };

        let dir = tempfile::tempdir().unwrap();
        let cli_root = dir.path().join(".gemini/antigravity-cli");
        let conv_dir = cli_root.join("conversations");
        fs::create_dir_all(&conv_dir).unwrap();

        let parent_id = "11111111-1111-4111-8111-111111111111";
        let fork_id = "22222222-2222-4222-8222-222222222222";
        let fork_db = conv_dir.join(format!("{fork_id}.db"));

        // WAL-mode fork DB with cloned parent turn (parent DB deleted) + own turn + step_type 17/23 + retries.
        let conn = Connection::open(&fork_db).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE steps (idx INTEGER PRIMARY KEY, step_type INTEGER, metadata BLOB);
             CREATE TABLE gen_metadata (idx INTEGER PRIMARY KEY, data BLOB);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO steps (idx, step_type, metadata) VALUES (1, 17, ?1)",
            [encode_step_blob(StepBlobSpec {
                created_at_secs: Some(1_795_000_100),
                origin_cascade_id: Some(parent_id),
                retry_records: vec![RetrySpec {
                    error: Some("HTTP 429 cloned"),
                    reason_code: 429,
                    delay_secs: 5,
                    ..Default::default()
                }],
                ..Default::default()
            })],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO steps (idx, step_type, metadata) VALUES (2, 17, ?1)",
            [encode_step_blob(StepBlobSpec {
                created_at_secs: Some(1_795_000_200),
                origin_cascade_id: Some(fork_id),
                ..Default::default()
            })],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO steps (idx, step_type, metadata) VALUES (3, 23, ?1)",
            [encode_step_blob(StepBlobSpec {
                created_at_secs: Some(1_795_000_250),
                origin_cascade_id: Some(fork_id),
                usage: Some(UsageSpec {
                    model_enum: 1669,
                    input_tok: 500,
                    output_tok: 50,
                    ..Default::default()
                }),
                retry_records: vec![RetrySpec {
                    error: Some("No capacity available"),
                    reason_code: 0,
                    delay_secs: 2,
                    ..Default::default()
                }],
                ..Default::default()
            })],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO gen_metadata (idx, data) VALUES (1, ?1)",
            [encode_gen_blob(GenBlobSpec {
                step_indices: vec![1],
                origin_cascade_id: Some(parent_id),
                model_enum: 1669,
                resp_model: Some("gemini-4-hydrogen"),
                input_tok: 99_999,
                output_tok: 999,
                dur_secs: 1,
                ..Default::default()
            })],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO gen_metadata (idx, data) VALUES (2, ?1)",
            [encode_gen_blob(GenBlobSpec {
                step_indices: vec![2],
                origin_cascade_id: Some(fork_id),
                model_enum: 1669,
                resp_model: Some("gemini-4-hydrogen"),
                input_tok: 1_000_000,
                output_tok: 100,
                dur_secs: 2,
                retry_records: vec![RetrySpec {
                    error: Some("HTTP 429 quota reached"),
                    reason_code: 429,
                    delay_secs: 1,
                    delay_nanos: 500_000_000,
                    ..Default::default()
                }],
                ..Default::default()
            })],
        )
        .unwrap();

        let roots = [("cli", cli_root)];
        let cache_dir = dir.path().join("cache");
        let pricing = test_pricing_catalog(&cache_dir);
        let stats_cache = cache_dir.join(STATS_SCAN_CACHE_FILE);

        // Cold scan: miss = 1, hit = 0; foreign parent rows excluded; own turns + diagnostics captured.
        let first = scan_sqlite_conversations_with_cache(
            &roots,
            &TimeZoneSpec::utc(),
            &pricing,
            Some(&stats_cache),
        );
        assert_eq!(first.cache_misses, 1);
        assert_eq!(first.cache_hits, 0);
        assert_eq!(first.events.len(), 2);
        assert_eq!(first.diagnostics.retry_count, 2);
        assert_eq!(first.diagnostics.retries_429, 1);
        assert_eq!(first.diagnostics.retries_503, 1);
        assert!((first.diagnostics.total_retry_delay_secs - 3.5).abs() < 1e-6);
        assert_eq!(first.diagnostics.error_steps, 1);
        assert_eq!(first.diagnostics.compaction_steps, 1);
        assert!(stats_cache.is_file());

        // Second scan on unchanged DB hits cache and preserves identical events & diagnostics.
        let second = scan_sqlite_conversations_with_cache(
            &roots,
            &TimeZoneSpec::utc(),
            &pricing,
            Some(&stats_cache),
        );
        assert_eq!(second.cache_hits, 1);
        assert_eq!(second.cache_misses, 0);
        assert_eq!(second.events.len(), 2);
        assert_eq!(second.diagnostics, first.diagnostics);

        // WAL-only mutation (writer stays open so new frame lands in -wal) invalidates cache.
        conn.execute(
            "INSERT INTO gen_metadata (idx, data) VALUES (3, ?1)",
            [encode_gen_blob(GenBlobSpec {
                step_indices: vec![2],
                origin_cascade_id: Some(fork_id),
                model_enum: 1669,
                resp_model: Some("gemini-4-hydrogen"),
                input_tok: 2_000,
                output_tok: 200,
                dur_secs: 2,
                ..Default::default()
            })],
        )
        .unwrap();
        let third = scan_sqlite_conversations_with_cache(
            &roots,
            &TimeZoneSpec::utc(),
            &pricing,
            Some(&stats_cache),
        );
        assert_eq!(third.cache_hits, 0);
        assert_eq!(third.cache_misses, 1);
        assert_eq!(third.events.len(), 3);
        drop(conn);
    }

    #[test]
    fn extract_sqlite_db_data_dedups_mirrored_gen_and_step_retries_while_keeping_compaction_retries(
    ) {
        use crate::agy::protobuf::{
            encode_gen_blob, encode_step_blob, GenBlobSpec, RetrySpec, StepBlobSpec,
        };

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("mirrored_retries.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE steps (idx INTEGER PRIMARY KEY, step_type INTEGER, metadata BLOB);
             CREATE TABLE gen_metadata (idx INTEGER PRIMARY KEY, data BLOB);",
        )
        .unwrap();

        // Step 1 is covered by gen_metadata(1), and BOTH mirror the same 429 retry -> count ONCE.
        conn.execute(
            "INSERT INTO steps (idx, step_type, metadata) VALUES (1, 15, ?1)",
            [encode_step_blob(StepBlobSpec {
                created_at_secs: Some(1_795_000_100),
                retry_records: vec![RetrySpec {
                    error: Some("HTTP 429 rate limit"),
                    reason_code: 429,
                    delay_secs: 2,
                    ..Default::default()
                }],
                ..Default::default()
            })],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO gen_metadata (idx, data) VALUES (1, ?1)",
            [encode_gen_blob(GenBlobSpec {
                step_indices: vec![1],
                model_enum: 1669,
                input_tok: 1_000,
                output_tok: 100,
                dur_secs: 1,
                retry_records: vec![RetrySpec {
                    error: Some("HTTP 429 rate limit"),
                    reason_code: 429,
                    delay_secs: 2,
                    ..Default::default()
                }],
                ..Default::default()
            })],
        )
        .unwrap();

        // Step 2 is an uncovered compaction checkpoint (step_type = 23) with a 503 retry -> counted.
        conn.execute(
            "INSERT INTO steps (idx, step_type, metadata) VALUES (2, 23, ?1)",
            [encode_step_blob(StepBlobSpec {
                created_at_secs: Some(1_795_000_200),
                retry_records: vec![RetrySpec {
                    error: Some("HTTP 503 overloaded"),
                    reason_code: 503,
                    delay_secs: 3,
                    ..Default::default()
                }],
                ..Default::default()
            })],
        )
        .unwrap();

        let data = extract_sqlite_db_data(&conn, None, None).unwrap();
        assert_eq!(data.diagnostics.retry_count, 2);
        assert_eq!(data.diagnostics.retries_429, 1);
        assert_eq!(data.diagnostics.retries_503, 1);
        assert_eq!(data.diagnostics.retries_non_429_503(), 0);
        assert!((data.diagnostics.total_retry_delay_secs - 5.0).abs() < 1e-6);
        assert_eq!(data.diagnostics.compaction_steps, 1);
        assert!(!data.diagnostics.is_empty());
    }
}
