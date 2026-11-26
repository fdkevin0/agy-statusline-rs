//! Per-conversation SQLite cost estimation and bounded LRU cost cache (`conv_cost_cache.json`).

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::{
    db_file_fingerprint, deduplicate_parent_priority_turns, extract_sqlite_db_turns,
    merge_raw_turn, read_single_db_turns, resolve_descendant_conversation_ids, root_path_key,
    DbFileFingerprint, RawTurn, TurnPricer,
};
use crate::agy::db::{is_valid_uuid, resolve_conversation_db_path};
use crate::stats::pricing::{round_usd_6, PricingCatalog, PricingFileFingerprint};
use crate::sys::fs::{atomic_write, is_safe_cache_target, read_bounded_regular_file};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub(crate) struct ConversationCost {
    pub(crate) usd: Option<f64>,
    pub(crate) subagent_usd: Option<f64>,
    pub(crate) unpriced_turns: u64,
}

const CONV_COST_CACHE_FILE: &str = "conv_cost_cache.json";
const CONV_COST_CACHE_SCHEMA_VERSION: u32 = 2;
const CONV_COST_CACHE_MAX_ENTRIES: usize = 32;
const MAX_CONV_COST_CACHE_BYTES: u64 = 512 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct ConvCostCacheEntry {
    cli_dir: String,
    conversation_id: String,
    parent_fp: Option<DbFileFingerprint>,
    summaries_fp: Option<DbFileFingerprint>,
    descendants: Vec<(String, Option<DbFileFingerprint>)>,
    pricing_fp: PricingFileFingerprint,
    cost: ConversationCost,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ConvCostCacheFile {
    schema_version: u32,
    #[serde(default)]
    entries: Vec<ConvCostCacheEntry>,
}

fn conversation_db_fingerprint(cli_dir: &Path, conversation_id: &str) -> Option<DbFileFingerprint> {
    let db_path = resolve_conversation_db_path(cli_dir, conversation_id)?;
    db_file_fingerprint(&db_path)
}

fn summaries_db_fingerprint(cli_dir: &Path) -> Option<DbFileFingerprint> {
    db_file_fingerprint(&cli_dir.join("conversation_summaries.db"))
}

fn read_conv_cost_cache_file(cache_dir: &Path) -> Option<ConvCostCacheFile> {
    let path = cache_dir.join(CONV_COST_CACHE_FILE);
    let (bytes, _) = read_bounded_regular_file(&path, MAX_CONV_COST_CACHE_BYTES).ok()?;
    let parsed = serde_json::from_slice::<ConvCostCacheFile>(&bytes).ok()?;
    (parsed.schema_version == CONV_COST_CACHE_SCHEMA_VERSION).then_some(parsed)
}

fn write_conv_cost_cache_entry_with_limit(
    cache_dir: &Path,
    entry: ConvCostCacheEntry,
    max_entries: usize,
) {
    let cache_path = cache_dir.join(CONV_COST_CACHE_FILE);
    if !is_safe_cache_target(&cache_path) {
        return;
    }
    let mut file = read_conv_cost_cache_file(cache_dir).unwrap_or(ConvCostCacheFile {
        schema_version: CONV_COST_CACHE_SCHEMA_VERSION,
        entries: Vec::new(),
    });
    file.entries
        .retain(|e| !(e.cli_dir == entry.cli_dir && e.conversation_id == entry.conversation_id));
    file.entries.insert(0, entry);
    file.entries.truncate(max_entries);
    if let Ok(bytes) = serde_json::to_vec(&file) {
        let _ = atomic_write(&cache_path, &bytes);
    }
}

/// Fast `lstat`-only lookup in `conv_cost_cache.json` before parsing `models_dev_pricing.json`
/// or opening any SQLite DB for cost scanning.
pub(crate) fn lookup_cached_conversation_cost(
    cache_dir: &Path,
    cli_dir: &Path,
    conversation_id: &str,
    pricing_fp: PricingFileFingerprint,
) -> Option<ConversationCost> {
    if !is_valid_uuid(conversation_id) {
        return None;
    }
    let parent_fp = conversation_db_fingerprint(cli_dir, conversation_id);
    let summaries_fp = summaries_db_fingerprint(cli_dir);
    if parent_fp.is_none() && summaries_fp.is_none() {
        return None;
    }
    let cache = read_conv_cost_cache_file(cache_dir)?;
    let cli_dir_key = root_path_key(cli_dir);
    let conv_id_lower = conversation_id.to_ascii_lowercase();
    let entry = cache
        .entries
        .iter()
        .find(|e| e.cli_dir == cli_dir_key && e.conversation_id == conv_id_lower)?;
    if entry.parent_fp != parent_fp
        || entry.summaries_fp != summaries_fp
        || entry.pricing_fp != pricing_fp
    {
        return None;
    }
    let descendants_match = entry
        .descendants
        .iter()
        .all(|(desc_id, desc_fp)| conversation_db_fingerprint(cli_dir, desc_id) == *desc_fp);
    if !descendants_match {
        return None;
    }
    if !entry.cost.usd.is_none_or(|v| v.is_finite() && v >= 0.0)
        || !entry
            .cost
            .subagent_usd
            .is_none_or(|v| v.is_finite() && v >= 0.0)
    {
        return None;
    }
    Some(entry.cost)
}

fn compute_conversation_cost_from_turns(
    tagged_turns: Vec<(bool, RawTurn)>,
    pricing: &PricingCatalog,
) -> Option<ConversationCost> {
    if tagged_turns.is_empty() {
        return None;
    }

    let deduped = deduplicate_parent_priority_turns(
        tagged_turns,
        |(_, t)| t,
        |(sub, _)| *sub,
        |(_, dst), (_, other)| merge_raw_turn(dst, other),
    );

    let pricer = TurnPricer::build(pricing, deduped.iter().map(|(_, t)| t));
    let mut parent_usd = 0.0_f64;
    let mut parent_priced = 0_u64;
    let mut parent_unpriced = 0_u64;
    let mut sub_usd = 0.0_f64;
    let mut sub_priced = 0_u64;
    let mut sub_unpriced = 0_u64;

    for (is_sub, turn) in &deduped {
        if !turn.has_billable_usage() {
            continue;
        }
        let resolved = pricer.price_turn(turn);
        match (is_sub, resolved.cost_usd) {
            (false, Some(usd)) => {
                parent_usd += usd;
                parent_priced += 1;
            }
            (false, None) => parent_unpriced += 1,
            (true, Some(usd)) => {
                sub_usd += usd;
                sub_priced += 1;
            }
            (true, None) => sub_unpriced += 1,
        }
    }

    let total_priced = parent_priced + sub_priced;
    let total_unpriced = parent_unpriced + sub_unpriced;
    let total_usd = parent_usd + sub_usd;
    (total_priced > 0 || total_unpriced > 0).then(|| ConversationCost {
        usd: (total_priced > 0 && total_usd.is_finite()).then(|| round_usd_6(total_usd)),
        subagent_usd: (sub_priced > 0 && sub_usd.is_finite()).then(|| round_usd_6(sub_usd)),
        unpriced_turns: total_unpriced,
    })
}

/// Estimates the subtotal for priced usage in a SQLite conversation and any descendant subagents,
/// optionally reusing an already-open read-only parent connection and persisting the result into
/// the bounded LRU `conv_cost_cache.json` cache.
pub(crate) fn estimate_conversation_cost_usd_with_parent_conn(
    cli_dir: &Path,
    conversation_id: &str,
    pricing: &PricingCatalog,
    parent_conn: Option<&rusqlite::Connection>,
    cache_target: Option<(&Path, PricingFileFingerprint)>,
) -> Option<ConversationCost> {
    if !is_valid_uuid(conversation_id) {
        return None;
    }

    let parent_db_path = resolve_conversation_db_path(cli_dir, conversation_id);
    // Stat before reading so concurrent SQLite commits invalidate on the next tick.
    let parent_fp = parent_db_path.as_deref().and_then(db_file_fingerprint);

    let mut tagged_turns: Vec<(bool, RawTurn)> = Vec::new();
    if let Some(conn) = parent_conn {
        for turn in extract_sqlite_db_turns(conn, None, Some(conversation_id))? {
            tagged_turns.push((false, turn));
        }
    } else if let Some(db_path) = parent_db_path.as_deref() {
        for turn in read_single_db_turns(db_path, Some(conversation_id))? {
            tagged_turns.push((false, turn));
        }
    }

    let summaries_fp = summaries_db_fingerprint(cli_dir);
    let desc_ids = resolve_descendant_conversation_ids(cli_dir, conversation_id);
    let mut descendants = Vec::with_capacity(desc_ids.len());
    for desc_id in desc_ids {
        let desc_db_path = resolve_conversation_db_path(cli_dir, &desc_id);
        let desc_fp = desc_db_path.as_deref().and_then(db_file_fingerprint);
        descendants.push((desc_id.to_ascii_lowercase(), desc_fp));
        if let Some(turns) = desc_db_path
            .as_deref()
            .and_then(|p| read_single_db_turns(p, Some(&desc_id)))
        {
            for turn in turns {
                tagged_turns.push((true, turn));
            }
        }
    }

    let cost = compute_conversation_cost_from_turns(tagged_turns, pricing)?;
    if let Some((cache_dir, pricing_fp)) = cache_target {
        write_conv_cost_cache_entry_with_limit(
            cache_dir,
            ConvCostCacheEntry {
                cli_dir: root_path_key(cli_dir),
                conversation_id: conversation_id.to_ascii_lowercase(),
                parent_fp,
                summaries_fp,
                descendants,
                pricing_fp,
                cost,
            },
            CONV_COST_CACHE_MAX_ENTRIES,
        );
    }
    Some(cost)
}

/// Estimates the subtotal for priced usage in a SQLite conversation and any descendant subagents.
#[cfg(test)]
fn estimate_conversation_cost_usd(
    cli_dir: &Path,
    conversation_id: &str,
    pricing: &PricingCatalog,
) -> Option<ConversationCost> {
    estimate_conversation_cost_usd_with_parent_conn(cli_dir, conversation_id, pricing, None, None)
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;
    use std::fs;
    use std::path::Path;

    use super::*;
    use crate::stats::pricing::PricingCatalog;
    use crate::stats::scan::tests::test_pricing_catalog;
    use crate::stats::test_helpers::build_gen_blob;

    fn seed_cost_db(cli_dir: &Path, uuid: &str, idx: i64, blob: Vec<u8>) {
        let db_path = cli_dir.join("conversations").join(format!("{uuid}.db"));
        fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let conn = Connection::open(&db_path).unwrap();
        conn.execute(
            "CREATE TABLE IF NOT EXISTS gen_metadata (idx INTEGER PRIMARY KEY, data BLOB)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO gen_metadata (idx, data) VALUES (?1, ?2)",
            rusqlite::params![idx, blob],
        )
        .unwrap();
    }

    #[test]
    fn estimate_conversation_cost_aggregates_descendant_subagents_and_guards_cycles() {
        let dir = tempfile::tempdir().unwrap();
        let cli_dir = dir.path().join(".gemini/antigravity-cli");
        let parent = "11111111-1111-4111-8111-111111111111";
        let child = "22222222-2222-4222-8222-222222222222";
        let grandchild = "33333333-3333-4333-8333-333333333333";
        let missing_child = "44444444-4444-4444-8444-444444444444";
        let unrelated = "55555555-5555-4555-8555-555555555555";
        let pricing = PricingCatalog::default();

        seed_cost_db(
            &cli_dir,
            parent,
            1,
            build_gen_blob(None, 10, "m-parent", "", 100, 0, 50, 0, 1, 100, Some(0.10)),
        );
        seed_cost_db(
            &cli_dir,
            child,
            1,
            build_gen_blob(None, 10, "m-child", "", 100, 0, 50, 0, 1, 100, Some(0.20)),
        );
        seed_cost_db(
            &cli_dir,
            grandchild,
            1,
            build_gen_blob(None, 10, "m-grand", "", 100, 0, 50, 0, 1, 100, Some(0.15)),
        );
        seed_cost_db(
            &cli_dir,
            grandchild,
            2,
            build_gen_blob(None, 10, "m-unpriced", "", 100, 0, 50, 0, 1, 100, None),
        );
        seed_cost_db(
            &cli_dir,
            unrelated,
            1,
            build_gen_blob(None, 10, "m-other", "", 100, 0, 50, 0, 1, 100, Some(9.99)),
        );

        // Without conversation_summaries.db, only parent is attributed.
        let parent_only = estimate_conversation_cost_usd(&cli_dir, parent, &pricing).unwrap();
        assert_eq!(parent_only.usd, Some(0.1));
        assert_eq!(parent_only.subagent_usd, None);
        assert_eq!(parent_only.unpriced_turns, 0);

        let summaries = Connection::open(cli_dir.join("conversation_summaries.db")).unwrap();
        summaries
            .execute(
                "CREATE TABLE conversation_summaries (
                    conversation_id TEXT,
                    parent_conversation_id TEXT,
                    nesting_depth INTEGER
                )",
                [],
            )
            .unwrap();
        summaries
            .execute(
                "INSERT INTO conversation_summaries VALUES (?1, ?2, 1)",
                [child, parent],
            )
            .unwrap();
        summaries
            .execute(
                "INSERT INTO conversation_summaries VALUES (?1, ?2, 2)",
                [grandchild, child],
            )
            .unwrap();
        summaries
            .execute(
                "INSERT INTO conversation_summaries VALUES (?1, ?2, 1)",
                [missing_child, parent],
            )
            .unwrap();
        summaries
            .execute(
                "INSERT INTO conversation_summaries VALUES (?1, ?2, 1)",
                [parent, parent],
            )
            .unwrap();
        summaries
            .execute(
                "INSERT INTO conversation_summaries VALUES (?1, ?2, 3)",
                [parent, grandchild],
            )
            .unwrap();
        summaries
            .execute(
                "INSERT INTO conversation_summaries VALUES ('not-a-uuid', ?1, 1)",
                [parent],
            )
            .unwrap();
        summaries
            .execute(
                "INSERT INTO conversation_summaries VALUES (?1, NULL, 0)",
                [unrelated],
            )
            .unwrap();
        drop(summaries);

        let combined = estimate_conversation_cost_usd(&cli_dir, parent, &pricing).unwrap();
        assert_eq!(combined.usd, Some(0.45));
        assert_eq!(combined.subagent_usd, Some(0.35));
        assert_eq!(combined.unpriced_turns, 1);
    }

    #[test]
    fn estimate_conversation_cost_uses_turn_pricer_for_compaction_and_worker_inference() {
        use crate::agy::protobuf::{
            encode_gen_blob, encode_step_blob, GenBlobSpec, StepBlobSpec, UsageSpec,
        };

        let dir = tempfile::tempdir().unwrap();
        let cli_dir = dir.path().join(".gemini/antigravity-cli");
        let pricing = test_pricing_catalog(&dir.path().join("cache"));
        let parent = "11111111-1111-4111-8111-111111111111";
        let child = "22222222-2222-4222-8222-222222222222";

        // Parent DB: 1M Hydrogen turn in gen_metadata + 1M bare compaction turn in steps.metadata.f9.
        let parent_db = cli_dir.join("conversations").join(format!("{parent}.db"));
        fs::create_dir_all(parent_db.parent().unwrap()).unwrap();
        let pconn = Connection::open(&parent_db).unwrap();
        pconn
            .execute_batch(
                "CREATE TABLE steps (idx INTEGER PRIMARY KEY, metadata BLOB);
                 CREATE TABLE gen_metadata (idx INTEGER PRIMARY KEY, data BLOB);",
            )
            .unwrap();
        pconn
            .execute(
                "INSERT INTO gen_metadata (idx, data) VALUES (1, ?1)",
                [encode_gen_blob(GenBlobSpec {
                    step_indices: vec![1],
                    model_enum: 1669,
                    resp_model: Some("gemini-flash-worker-agy"),
                    disp_name: Some("Gemini 4 Hydrogen (Orchestrator)"),
                    input_tok: 1_000_000,
                    ..Default::default()
                })],
            )
            .unwrap();
        pconn
            .execute(
                "INSERT INTO steps (idx, metadata) VALUES (2, ?1)",
                [encode_step_blob(StepBlobSpec {
                    created_at_secs: Some(1_795_000_000),
                    usage: Some(UsageSpec {
                        model_enum: 1669,
                        input_tok: 1_000_000,
                        ..Default::default()
                    }),
                    ..Default::default()
                })],
            )
            .unwrap();
        drop(pconn);

        // Child DB: 1M turn with legacy generic display name "Gemini Flash" on same worker routing ID.
        seed_cost_db(
            &cli_dir,
            child,
            1,
            encode_gen_blob(GenBlobSpec {
                step_indices: vec![1],
                model_enum: 0,
                resp_model: Some("gemini-flash-worker-agy"),
                disp_name: Some("Gemini Flash"),
                input_tok: 1_000_000,
                ..Default::default()
            }),
        );

        let summaries = Connection::open(cli_dir.join("conversation_summaries.db")).unwrap();
        summaries
            .execute(
                "CREATE TABLE conversation_summaries (
                    conversation_id TEXT,
                    parent_conversation_id TEXT,
                    nesting_depth INTEGER
                )",
                [],
            )
            .unwrap();
        summaries
            .execute(
                "INSERT INTO conversation_summaries VALUES (?1, ?2, 1)",
                [child, parent],
            )
            .unwrap();
        drop(summaries);

        let est = estimate_conversation_cost_usd(&cli_dir, parent, &pricing).unwrap();
        // Parent: $1.50 (main) + $1.50 (compaction) = $3.00; child: $1.50 -> total $4.50, sub $1.50.
        assert_eq!(est.usd, Some(4.5));
        assert_eq!(est.subagent_usd, Some(1.5));
        assert_eq!(est.unpriced_turns, 0);
    }

    #[cfg(unix)]
    #[test]
    fn db_file_fingerprint_tracks_ctime_and_zeros_idle_wal_ctime() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let wal_path = dir.path().join("test.db-wal");
        fs::write(&db_path, b"sqlite-bytes").unwrap();

        let fp_no_wal = db_file_fingerprint(&db_path).unwrap();
        assert!(fp_no_wal.db_ctime_secs > 0);
        assert_eq!(fp_no_wal.wal_ctime_secs, 0);
        assert_eq!(fp_no_wal.wal_ctime_nanos, 0);

        // 0-byte WAL must keep wal_ctime zeroed.
        fs::write(&wal_path, b"").unwrap();
        let fp_empty_wal = db_file_fingerprint(&db_path).unwrap();
        assert_eq!(fp_empty_wal, fp_no_wal);

        // Non-empty WAL records non-zero wal_ctime and changes the fingerprint.
        fs::write(&wal_path, b"wal-frame").unwrap();
        let fp_active_wal = db_file_fingerprint(&db_path).unwrap();
        assert!(fp_active_wal.wal_ctime_secs > 0);
        assert_ne!(fp_active_wal, fp_no_wal);

        // A ctime-only difference changes DbFileFingerprint equality.
        let mut altered = fp_no_wal;
        altered.db_ctime_secs = altered.db_ctime_secs.saturating_sub(1);
        assert_ne!(altered, fp_no_wal);
    }

    #[test]
    fn conv_cost_cache_bounds_lru_entries_and_promotes_updated_key() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        let mk_entry = |id: &str, usd: f64| ConvCostCacheEntry {
            cli_dir: "/isolated/cli".to_string(),
            conversation_id: id.to_string(),
            parent_fp: None,
            summaries_fp: None,
            descendants: Vec::new(),
            pricing_fp: PricingFileFingerprint::default(),
            cost: ConversationCost {
                usd: Some(usd),
                subagent_usd: None,
                unpriced_turns: 0,
            },
        };

        write_conv_cost_cache_entry_with_limit(
            cache_dir,
            mk_entry("11111111-1111-4111-8111-111111111111", 1.0),
            2,
        );
        write_conv_cost_cache_entry_with_limit(
            cache_dir,
            mk_entry("22222222-2222-4222-8222-222222222222", 2.0),
            2,
        );
        write_conv_cost_cache_entry_with_limit(
            cache_dir,
            mk_entry("33333333-3333-4333-8333-333333333333", 3.0),
            2,
        );

        let after_evict = read_conv_cost_cache_file(cache_dir).unwrap();
        assert_eq!(after_evict.entries.len(), 2);
        assert_eq!(
            after_evict.entries[0].conversation_id,
            "33333333-3333-4333-8333-333333333333"
        );
        assert_eq!(
            after_evict.entries[1].conversation_id,
            "22222222-2222-4222-8222-222222222222"
        );

        // Updating entry 2 promotes it to MRU so entry 3 is evicted next.
        write_conv_cost_cache_entry_with_limit(
            cache_dir,
            mk_entry("22222222-2222-4222-8222-222222222222", 2.5),
            2,
        );
        write_conv_cost_cache_entry_with_limit(
            cache_dir,
            mk_entry("44444444-4444-4444-8444-444444444444", 4.0),
            2,
        );

        let after_promote = read_conv_cost_cache_file(cache_dir).unwrap();
        assert_eq!(after_promote.entries.len(), 2);
        assert_eq!(
            after_promote.entries[0].conversation_id,
            "44444444-4444-4444-8444-444444444444"
        );
        assert_eq!(
            after_promote.entries[1].conversation_id,
            "22222222-2222-4222-8222-222222222222"
        );
        assert_eq!(after_promote.entries[1].cost.usd, Some(2.5));
    }

    #[test]
    fn estimate_conversation_cost_excludes_cloned_fork_turns() {
        use crate::agy::protobuf::{encode_gen_blob, GenBlobSpec};

        let dir = tempfile::tempdir().unwrap();
        let cli_dir = dir.path().join(".gemini/antigravity-cli");
        let pricing = PricingCatalog::default();
        let parent_id = "11111111-1111-4111-8111-111111111111";
        let fork_id = "22222222-2222-4222-8222-222222222222";

        seed_cost_db(
            &cli_dir,
            fork_id,
            1,
            encode_gen_blob(GenBlobSpec {
                origin_cascade_id: Some(parent_id),
                model_enum: 10,
                resp_model: Some("m-parent"),
                input_tok: 100,
                output_tok: 50,
                model_cost: Some(9.00),
                ..Default::default()
            }),
        );
        seed_cost_db(
            &cli_dir,
            fork_id,
            2,
            encode_gen_blob(GenBlobSpec {
                origin_cascade_id: Some(fork_id),
                model_enum: 10,
                resp_model: Some("m-fork"),
                input_tok: 100,
                output_tok: 50,
                model_cost: Some(0.25),
                ..Default::default()
            }),
        );

        let est = estimate_conversation_cost_usd(&cli_dir, fork_id, &pricing).unwrap();
        assert_eq!(est.usd, Some(0.25));
        assert_eq!(est.subagent_usd, None);
        assert_eq!(est.unpriced_turns, 0);
    }

    #[cfg(unix)]
    #[test]
    fn conv_cost_cache_skips_writes_into_symlinked_cache_dir_or_file() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let real_cache = dir.path().join("real_cache");
        fs::create_dir_all(&real_cache).unwrap();
        let sym_cache = dir.path().join("sym_cache");
        symlink(&real_cache, &sym_cache).unwrap();

        let entry = ConvCostCacheEntry {
            cli_dir: "/isolated/cli".to_string(),
            conversation_id: "11111111-1111-4111-8111-111111111111".to_string(),
            parent_fp: None,
            summaries_fp: None,
            descendants: Vec::new(),
            pricing_fp: PricingFileFingerprint::default(),
            cost: ConversationCost {
                usd: Some(1.0),
                subagent_usd: None,
                unpriced_turns: 0,
            },
        };

        write_conv_cost_cache_entry_with_limit(&sym_cache, entry.clone(), 4);
        assert!(!real_cache.join(CONV_COST_CACHE_FILE).exists());

        let sentinel = dir.path().join("sentinel.json");
        fs::write(&sentinel, b"sentinel").unwrap();
        symlink(&sentinel, real_cache.join(CONV_COST_CACHE_FILE)).unwrap();

        write_conv_cost_cache_entry_with_limit(&real_cache, entry, 4);
        assert_eq!(fs::read(&sentinel).unwrap(), b"sentinel");
    }
}
