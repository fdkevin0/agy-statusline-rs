#![forbid(unsafe_code)]
//! Real-SQLite integration tests for Phase 1B optimizations:
//! - Zero-lock 100-byte SQLite header readiness check (0-byte, journal, uninitialized, WAL, non-WAL)
//! - Fork / side-question `origin_cascade_id` exclusion across statusline, cost, single-conv, and global stats
//! - `RetryAttempt` (`429`/`503`/5xx/timeout) and `step_type` (`17`/`23`) decoding
//! - Persistent global per-DB scan cache (`stats_scan_cache.json`) hit, dynamic pricing, WAL invalidation, and pruning

mod common;

use std::fs;
use std::path::Path;

use agy_statusline::agy::db::{is_sqlite_db_ready_for_read, SpeedReader, SqliteSpeedReader};
use agy_statusline::agy::protobuf::{
    classify_retry_attempt, extract_gen_metadata_record, extract_step_metadata_record,
    RetryStatusClass,
};
use agy_statusline::config::layout::{LayoutConfig, ThemeStyle, WidgetId};
use agy_statusline::stats::{ConversationStatsReport, StatsReport};
use agy_statusline::statusline::git::{GitInfo, GitProbe};
use agy_statusline::statusline::{
    process_statusline, PricingSource, SessionSource, StatuslineRuntime,
};
use common::{isolated_agy_cmd, unix_now_secs};
use prost::Message;
use rusqlite::Connection;

struct NoopGit;
impl GitProbe for NoopGit {
    fn probe(&self, _dir: &Path, _include_diff: bool) -> GitInfo {
        GitInfo::default()
    }
}

#[derive(Clone, PartialEq, Message)]
struct TestGenBlob {
    #[prost(message, optional, tag = "1")]
    chat_model: Option<TestChatModel>,
    #[prost(uint64, repeated, tag = "2")]
    step_indices: Vec<u64>,
}

#[derive(Clone, PartialEq, Message)]
struct TestRetryAttempt {
    #[prost(string, optional, tag = "3")]
    error: Option<String>,
    #[prost(uint32, tag = "5")]
    reason_code: u32,
    #[prost(message, optional, tag = "7")]
    retry_delay: Option<TestDuration>,
}

#[derive(Clone, PartialEq, Message)]
struct TestMapEntry {
    #[prost(string, tag = "1")]
    key: String,
    #[prost(string, tag = "2")]
    value: String,
}

