//! Persistent per-DB raw unpriced scan cache (`stats_scan_cache.json`) for global `stats`.
//!
//! Caches per-DB extracted [`ScannedDbData`] (pre-[`super::TurnPricer`] raw turns and
//! retry/error/compaction diagnostics) keyed by canonical DB path and [`DbFileFingerprint`].
//! Unchanged databases skip `sqlite3_open` and protobuf decoding entirely while dynamic
//! timezone and pricing catalog projection still run fresh in memory.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::{DbFileFingerprint, ScannedDbData};
use crate::sys::fs::{
    atomic_write_preserving_mode, is_safe_cache_target, read_bounded_regular_file,
};

pub(crate) const STATS_SCAN_CACHE_FILE: &str = "stats_scan_cache.json";
const STATS_SCAN_CACHE_SCHEMA_VERSION: u32 = 1;
const MAX_STATS_SCAN_CACHE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_STATS_SCAN_CACHE_ENTRIES: usize = 10_000;

/// One cached database scan snapshot guarded by its file + WAL fingerprint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct CachedDbEntry {
    pub(super) fingerprint: DbFileFingerprint,
    pub(super) data: ScannedDbData,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct StatsScanCacheFile {
    schema_version: u32,
    #[serde(default)]
    entries: BTreeMap<String, CachedDbEntry>,
}

/// In-memory view of `stats_scan_cache.json` tracking whether disk persistence is needed.
#[derive(Debug, Default)]
pub(super) struct GlobalStatsCache {
    entries: BTreeMap<String, CachedDbEntry>,
    dirty: bool,
}

impl GlobalStatsCache {
    /// Loads the cache from `cache_path`, rejecting symlinked parents/files, oversized payloads,
    /// and mismatched schemas using a bounded read (`MAX_STATS_SCAN_CACHE_BYTES + 1`).
    pub(super) fn load(cache_path: &Path) -> Self {
        let Ok((bytes, _)) = read_bounded_regular_file(cache_path, MAX_STATS_SCAN_CACHE_BYTES)
        else {
            return Self::default();
        };
        let Ok(parsed) = serde_json::from_slice::<StatsScanCacheFile>(&bytes) else {
            return Self::default();
        };
        if parsed.schema_version != STATS_SCAN_CACHE_SCHEMA_VERSION {
            return Self::default();
        }
        Self {
            entries: parsed.entries,
            dirty: false,
        }
    }

    /// Returns cached [`ScannedDbData`] when `canonical_key` exists and its fingerprint matches.
    pub(super) fn get_matching(
        &self,
        canonical_key: &str,
        fingerprint: DbFileFingerprint,
    ) -> Option<&ScannedDbData> {
        let entry = self.entries.get(canonical_key)?;
        (entry.fingerprint == fingerprint).then_some(&entry.data)
    }

    /// Inserts or updates a scanned DB entry and marks the cache dirty if it changed.
    pub(super) fn upsert(
        &mut self,
        canonical_key: String,
        fingerprint: DbFileFingerprint,
        data: ScannedDbData,
    ) {
        let next = CachedDbEntry { fingerprint, data };
        if self.entries.get(&canonical_key) != Some(&next) {
            self.entries.insert(canonical_key, next);
            self.dirty = true;
        }
    }

    /// Removes entries whose canonical DB paths are absent from `live_keys`.
    pub(super) fn prune_missing(&mut self, live_keys: &HashSet<String>) {
        let before = self.entries.len();
        self.entries.retain(|key, _| live_keys.contains(key));
        if self.entries.len() != before {
            self.dirty = true;
        }
    }