#[derive(Clone, PartialEq, Message)]
struct TestChatModel {
    #[prost(uint64, tag = "3")]
    model: u64,
    #[prost(message, optional, tag = "4")]
    usage: Option<TestUsage>,
    #[prost(float, optional, tag = "5")]
    model_cost: Option<f32>,
    #[prost(message, optional, tag = "9")]
    chat_start_metadata: Option<TestChatStart>,
    #[prost(message, optional, tag = "11")]
    time_to_first_token: Option<TestDuration>,
    #[prost(message, optional, tag = "12")]
    streaming_duration: Option<TestDuration>,
    #[prost(message, repeated, tag = "17")]
    retries: Vec<TestRetryAttempt>,
    #[prost(string, optional, tag = "19")]
    response_model: Option<String>,
    #[prost(message, repeated, tag = "20")]
    extra_meta: Vec<TestMapEntry>,
    #[prost(string, optional, tag = "21")]
    model_display_name: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
struct TestUsage {
    #[prost(uint64, tag = "2")]
    input_tokens: u64,
    #[prost(uint64, tag = "3")]
    output_tokens: u64,
    #[prost(uint64, tag = "5")]
    cache_read_tokens: u64,
}

#[derive(Clone, PartialEq, Message)]
struct TestChatStart {
    #[prost(message, optional, tag = "10")]
    context_window_metadata: Option<TestContextWindow>,
}

#[derive(Clone, PartialEq, Message)]
struct TestContextWindow {
    #[prost(int64, tag = "1")]
    estimated_tokens_used: i64,
    #[prost(int64, tag = "4")]
    max_context_tokens: i64,
}

#[derive(Clone, PartialEq, Message)]
struct TestDuration {
    #[prost(int64, tag = "1")]
    seconds: i64,
    #[prost(int64, tag = "2")]
    nanos: i64,
}

#[derive(Clone, PartialEq, Message)]
struct TestTimestamp {
    #[prost(int64, tag = "1")]
    seconds: i64,
}

#[derive(Clone, PartialEq, Message)]
struct TestSourceStepInfo {
    #[prost(string, optional, tag = "4")]
    cascade_id: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
struct TestStepBlob {
    #[prost(message, optional, tag = "1")]
    created_at: Option<TestTimestamp>,
    #[prost(message, optional, tag = "20")]
    source_trajectory_step_info: Option<TestSourceStepInfo>,
    #[prost(message, repeated, tag = "28")]
    retries: Vec<TestRetryAttempt>,
}

fn encode_step(ts_secs: i64, origin_cid: Option<&str>, retries: Vec<TestRetryAttempt>) -> Vec<u8> {
    TestStepBlob {
        created_at: Some(TestTimestamp { seconds: ts_secs }),
        source_trajectory_step_info: origin_cid.map(|cid| TestSourceStepInfo {
            cascade_id: Some(cid.to_string()),
        }),
        retries,
    }
    .encode_to_vec()
}

#[allow(clippy::too_many_arguments)]
fn encode_gen(
    step_idx: u64,
    origin_cid: Option<&str>,
    resp_model: &str,
    input_tokens: u64,
    output_tokens: u64,
    dur_secs: i64,
    max_ctx: i64,
    model_cost: Option<f32>,
    retries: Vec<TestRetryAttempt>,
) -> Vec<u8> {
    let extra_meta = origin_cid
        .map(|cid| {
            vec![TestMapEntry {
                key: "cascade_id".to_string(),
                value: cid.to_string(),
            }]
        })
        .unwrap_or_default();
    TestGenBlob {
        chat_model: Some(TestChatModel {
            model: 1669,
            usage: Some(TestUsage {
                input_tokens,
                output_tokens,
                cache_read_tokens: 0,
            }),
            model_cost,
            chat_start_metadata: Some(TestChatStart {
                context_window_metadata: Some(TestContextWindow {
                    estimated_tokens_used: 0,
                    max_context_tokens: max_ctx,
                }),
            }),
            time_to_first_token: Some(TestDuration {
                seconds: 0,
                nanos: 500_000_000,
            }),
            streaming_duration: Some(TestDuration {
                seconds: dur_secs,
                nanos: 0,
            }),
            retries,
            response_model: Some(resp_model.to_string()),
            extra_meta,
            model_display_name: Some("Gemini 4 Hydrogen".to_string()),
        }),
        step_indices: vec![step_idx],
    }
    .encode_to_vec()
}

#[test]
fn header_readiness_rejects_uninitialized_and_journaled_dbs_while_accepting_wal_and_non_wal() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // 0-byte file -> rejected.
    let zero_db = root.join("zero.db");
    fs::write(&zero_db, b"").unwrap();
    assert!(!is_sqlite_db_ready_for_read(&zero_db));

    // Short (<100 bytes) file -> rejected.
    let short_db = root.join("short.db");
    fs::write(&short_db, b"SQLite format 3\0short").unwrap();
    assert!(!is_sqlite_db_ready_for_read(&short_db));

    // 100-byte non-SQLite magic -> rejected.
    let bad_magic = root.join("bad_magic.db");
    fs::write(&bad_magic, [0x42u8; 100]).unwrap();
    assert!(!is_sqlite_db_ready_for_read(&bad_magic));

    // Uninitialized header (valid magic, page_count=1, schema_cookie=0, no WAL) -> rejected.
    let mut uninit_hdr = [0u8; 100];
    uninit_hdr[0..16].copy_from_slice(b"SQLite format 3\0");
    uninit_hdr[18] = 1;
    uninit_hdr[19] = 1;
    uninit_hdr[28..32].copy_from_slice(&1u32.to_be_bytes());
    uninit_hdr[40..44].copy_from_slice(&0u32.to_be_bytes());
    let uninit_db = root.join("uninit.db");
    fs::write(&uninit_db, uninit_hdr).unwrap();
    assert!(!is_sqlite_db_ready_for_read(&uninit_db));

    // Uninitialized header + header-only 32-byte WAL -> still rejected (requires wal_size > 32).
    fs::write(root.join("uninit.db-wal"), [0u8; 32]).unwrap();
    assert!(!is_sqlite_db_ready_for_read(&uninit_db));

    // Uninitialized header + non-empty WAL frame (> 32 bytes) -> accepted.
    fs::write(root.join("uninit.db-wal"), [0u8; 64]).unwrap();
    assert!(is_sqlite_db_ready_for_read(&uninit_db));

    // Valid finalized non-WAL DB -> accepted; non-empty rollback journal -> rejected.
    let non_wal = root.join("non_wal.db");
    let conn = Connection::open(&non_wal).unwrap();
    conn.execute(
        "CREATE TABLE gen_metadata (idx INTEGER PRIMARY KEY, data BLOB)",
        [],
    )
    .unwrap();
    drop(conn);
    assert!(is_sqlite_db_ready_for_read(&non_wal));

    let journal = root.join("non_wal.db-journal");
    fs::write(&journal, b"active-rollback-journal").unwrap();
    assert!(!is_sqlite_db_ready_for_read(&non_wal));

    // Truncated 0-byte rollback journal is clean and accepted.
    fs::write(&journal, b"").unwrap();
    assert!(is_sqlite_db_ready_for_read(&non_wal));

    // Real live WAL-mode DB (connection kept open with uncommitted checkpoint frames in -wal).
    let wal_db = root.join("live_wal.db");
    let wal_conn = Connection::open(&wal_db).unwrap();
    wal_conn
        .execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE gen_metadata (idx INTEGER PRIMARY KEY, data BLOB);
             INSERT INTO gen_metadata (idx, data) VALUES (1, X'0102');",
        )
        .unwrap();
    assert!(is_sqlite_db_ready_for_read(&wal_db));
    drop(wal_conn);
    assert!(is_sqlite_db_ready_for_read(&wal_db));
}

#[cfg(unix)]
#[test]
fn header_readiness_rejects_symlinked_journal_and_db() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let valid_db = root.join("real.db");
    let conn = Connection::open(&valid_db).unwrap();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY)", [])
        .unwrap();
    drop(conn);
    assert!(is_sqlite_db_ready_for_read(&valid_db));

    let sym_db = root.join("link.db");
    symlink(&valid_db, &sym_db).unwrap();
    assert!(!is_sqlite_db_ready_for_read(&sym_db));

    let dummy = root.join("empty_target");
    fs::write(&dummy, b"").unwrap();
    symlink(&dummy, root.join("real.db-journal")).unwrap();
    assert!(!is_sqlite_db_ready_for_read(&valid_db));
}

#[test]
fn fork_origin_cascade_id_excluded_across_statusline_cost_single_conv_and_global_stats() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let cli_dir = home.join(".gemini/antigravity-cli");
    let conv_dir = cli_dir.join("conversations");
    fs::create_dir_all(&conv_dir).unwrap();

    let parent_id = "aaaa1111-1111-4111-8111-111111111111";
    let fork_id = "bbbb2222-2222-4222-8222-222222222222";
    let now_secs = unix_now_secs() as i64;

    // Fork DB contains cloned parent turn + own turn (parent DB does NOT exist on disk).
    let fork_db = conv_dir.join(format!("{fork_id}.db"));
    let conn = Connection::open(&fork_db).unwrap();
    conn.execute_batch(
        "CREATE TABLE steps (idx INTEGER PRIMARY KEY, step_type INTEGER, metadata BLOB);
         CREATE TABLE gen_metadata (idx INTEGER PRIMARY KEY, data BLOB);",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO steps (idx, step_type, metadata) VALUES (1, 17, ?1)",
        [encode_step(now_secs - 60, Some(parent_id), vec![])],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO steps (idx, step_type, metadata) VALUES (2, 15, ?1)",
        [encode_step(now_secs - 30, Some(fork_id), vec![])],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO gen_metadata (idx, data) VALUES (1, ?1)",
        [encode_gen(
            1,
            Some(parent_id),
            "gemini-4-hydrogen",
            50_000,
            500,
            1,
            900_000,
            Some(9.00),
            vec![],
        )],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO gen_metadata (idx, data) VALUES (2, ?1)",
        [encode_gen(
            2,
            Some(fork_id),
            "gemini-4-hydrogen",
            1_000,
            100,
            2,
            256_000,
            Some(0.25),
            vec![],
        )],
    )
    .unwrap();
    drop(conn);

    // 1. Speed/context reader excludes cloned parent row.
    let reader = SqliteSpeedReader::new(&cli_dir);
    let meta = reader.session_metadata(fork_id);
    assert_eq!(meta.speed, Some(50.0));
    assert_eq!(meta.max_context_tokens, Some(256_000));

    // 2. Statusline fallback session cost excludes cloned parent row ($0.25, NOT $9.25).
    let mut cfg = LayoutConfig::preset_addon();
    cfg.rows = vec![vec![WidgetId::Cost]];
    cfg.theme = ThemeStyle::Mono;
    let payload = format!(r#"{{"agent_state":"idle","conversation_id":"{fork_id}"}}"#);
    let cache_dir = home.join("cache/agy-statusline");
    fs::create_dir_all(&cache_dir).unwrap();
    let pricing = agy_statusline::stats::PricingCatalog::default();
    let runtime = StatuslineRuntime {
        session: SessionSource::LocalDir(&cli_dir),
        git_probe: &NoopGit,
        cache_dir: Some(&cache_dir),
        pricing: PricingSource::Catalog(&pricing),
        now_unix_secs: now_secs as u64,
    };
    let mut status_out = Vec::new();
    process_statusline(&payload, &runtime, &cfg, &mut status_out).unwrap();
    let status_text = String::from_utf8(status_out).unwrap();
    assert!(status_text.contains("$0.25"), "{status_text}");
    assert!(!status_text.contains("$9.25"), "{status_text}");

    // 3. Single-conversation stats excludes cloned parent row.
    let single_out = isolated_agy_cmd(home)
        .args(["stats", fork_id, "--json"])
        .output()
        .unwrap();
    assert!(single_out.status.success(), "{single_out:?}");
    let single_report: ConversationStatsReport =
        serde_json::from_slice(&single_out.stdout).unwrap();
    assert_eq!(single_report.totals.requests, 1);
    assert_eq!(single_report.totals.input_tokens, 1_000);
    assert!((single_report.totals.priced_usd - 0.25).abs() < 1e-6);

    // 4. Global stats excludes cloned parent row even though parent DB is absent.
    let global_out = isolated_agy_cmd(home)
        .args(["stats", "--json"])
        .output()
        .unwrap();
    assert!(global_out.status.success(), "{global_out:?}");
    let global_report: StatsReport = serde_json::from_slice(&global_out.stdout).unwrap();
    assert_eq!(global_report.totals.requests, 1);
    assert_eq!(global_report.totals.input_tokens, 1_000);
    assert!((global_report.totals.priced_usd - 0.25).abs() < 1e-6);
}