    /// Persists the cache atomically via [`atomic_write_preserving_mode`] only if modified,
    /// enforcing symlink rejection and the serialized size cap.
    pub(super) fn save_if_dirty(&mut self, cache_path: &Path) -> bool {
        if !self.dirty {
            return false;
        }
        if !is_safe_cache_target(cache_path) {
            return false;
        }

        while self.entries.len() > MAX_STATS_SCAN_CACHE_ENTRIES {
            let Some(first_key) = self.entries.keys().next().cloned() else {
                break;
            };
            self.entries.remove(&first_key);
        }

        let payload = StatsScanCacheFile {
            schema_version: STATS_SCAN_CACHE_SCHEMA_VERSION,
            entries: self.entries.clone(),
        };
        let Ok(bytes) = serde_json::to_vec(&payload) else {
            return false;
        };
        if bytes.len() as u64 > MAX_STATS_SCAN_CACHE_BYTES {
            return false;
        }

        if atomic_write_preserving_mode(cache_path, &bytes).is_ok() {
            self.dirty = false;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::scan::{RawTurn, ScanDiagnostics};
    #[cfg(unix)]
    use std::fs;

    fn sample_fp(size: u64, wal_size: u64, wal_ts: i64) -> DbFileFingerprint {
        DbFileFingerprint {
            db_mtime_secs: 1_795_000_000,
            db_mtime_nanos: 0,
            db_ctime_secs: 1_795_000_000,
            db_ctime_nanos: 0,
            db_size: size,
            wal_mtime_secs: wal_ts,
            wal_mtime_nanos: 0,
            wal_ctime_secs: wal_ts,
            wal_ctime_nanos: 0,
            wal_size,
        }
    }

    fn sample_data(input_tokens: u64) -> ScannedDbData {
        ScannedDbData {
            turns: vec![RawTurn {
                ts_secs: Some(1_795_000_100),
                ts_quality: 4,
                model_enum: 1669,
                response_model: Some("gemini-4-hydrogen".to_string()),
                model_display_name: Some("Gemini 4 Hydrogen".to_string()),
                input_tokens,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                output_tokens: 100,
                thinking_tokens: 20,
                native_cost_usd: None,
                decode_speed: Some(50.0),
                ttft_secs: Some(0.5),
                id_11: Some("turn-1".to_string()),
                id_12: None,
                id_7: None,
            }],
            diagnostics: ScanDiagnostics {
                retry_count: 1,
                retries_429: 1,
                total_retry_delay_secs: 1.5,
                error_steps: 1,
                compaction_steps: 2,
                ..Default::default()
            },
        }
    }

    #[test]
    fn global_cache_roundtrips_prunes_deleted_and_skips_clean_writes() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("stats_scan_cache.json");

        let mut cache = GlobalStatsCache::load(&cache_path);
        assert!(!cache.save_if_dirty(&cache_path));
        assert!(!cache_path.exists());

        let fp_a = sample_fp(4096, 0, 0);
        let fp_b = sample_fp(8192, 512, 1_795_000_010);
        cache.upsert("/db/a.db".to_string(), fp_a, sample_data(1_000));
        cache.upsert("/db/b.db".to_string(), fp_b, sample_data(2_000));
        assert!(cache.save_if_dirty(&cache_path));
        assert!(!cache.save_if_dirty(&cache_path));

        let mut reloaded = GlobalStatsCache::load(&cache_path);
        assert_eq!(
            reloaded.get_matching("/db/a.db", fp_a).unwrap().turns[0].input_tokens,
            1_000
        );
        assert_eq!(
            reloaded
                .get_matching("/db/a.db", fp_a)
                .unwrap()
                .diagnostics
                .compaction_steps,
            2
        );
        assert!(reloaded
            .get_matching("/db/a.db", sample_fp(4096, 64, 1_795_000_010))
            .is_none());

        let live = HashSet::from(["/db/a.db".to_string()]);
        reloaded.prune_missing(&live);
        assert!(reloaded.save_if_dirty(&cache_path));

        let pruned = GlobalStatsCache::load(&cache_path);
        assert!(pruned.get_matching("/db/a.db", fp_a).is_some());
        assert!(pruned.get_matching("/db/b.db", fp_b).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn global_cache_rejects_symlinks_on_read_and_write() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let real_target = dir.path().join("target.json");
        fs::write(&real_target, b"sentinel").unwrap();
        let link_path = dir.path().join("stats_scan_cache.json");
        symlink(&real_target, &link_path).unwrap();

        let mut cache = GlobalStatsCache::load(&link_path);
        cache.upsert(
            "/db/a.db".to_string(),
            sample_fp(4096, 0, 0),
            sample_data(500),
        );
        assert!(!cache.save_if_dirty(&link_path));
        assert_eq!(fs::read(&real_target).unwrap(), b"sentinel");

        // Symlinked parent directory is also rejected on both read and write.
        let real_dir = dir.path().join("real_cache_dir");
        fs::create_dir_all(&real_dir).unwrap();
        let valid_cache = real_dir.join("stats_scan_cache.json");
        let mut seed = GlobalStatsCache::default();
        seed.upsert(
            "/db/a.db".to_string(),
            sample_fp(4096, 0, 0),
            sample_data(500),
        );
        assert!(seed.save_if_dirty(&valid_cache));

        let sym_dir = dir.path().join("sym_cache_dir");
        symlink(&real_dir, &sym_dir).unwrap();
        let via_sym_parent = sym_dir.join("stats_scan_cache.json");
        let loaded = GlobalStatsCache::load(&via_sym_parent);
        assert!(loaded
            .get_matching("/db/a.db", sample_fp(4096, 0, 0))
            .is_none());
        let mut writer = GlobalStatsCache::default();
        writer.upsert(
            "/db/c.db".to_string(),
            sample_fp(4096, 0, 0),
            sample_data(999),
        );
        assert!(!writer.save_if_dirty(&via_sym_parent));
    }
}