#[test]
fn retry_attempt_and_step_type_diagnostics_and_global_cache_hit_and_wal_invalidation() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let cli_dir = home.join(".gemini/antigravity-cli");
    let conv_dir = cli_dir.join("conversations");
    fs::create_dir_all(&conv_dir).unwrap();

    let conv_id = "cccc3333-3333-4333-8333-333333333333";
    let now_secs = unix_now_secs() as i64;
    let db_path = conv_dir.join(format!("{conv_id}.db"));

    // Verify protobuf decoding directly for RetryAttempt (429 / 503 / timeout) & origin_cascade_id.
    let gen_blob_1 = encode_gen(
        1,
        Some(conv_id),
        "gemini-4-hydrogen",
        1_000_000,
        200,
        2,
        512_000,
        None,
        vec![
            TestRetryAttempt {
                error: Some("HTTP 429 RESOURCE_EXHAUSTED".to_string()),
                reason_code: 429,
                retry_delay: Some(TestDuration {
                    seconds: 1,
                    nanos: 500_000_000,
                }),
            },
            TestRetryAttempt {
                error: Some("503 UNAVAILABLE overloaded".to_string()),
                reason_code: 0,
                retry_delay: Some(TestDuration {
                    seconds: 2,
                    nanos: 0,
                }),
            },
        ],
    );
    let decoded_gen = extract_gen_metadata_record(&gen_blob_1).unwrap();
    assert_eq!(decoded_gen.origin_cascade_id.as_deref(), Some(conv_id));
    assert_eq!(decoded_gen.retry_attempts.len(), 2);
    assert_eq!(
        decoded_gen.retry_attempts[0].status_class,
        RetryStatusClass::RateLimit429
    );
    assert_eq!(decoded_gen.retry_attempts[0].status_code, Some(429));
    assert_eq!(
        decoded_gen.retry_attempts[1].status_class,
        RetryStatusClass::Capacity503
    );
    assert_eq!(decoded_gen.retry_attempts[1].status_code, Some(503));
    assert_eq!(
        classify_retry_attempt(Some("stream idle timeout"), 0),
        (RetryStatusClass::Timeout, None)
    );

    let step_blob_err = encode_step(now_secs - 60, Some(conv_id), vec![]);
    let decoded_step = extract_step_metadata_record(&step_blob_err).unwrap();
    assert_eq!(decoded_step.origin_cascade_id.as_deref(), Some(conv_id));

    // Seed live WAL-mode DB with step_type 17 (error) and 23 (compaction checkpoint).
    let conn = Connection::open(&db_path).unwrap();
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         CREATE TABLE steps (idx INTEGER PRIMARY KEY, step_type INTEGER, metadata BLOB);
         CREATE TABLE gen_metadata (idx INTEGER PRIMARY KEY, data BLOB);",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO steps (idx, step_type, metadata) VALUES (1, 17, ?1)",
        [step_blob_err],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO steps (idx, step_type, metadata) VALUES (2, 23, ?1)",
        [encode_step(now_secs - 40, Some(conv_id), vec![])],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO gen_metadata (idx, data) VALUES (1, ?1)",
        [gen_blob_1],
    )
    .unwrap();

    // Seed initial pricing catalog: $1.00 / Mtok input.
    let app_cache = home.join("cache/agy-statusline");
    fs::create_dir_all(&app_cache).unwrap();
    let pricing_file = app_cache.join("models_dev_pricing.json");
    fs::write(
        &pricing_file,
        format!(
            r#"{{"fetched_at":{now_secs},"models":{{"gemini-4-hydrogen":{{"input_per_mtok":1.0,"output_per_mtok":0.0}}}}}}"#
        ),
    )
    .unwrap();

    // Run 1: populates stats_scan_cache.json with raw turns and diagnostics.
    let out1 = isolated_agy_cmd(home)
        .args(["stats", "--json"])
        .output()
        .unwrap();
    assert!(out1.status.success(), "{out1:?}");
    let rep1: StatsReport = serde_json::from_slice(&out1.stdout).unwrap();
    assert_eq!(rep1.totals.requests, 1);
    assert!((rep1.totals.priced_usd - 1.0).abs() < 1e-6);

    let cache_path = app_cache.join("stats_scan_cache.json");
    assert!(cache_path.is_file());
    let cache_json: serde_json::Value =
        serde_json::from_slice(&fs::read(&cache_path).unwrap()).unwrap();
    assert_eq!(cache_json["schema_version"], 1);
    let canonical_db_key = fs::canonicalize(&db_path)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let cached_diag = &cache_json["entries"][&canonical_db_key]["data"]["diagnostics"];
    assert_eq!(cached_diag["retry_count"], 2);
    assert_eq!(cached_diag["retries_429"], 1);
    assert_eq!(cached_diag["retries_503"], 1);
    assert_eq!(cached_diag["error_steps"], 1);
    assert_eq!(cached_diag["compaction_steps"], 1);
    assert!((cached_diag["total_retry_delay_secs"].as_f64().unwrap() - 3.5).abs() < 1e-6);

    // Run 2: change pricing to $3.00 / Mtok while DB is unchanged -> cache hits raw turns, dynamic pricing yields $3.00!
    fs::write(
        &pricing_file,
        format!(
            r#"{{"fetched_at":{now_secs},"models":{{"gemini-4-hydrogen":{{"input_per_mtok":3.0,"output_per_mtok":0.0}}}}}}"#
        ),
    )
    .unwrap();
    let out2 = isolated_agy_cmd(home)
        .args(["stats", "--json"])
        .output()
        .unwrap();
    assert!(out2.status.success(), "{out2:?}");
    let rep2: StatsReport = serde_json::from_slice(&out2.stdout).unwrap();
    assert_eq!(rep2.totals.requests, 1);
    assert!((rep2.totals.priced_usd - 3.0).abs() < 1e-6);

    // Run 3: WAL mutation on open writer connection invalidates cache and picks up turn 2 ($6.00 total).
    conn.execute(
        "INSERT INTO gen_metadata (idx, data) VALUES (2, ?1)",
        [encode_gen(
            1,
            Some(conv_id),
            "gemini-4-hydrogen",
            1_000_000,
            200,
            2,
            512_000,
            None,
            vec![],
        )],
    )
    .unwrap();
    let out3 = isolated_agy_cmd(home)
        .args(["stats", "--json"])
        .output()
        .unwrap();
    assert!(out3.status.success(), "{out3:?}");
    let rep3: StatsReport = serde_json::from_slice(&out3.stdout).unwrap();
    assert_eq!(rep3.totals.requests, 2);
    assert!((rep3.totals.priced_usd - 6.0).abs() < 1e-6);

    // Run 4: deleting DB prunes its entry from stats_scan_cache.json.
    drop(conn);
    fs::remove_file(&db_path).unwrap();
    let out4 = isolated_agy_cmd(home)
        .args(["stats", "--json"])
        .output()
        .unwrap();
    assert!(out4.status.success(), "{out4:?}");
    let rep4: StatsReport = serde_json::from_slice(&out4.stdout).unwrap();
    assert_eq!(rep4.totals.requests, 0);
    let pruned_json: serde_json::Value =
        serde_json::from_slice(&fs::read(&cache_path).unwrap()).unwrap();
    assert!(pruned_json["entries"][&canonical_db_key].is_null());
}
