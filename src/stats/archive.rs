//! Persistent local SQLite archive for `agy-statusline stats`.
//!
//! Antigravity CLI (`agy`) prunes `conversations/<uuid>.db` and `brain/<uuid>`
//! to the 500 most-recent sessions on startup. This module incrementally mirrors
//! normalized per-turn usage facts into `$XDG_DATA_HOME/agy-statusline/stats_history.db`
//! so pruned history remains intact for all time windows, timezones, TUI filters,
//! and retroactive `models.dev` repricing.

use std::collections::{HashMap, HashSet};
use std::env;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use rusqlite::{params, Connection, OpenFlags, TransactionBehavior};

use super::pricing::PricingCatalog;
use super::query::resolve_roots;
use super::scan::{
    discover_sqlite_db_targets, project_sqlite_conversations, root_path_key,
    scan_sqlite_targets_raw, DbFileFingerprint, DbTarget, RawConversationScan, RawTurn, UsageEvent,
};
use super::tz::TimeZoneSpec;
use super::SqliteStats;
use crate::paths::{xdg_cache_dir, xdg_data_dir};

pub(super) const ARCHIVE_DB_FILENAME: &str = "stats_history.db";
pub(super) const SYNC_STAMP_FILENAME: &str = "stats_archive_sync.ts";
pub(super) const DEFAULT_ARCHIVE_SYNC_INTERVAL_SECS: u64 = 1_800;

const ARCHIVE_INIT_LOCK_FILENAME: &str = "stats_history.lock";
const ARCHIVE_SCHEMA_VERSION: i32 = 1;
const ARCHIVE_BUSY_TIMEOUT_MS: u64 = 5_000;

/// Summary of an incremental archive sync run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ArchiveSyncSummary {
    pub(super) archive_path: PathBuf,
    pub(super) live_db_files: u64,
    pub(super) scanned_db_files: u64,
    pub(super) total_db_files: u64,
    pub(super) archived_db_files: u64,
}

fn deduplicate_roots(roots: &[(&'static str, PathBuf)]) -> Vec<(&'static str, String)> {
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(roots.len());
    for (source, root) in roots {
        let key = root_path_key(root);
        if seen.insert(key.clone()) {
            out.push((*source, key));
        }
    }
    out
}

fn acquire_init_lock(parent: &Path) -> io::Result<File> {
    fs::create_dir_all(parent)?;
    let lock = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(parent.join(ARCHIVE_INIT_LOCK_FILENAME))?;
    lock.lock()?;
    Ok(lock)
}

fn read_user_version(conn: &Connection) -> rusqlite::Result<i32> {
    conn.query_row("PRAGMA user_version", [], |row| row.get(0))
}

fn read_journal_mode(conn: &Connection) -> rusqlite::Result<String> {
    conn.query_row("PRAGMA journal_mode", [], |row| row.get(0))
}

fn configure_connection_pragmas(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;",
    )
}

fn open_archive_db(db_path: &Path) -> rusqlite::Result<Option<Connection>> {
    // Fast path for an existing archive DB: inspect `user_version` and `journal_mode`
    // read-only first so newer-schema files are never mutated and steady-state runs avoid
    // touching the init lock or re-running `PRAGMA journal_mode = WAL`.
    if db_path.is_file() {
        let conn = Connection::open_with_flags(
            db_path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(Duration::from_millis(ARCHIVE_BUSY_TIMEOUT_MS))?;
        let version = read_user_version(&conn)?;
        if version > ARCHIVE_SCHEMA_VERSION {
            // Never mutate or overwrite an archive created by a newer schema version.
            return Ok(None);
        }
        if version == ARCHIVE_SCHEMA_VERSION
            && read_journal_mode(&conn)?.eq_ignore_ascii_case("wal")
        {
            configure_connection_pragmas(&conn)?;
            return Ok(Some(conn));
        }
        // Release the connection before taking the OS init lock so WAL upgrade/migration
        // cannot deadlock with another initializer.
        drop(conn);
    }

    // Serialize cold creation, WAL upgrade, and schema migration across concurrent processes
    // so parallel cold starts never hit SQLite's lock-upgrade SQLITE_BUSY deadlock on
    // `PRAGMA journal_mode = WAL`.
    let Some(parent) = db_path.parent() else {
        return Ok(None);
    };
    let Ok(_init_lock) = acquire_init_lock(parent) else {
        return Ok(None);
    };

    let mut conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(Duration::from_millis(ARCHIVE_BUSY_TIMEOUT_MS))?;

    let version = read_user_version(&conn)?;
    if version > ARCHIVE_SCHEMA_VERSION {
        return Ok(None);
    }
    if !read_journal_mode(&conn)?.eq_ignore_ascii_case("wal") {
        conn.execute_batch("PRAGMA journal_mode = WAL;")?;
    }
    configure_connection_pragmas(&conn)?;

    if version < ARCHIVE_SCHEMA_VERSION {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS archived_conversations (
                root_path        TEXT NOT NULL,
                uuid             TEXT NOT NULL,
                source           TEXT NOT NULL,
                db_mtime_secs    INTEGER NOT NULL,
                db_mtime_nanos   INTEGER NOT NULL,
                db_size          INTEGER NOT NULL,
                wal_mtime_secs   INTEGER NOT NULL,
                wal_mtime_nanos  INTEGER NOT NULL,
                wal_size         INTEGER NOT NULL,
                synced_at_secs   INTEGER NOT NULL,
                PRIMARY KEY (root_path, uuid)
            ) WITHOUT ROWID;

            CREATE TABLE IF NOT EXISTS archived_turns (
                root_path          TEXT NOT NULL,
                uuid               TEXT NOT NULL,
                turn_idx           INTEGER NOT NULL,
                ts_secs            INTEGER,
                model_enum         INTEGER NOT NULL,
                response_model     TEXT,
                model_display_name TEXT,
                input_tokens       INTEGER NOT NULL,
                cache_read_tokens  INTEGER NOT NULL,
                cache_write_tokens INTEGER NOT NULL,
                output_tokens      INTEGER NOT NULL,
                thinking_tokens    INTEGER NOT NULL,
                native_cost_usd    REAL,
                decode_speed       REAL,
                ttft_secs          REAL,
                has_billable_usage INTEGER NOT NULL,
                PRIMARY KEY (root_path, uuid, turn_idx),
                FOREIGN KEY (root_path, uuid)
                    REFERENCES archived_conversations (root_path, uuid)
                    ON DELETE CASCADE
            ) WITHOUT ROWID;

            PRAGMA user_version = 1;",
        )?;
        tx.commit()?;
    }

    Ok(Some(conn))
}

fn load_existing_fingerprints(
    conn: &Connection,
    unique_roots: &[(&'static str, String)],
) -> rusqlite::Result<HashMap<(String, String), DbFileFingerprint>> {
    let mut out = HashMap::new();
    let mut stmt = conn.prepare(
        "SELECT uuid, db_mtime_secs, db_mtime_nanos, db_size,
                wal_mtime_secs, wal_mtime_nanos, wal_size
         FROM archived_conversations
         WHERE root_path = ?1",
    )?;
    for (_, root_key) in unique_roots {
        let mut rows = stmt.query([root_key.as_str()])?;
        while let Some(row) = rows.next()? {
            let uuid: String = row.get(0)?;
            let fp = DbFileFingerprint {
                db_mtime_secs: row.get(1)?,
                db_mtime_nanos: row.get::<_, i64>(2)?.max(0) as u32,
                db_size: row.get::<_, i64>(3)?.max(0) as u64,
                wal_mtime_secs: row.get(4)?,
                wal_mtime_nanos: row.get::<_, i64>(5)?.max(0) as u32,
                wal_size: row.get::<_, i64>(6)?.max(0) as u64,
            };
            out.insert((root_key.clone(), uuid), fp);
        }
    }
    Ok(out)
}

fn upsert_scanned_conversations(
    conn: &mut Connection,
    scanned: &[RawConversationScan],
    now_unix_secs: u64,
) -> rusqlite::Result<()> {
    if scanned.is_empty() {
        return Ok(());
    }
    let synced_at = now_unix_secs.min(i64::MAX as u64) as i64;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    {
        let mut upsert_conv = tx.prepare(
            "INSERT INTO archived_conversations (
                root_path, uuid, source,
                db_mtime_secs, db_mtime_nanos, db_size,
                wal_mtime_secs, wal_mtime_nanos, wal_size,
                synced_at_secs
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(root_path, uuid) DO UPDATE SET
                source          = excluded.source,
                db_mtime_secs   = excluded.db_mtime_secs,
                db_mtime_nanos  = excluded.db_mtime_nanos,
                db_size         = excluded.db_size,
                wal_mtime_secs  = excluded.wal_mtime_secs,
                wal_mtime_nanos = excluded.wal_mtime_nanos,
                wal_size        = excluded.wal_size,
                synced_at_secs  = excluded.synced_at_secs",
        )?;
        let mut delete_turns =
            tx.prepare("DELETE FROM archived_turns WHERE root_path = ?1 AND uuid = ?2")?;
        let mut insert_turn = tx.prepare(
            "INSERT INTO archived_turns (
                root_path, uuid, turn_idx, ts_secs,
                model_enum, response_model, model_display_name,
                input_tokens, cache_read_tokens, cache_write_tokens,
                output_tokens, thinking_tokens,
                native_cost_usd, decode_speed, ttft_secs, has_billable_usage
             ) VALUES (
                ?1, ?2, ?3, ?4,
                ?5, ?6, ?7,
                ?8, ?9, ?10,
                ?11, ?12,
                ?13, ?14, ?15, ?16
             )",
        )?;

        for conv in scanned {
            let fp = conv.fingerprint;
            upsert_conv.execute(params![
                conv.root_key,
                conv.uuid,
                conv.source,
                fp.db_mtime_secs,
                i64::from(fp.db_mtime_nanos),
                fp.db_size.min(i64::MAX as u64) as i64,
                fp.wal_mtime_secs,
                i64::from(fp.wal_mtime_nanos),
                fp.wal_size.min(i64::MAX as u64) as i64,
                synced_at,
            ])?;
            delete_turns.execute(params![conv.root_key, conv.uuid])?;
            for (idx, turn) in conv.turns.iter().enumerate() {
                insert_turn.execute(params![
                    conv.root_key,
                    conv.uuid,
                    idx as i64,
                    turn.ts_secs,
                    turn.model_enum.min(i64::MAX as u64) as i64,
                    turn.response_model.as_deref(),
                    turn.model_display_name.as_deref(),
                    turn.input_tokens.min(i64::MAX as u64) as i64,
                    turn.cache_read_tokens.min(i64::MAX as u64) as i64,
                    turn.cache_write_tokens.min(i64::MAX as u64) as i64,
                    turn.output_tokens.min(i64::MAX as u64) as i64,
                    turn.thinking_tokens.min(i64::MAX as u64) as i64,
                    turn.native_cost_usd,
                    turn.decode_speed,
                    turn.ttft_secs,
                    i64::from(turn.has_billable_usage),
                ])?;
            }
        }
    }
    tx.commit()
}

fn sync_changed_targets(
    conn: &mut Connection,
    unique_roots: &[(&'static str, String)],
    targets: &[DbTarget],
    now_unix_secs: u64,
) -> rusqlite::Result<u64> {
    let existing = load_existing_fingerprints(conn, unique_roots)?;
    let changed_targets: Vec<_> = targets
        .iter()
        .filter(|t| {
            existing
                .get(&(t.root_key.clone(), t.uuid.clone()))
                .is_none_or(|prev| *prev != t.fingerprint)
        })
        .cloned()
        .collect();
    let scanned = scan_sqlite_targets_raw(&changed_targets);
    upsert_scanned_conversations(conn, &scanned, now_unix_secs)?;
    Ok(scanned.len() as u64)
}

struct StoredConversation {
    source: &'static str,
    uuid: String,
    turns: Vec<RawTurn>,
}

fn load_stored_conversations(
    conn: &Connection,
    unique_roots: &[(&'static str, String)],
    live_keys: &HashSet<(&str, &str)>,
) -> rusqlite::Result<(Vec<StoredConversation>, u64)> {
    let mut conversations = Vec::new();
    let mut archived_db_files = 0u64;

    let mut conv_stmt = conn.prepare(
        "SELECT uuid FROM archived_conversations WHERE root_path = ?1 ORDER BY uuid ASC",
    )?;
    let mut turn_stmt = conn.prepare(
        "SELECT uuid, ts_secs, model_enum, response_model, model_display_name,
                input_tokens, cache_read_tokens, cache_write_tokens,
                output_tokens, thinking_tokens,
                native_cost_usd, decode_speed, ttft_secs, has_billable_usage
         FROM archived_turns
         WHERE root_path = ?1
         ORDER BY uuid ASC, turn_idx ASC",
    )?;

    for (source, root_key) in unique_roots {
        let mut turns_by_uuid: HashMap<String, Vec<RawTurn>> = HashMap::new();
        let mut turn_rows = turn_stmt.query([root_key.as_str()])?;
        while let Some(row) = turn_rows.next()? {
            let uuid: String = row.get(0)?;
            let turn = RawTurn {
                ts_secs: row.get(1)?,
                model_enum: row.get::<_, i64>(2)?.max(0) as u64,
                response_model: row.get(3)?,
                model_display_name: row.get(4)?,
                input_tokens: row.get::<_, i64>(5)?.max(0) as u64,
                cache_read_tokens: row.get::<_, i64>(6)?.max(0) as u64,
                cache_write_tokens: row.get::<_, i64>(7)?.max(0) as u64,
                output_tokens: row.get::<_, i64>(8)?.max(0) as u64,
                thinking_tokens: row.get::<_, i64>(9)?.max(0) as u64,
                native_cost_usd: row.get(10)?,
                decode_speed: row.get(11)?,
                ttft_secs: row.get(12)?,
                has_billable_usage: row.get::<_, i64>(13)? != 0,
            };
            turns_by_uuid.entry(uuid).or_default().push(turn);
        }

        let mut conv_rows = conv_stmt.query([root_key.as_str()])?;
        while let Some(row) = conv_rows.next()? {
            let uuid: String = row.get(0)?;
            if !live_keys.contains(&(root_key.as_str(), uuid.as_str())) {
                archived_db_files += 1;
            }
            let turns = turns_by_uuid.remove(&uuid).unwrap_or_default();
            conversations.push(StoredConversation {
                source,
                uuid,
                turns,
            });
        }
    }

    Ok((conversations, archived_db_files))
}

fn count_stored_conversations(
    conn: &Connection,
    unique_roots: &[(&'static str, String)],
    live_keys: &HashSet<(&str, &str)>,
) -> rusqlite::Result<(u64, u64)> {
    let mut total = 0u64;
    let mut archived = 0u64;
    let mut stmt = conn.prepare("SELECT uuid FROM archived_conversations WHERE root_path = ?1")?;
    for (_, root_key) in unique_roots {
        let mut rows = stmt.query([root_key.as_str()])?;
        while let Some(row) = rows.next()? {
            let uuid: String = row.get(0)?;
            total += 1;
            if !live_keys.contains(&(root_key.as_str(), uuid.as_str())) {
                archived += 1;
            }
        }
    }
    Ok((total, archived))
}

pub(super) fn is_sync_fresh(cache_dir: &Path, now_unix_secs: u64, interval_secs: u64) -> bool {
    let Ok(raw) = fs::read_to_string(cache_dir.join(SYNC_STAMP_FILENAME)) else {
        return false;
    };
    let Ok(ts) = raw.trim().parse::<u64>() else {
        return false;
    };
    now_unix_secs.saturating_sub(ts) < interval_secs
}

pub(super) fn record_sync_stamp(cache_dir: Option<&Path>, now_unix_secs: u64) -> bool {
    let Some(dir) = cache_dir else {
        return false;
    };
    fs::create_dir_all(dir).is_ok()
        && fs::write(dir.join(SYNC_STAMP_FILENAME), format!("{now_unix_secs}\n")).is_ok()
}

fn project_live_targets(
    targets: &[DbTarget],
    tz: &TimeZoneSpec,
    pricing: &PricingCatalog,
) -> (
    SqliteStats,
    Vec<UsageEvent>,
    HashSet<(&'static str, String)>,
) {
    let scanned = scan_sqlite_targets_raw(targets);
    project_sqlite_conversations(
        scanned
            .iter()
            .map(|c| (c.source, c.uuid.as_str(), c.turns.as_slice())),
        0,
        tz,
        pricing,
    )
}

/// Incrementally syncs live SQLite conversation DBs into `data_dir/stats_history.db`
/// and projects all live + archived conversations through `tz` and `pricing`.
///
/// Falls back transparently to a live-only scan when the archive database cannot be
/// opened or updated.
pub(super) fn scan_sqlite_conversations_with_archive(
    roots: &[(&'static str, PathBuf)],
    data_dir: &Path,
    cache_dir: Option<&Path>,
    now_unix_secs: u64,
    tz: &TimeZoneSpec,
    pricing: &PricingCatalog,
) -> (
    SqliteStats,
    Vec<UsageEvent>,
    HashSet<(&'static str, String)>,
) {
    let archive_path = data_dir.join(ARCHIVE_DB_FILENAME);
    let unique_roots = deduplicate_roots(roots);
    let targets = discover_sqlite_db_targets(roots);

    // Avoid creating an empty archive DB on a fresh home with no conversations.
    if targets.is_empty() && !archive_path.exists() {
        return (SqliteStats::default(), Vec::new(), HashSet::new());
    }

    let Ok(Some(mut conn)) = open_archive_db(&archive_path) else {
        return project_live_targets(&targets, tz, pricing);
    };
    if sync_changed_targets(&mut conn, &unique_roots, &targets, now_unix_secs).is_err() {
        return project_live_targets(&targets, tz, pricing);
    }

    let live_keys: HashSet<(&str, &str)> = targets
        .iter()
        .map(|t| (t.root_key.as_str(), t.uuid.as_str()))
        .collect();
    let Ok((stored, archived_db_files)) =
        load_stored_conversations(&conn, &unique_roots, &live_keys)
    else {
        return project_live_targets(&targets, tz, pricing);
    };

    record_sync_stamp(cache_dir, now_unix_secs);
    project_sqlite_conversations(
        stored
            .iter()
            .map(|c| (c.source, c.uuid.as_str(), c.turns.as_slice())),
        archived_db_files,
        tz,
        pricing,
    )
}

/// Runs an incremental archive sync against `roots` and returns sync counts.
pub(super) fn sync_roots_into_archive(
    roots: &[(&'static str, PathBuf)],
    data_dir: &Path,
    cache_dir: Option<&Path>,
    now_unix_secs: u64,
) -> io::Result<ArchiveSyncSummary> {
    let archive_path = data_dir.join(ARCHIVE_DB_FILENAME);
    let unique_roots = deduplicate_roots(roots);
    let targets = discover_sqlite_db_targets(roots);

    if targets.is_empty() && !archive_path.exists() {
        record_sync_stamp(cache_dir, now_unix_secs);
        return Ok(ArchiveSyncSummary {
            archive_path,
            live_db_files: 0,
            scanned_db_files: 0,
            total_db_files: 0,
            archived_db_files: 0,
        });
    }

    let mut conn = open_archive_db(&archive_path)
        .map_err(|e| io::Error::other(format!("failed to open stats archive: {e}")))?
        .ok_or_else(|| {
            io::Error::other(format!(
                "stats archive at {} uses an unsupported newer schema or unwritable directory",
                archive_path.display()
            ))
        })?;
    let scanned_db_files = sync_changed_targets(&mut conn, &unique_roots, &targets, now_unix_secs)
        .map_err(|e| io::Error::other(format!("failed to update stats archive: {e}")))?;

    let live_keys: HashSet<(&str, &str)> = targets
        .iter()
        .map(|t| (t.root_key.as_str(), t.uuid.as_str()))
        .collect();
    let (total_db_files, archived_db_files) =
        count_stored_conversations(&conn, &unique_roots, &live_keys)
            .map_err(|e| io::Error::other(format!("failed to count archived DBs: {e}")))?;

    record_sync_stamp(cache_dir, now_unix_secs);
    Ok(ArchiveSyncSummary {
        archive_path,
        live_db_files: targets.len() as u64,
        scanned_db_files,
        total_db_files,
        archived_db_files,
    })
}

/// Runs `agy-statusline stats sync [--if-stale]`.
pub fn run_stats_sync_command<W: Write>(
    home: Option<&Path>,
    if_stale: bool,
    now_unix_secs: u64,
    out: &mut W,
) -> io::Result<()> {
    let roots = resolve_roots(home);
    let data_dir = xdg_data_dir()
        .or_else(|| home.map(|h| h.join(".local").join("share").join("agy-statusline")))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "unable to resolve stats archive data directory",
            )
        })?;
    let cache_dir =
        xdg_cache_dir().or_else(|| home.map(|h| h.join(".cache").join("agy-statusline")));

    if if_stale
        && cache_dir
            .as_deref()
            .is_some_and(|dir| is_sync_fresh(dir, now_unix_secs, DEFAULT_ARCHIVE_SYNC_INTERVAL_SECS))
    {
        writeln!(
            out,
            "Stats archive is fresh (interval {}m): {}",
            DEFAULT_ARCHIVE_SYNC_INTERVAL_SECS / 60,
            data_dir.join(ARCHIVE_DB_FILENAME).display()
        )?;
        return Ok(());
    }

    let summary = sync_roots_into_archive(&roots, &data_dir, cache_dir.as_deref(), now_unix_secs)?;
    writeln!(
        out,
        "Synced {} updated / {} live DBs ({} total, {} archived) -> {}",
        summary.scanned_db_files,
        summary.live_db_files,
        summary.total_db_files,
        summary.archived_db_files,
        summary.archive_path.display()
    )?;
    Ok(())
}

/// Spawns a detached background `agy-statusline stats sync` process when any local
/// `conversations/` directory exists and the sync stamp is missing or past the interval.
pub fn maybe_spawn_background_archive_sync(
    home: Option<&Path>,
    cache_dir: Option<&Path>,
    now_unix_secs: u64,
) {
    let Some(dir) = cache_dir else {
        return;
    };
    if is_sync_fresh(dir, now_unix_secs, DEFAULT_ARCHIVE_SYNC_INTERVAL_SECS) {
        return;
    }
    let roots = resolve_roots(home);
    if !roots.iter().any(|(_, r)| r.join("conversations").is_dir()) {
        return;
    }
    // Skip spawning if the throttle stamp cannot be persisted so an unwritable cache dir
    // never triggers a background process on every statusline refresh.
    if !record_sync_stamp(Some(dir), now_unix_secs) {
        return;
    }
    let Ok(exe) = env::current_exe() else {
        return;
    };
    let _ = Command::new(exe)
        .args(["stats", "sync"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protobuf::{encode_gen_blob, encode_step_meta_blob, GenBlobSpec};
    use crate::stats::pricing::{PricingCatalog, SyncMode, DEFAULT_PRICING_TTL_SECS};

    fn create_live_db(db_path: &Path) -> Connection {
        fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let conn = Connection::open(db_path).unwrap();
        conn.execute(
            "CREATE TABLE steps (idx INTEGER PRIMARY KEY, metadata BLOB)",
            [],
        )
        .unwrap();
        conn.execute(
            "CREATE TABLE gen_metadata (idx INTEGER PRIMARY KEY, data BLOB)",
            [],
        )
        .unwrap();
        conn
    }

    fn insert_turn(conn: &Connection, idx: i64, ts_secs: u64, model: &str, input_tok: u64) {
        conn.execute(
            "INSERT INTO steps (idx, metadata) VALUES (?1, ?2)",
            params![idx, encode_step_meta_blob(ts_secs as i64)],
        )
        .unwrap();
        let blob = encode_gen_blob(GenBlobSpec {
            step_indices: vec![idx as u64],
            model_enum: 1526,
            resp_model: Some(model),
            disp_name: Some(model),
            input_tok,
            output_tok: 200,
            thinking_tok: 50,
            dur_secs: 2,
            ttft_nanos: 400_000_000,
            ..Default::default()
        });
        conn.execute(
            "INSERT INTO gen_metadata (idx, data) VALUES (?1, ?2)",
            params![idx, blob],
        )
        .unwrap();
    }

    #[test]
    fn archive_preserves_turns_and_medians_after_live_db_is_pruned() {
        let dir = tempfile::tempdir().unwrap();
        let cli_root = dir.path().join("cli");
        let data_dir = dir.path().join("data");
        let cache_dir = dir.path().join("cache");
        let u1 = "11111111-1111-1111-1111-111111111111";
        let u2 = "22222222-2222-2222-2222-222222222222";
        let db1 = cli_root.join("conversations").join(format!("{u1}.db"));
        let db2 = cli_root.join("conversations").join(format!("{u2}.db"));

        let c1 = create_live_db(&db1);
        insert_turn(&c1, 1, 1_795_000_000, "model-alpha", 1_000);
        drop(c1);

        let c2 = create_live_db(&db2);
        insert_turn(&c2, 1, 1_795_100_000, "model-beta", 2_000);
        drop(c2);

        let roots = [("cli", cli_root.clone())];
        let tz = TimeZoneSpec::utc();
        let pricing = PricingCatalog::default();

        let (stats1, events1, covered1) = scan_sqlite_conversations_with_archive(
            &roots,
            &data_dir,
            Some(&cache_dir),
            1_795_200_000,
            &tz,
            &pricing,
        );
        assert_eq!(stats1.db_files, 2);
        assert_eq!(stats1.archived_db_files, 0);
        assert_eq!(stats1.completed_turns, 2);
        assert_eq!(events1.len(), 2);
        assert!(covered1.contains(&("cli", u1.to_string())));

        // Simulate AGY pruning the oldest conversation DB on startup.
        fs::remove_file(&db1).unwrap();

        let (stats2, events2, covered2) = scan_sqlite_conversations_with_archive(
            &roots,
            &data_dir,
            Some(&cache_dir),
            1_795_200_100,
            &tz,
            &pricing,
        );
        assert_eq!(stats2.db_files, 2);
        assert_eq!(stats2.archived_db_files, 1);
        assert_eq!(stats2.completed_turns, 2);
        assert_eq!(stats2.median_tok_per_sec, stats1.median_tok_per_sec);
        assert_eq!(stats2.median_ttft_secs, stats1.median_ttft_secs);
        assert_eq!(events2.len(), 2);
        assert!(covered2.contains(&("cli", u1.to_string())));
        assert!(covered2.contains(&("cli", u2.to_string())));
    }

    #[test]
    fn archive_detects_active_wal_writes_and_ignores_zero_byte_wal_touch() {
        let dir = tempfile::tempdir().unwrap();
        let cli_root = dir.path().join("cli");
        let data_dir = dir.path().join("data");
        let u1 = "33333333-3333-3333-3333-333333333333";
        let db1 = cli_root.join("conversations").join(format!("{u1}.db"));
        let wal1 = cli_root.join("conversations").join(format!("{u1}.db-wal"));

        let conn = create_live_db(&db1);
        conn.execute_batch("PRAGMA journal_mode = WAL;").unwrap();
        insert_turn(&conn, 1, 1_795_000_000, "model-alpha", 1_000);
        // Checkpoint and close so wal is 0 bytes / removed.
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
        drop(conn);

        let roots = [("cli", cli_root.clone())];
        let s1 = sync_roots_into_archive(&roots, &data_dir, None, 1_795_200_000).unwrap();
        assert_eq!(s1.live_db_files, 1);
        assert_eq!(s1.scanned_db_files, 1);

        // Idle second sync skips unchanged DB.
        let s2 = sync_roots_into_archive(&roots, &data_dir, None, 1_795_200_010).unwrap();
        assert_eq!(s2.scanned_db_files, 0);

        // Touching a 0-byte -wal sidecar (like AGY startup does) must not trigger a re-scan.
        fs::write(&wal1, b"").unwrap();
        let s3 = sync_roots_into_archive(&roots, &data_dir, None, 1_795_200_020).unwrap();
        assert_eq!(s3.scanned_db_files, 0);

        // Open in WAL mode with autocheckpoint disabled so turn 2 lives only in -wal.
        let live_conn = Connection::open(&db1).unwrap();
        live_conn
            .execute_batch("PRAGMA journal_mode = WAL; PRAGMA wal_autocheckpoint = 0;")
            .unwrap();
        insert_turn(&live_conn, 2, 1_795_000_060, "model-alpha", 3_000);

        let s4 = sync_roots_into_archive(&roots, &data_dir, None, 1_795_200_030).unwrap();
        assert_eq!(s4.scanned_db_files, 1);

        let (stats, events, _) = scan_sqlite_conversations_with_archive(
            &roots,
            &data_dir,
            None,
            1_795_200_040,
            &TimeZoneSpec::utc(),
            &PricingCatalog::default(),
        );
        assert_eq!(stats.completed_turns, 2);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].input_tokens + events[1].input_tokens, 4_000);
        drop(live_conn);
    }

    #[test]
    fn archive_isolates_distinct_root_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root_a = dir.path().join("agy-a");
        let root_b = dir.path().join("agy-b");
        let data_dir = dir.path().join("data");
        let u1 = "44444444-4444-4444-4444-444444444444";
        let u2 = "55555555-5555-5555-5555-555555555555";

        let ca = create_live_db(&root_a.join("conversations").join(format!("{u1}.db")));
        insert_turn(&ca, 1, 1_795_000_000, "model-a", 1_000);
        drop(ca);

        let cb = create_live_db(&root_b.join("conversations").join(format!("{u2}.db")));
        insert_turn(&cb, 1, 1_795_000_000, "model-b", 5_000);
        drop(cb);

        let tz = TimeZoneSpec::utc();
        let pricing = PricingCatalog::default();
        let (_, events_a, _) = scan_sqlite_conversations_with_archive(
            &[("cli", root_a.clone())],
            &data_dir,
            None,
            1_795_200_000,
            &tz,
            &pricing,
        );
        let (_, events_b, _) = scan_sqlite_conversations_with_archive(
            &[("cli", root_b.clone())],
            &data_dir,
            None,
            1_795_200_000,
            &tz,
            &pricing,
        );

        assert_eq!(events_a.len(), 1);
        assert_eq!(events_a[0].model_group, "model-a");
        assert_eq!(events_b.len(), 1);
        assert_eq!(events_b[0].model_group, "model-b");
    }

    #[test]
    fn archive_reprices_archived_turns_and_reprojects_timezones() {
        let dir = tempfile::tempdir().unwrap();
        let cli_root = dir.path().join("cli");
        let data_dir = dir.path().join("data");
        let cache_dir = dir.path().join("cache");
        let u1 = "66666666-6666-6666-6666-666666666666";
        let db1 = cli_root.join("conversations").join(format!("{u1}.db"));

        // 2026-11-18T22:00:00Z -> 2026-11-18 in UTC, 2026-11-19 in UTC+08:00.
        let ts_secs = 1_795_039_200;
        let c1 = create_live_db(&db1);
        insert_turn(&c1, 1, ts_secs, "model-reprice", 1_000_000);
        drop(c1);

        let roots = [("cli", cli_root.clone())];
        let (_, ev_unpriced, _) = scan_sqlite_conversations_with_archive(
            &roots,
            &data_dir,
            None,
            ts_secs + 60,
            &TimeZoneSpec::utc(),
            &PricingCatalog::default(),
        );
        assert_eq!(ev_unpriced.len(), 1);
        assert_eq!(ev_unpriced[0].local_date, "2026-11-18");
        assert_eq!(ev_unpriced[0].cost_usd, None);

        // Delete the original DB, then load with newly cached pricing and UTC+08:00.
        fs::remove_file(&db1).unwrap();
        fs::create_dir_all(&cache_dir).unwrap();
        fs::write(
            cache_dir.join("models_dev_pricing.json"),
            format!(
                r#"{{"fetched_at":{},"models":{{"model-reprice":{{"input_per_mtok":2.5}}}}}}"#,
                ts_secs + 120
            ),
        )
        .unwrap();
        let catalog = PricingCatalog::load_effective_with(
            Some(&cache_dir),
            SyncMode::Offline,
            ts_secs + 120,
            DEFAULT_PRICING_TTL_SECS,
            |_| unreachable!(),
        );
        let tz_plus8 = TimeZoneSpec::fixed_offset(8 * 3_600);
        let (stats, ev_repriced, _) = scan_sqlite_conversations_with_archive(
            &roots,
            &data_dir,
            None,
            ts_secs + 120,
            &tz_plus8,
            &catalog,
        );
        assert_eq!(stats.archived_db_files, 1);
        assert_eq!(ev_repriced.len(), 1);
        assert_eq!(ev_repriced[0].local_date, "2026-11-19");
        assert_eq!(ev_repriced[0].cost_usd, Some(2.5));
    }

    #[test]
    fn archive_falls_back_gracefully_on_future_schema_version_without_mutating_file() {
        let dir = tempfile::tempdir().unwrap();
        let cli_root = dir.path().join("cli");
        let data_dir = dir.path().join("data");
        fs::create_dir_all(&data_dir).unwrap();

        let archive_path = data_dir.join(ARCHIVE_DB_FILENAME);
        let future_conn = Connection::open(&archive_path).unwrap();
        future_conn.execute_batch("PRAGMA user_version = 99;").unwrap();
        drop(future_conn);
        let bytes_before = fs::read(&archive_path).unwrap();

        let u1 = "77777777-7777-7777-7777-777777777777";
        let db1 = cli_root.join("conversations").join(format!("{u1}.db"));
        let c1 = create_live_db(&db1);
        insert_turn(&c1, 1, 1_795_000_000, "model-alpha", 1_000);
        drop(c1);

        let (stats, events, _) = scan_sqlite_conversations_with_archive(
            &[("cli", cli_root)],
            &data_dir,
            None,
            1_795_200_000,
            &TimeZoneSpec::utc(),
            &PricingCatalog::default(),
        );
        assert_eq!(stats.db_files, 1);
        assert_eq!(stats.archived_db_files, 0);
        assert_eq!(events.len(), 1);

        // Verify the newer-schema DB bytes, journal mode, and sidecars were untouched.
        assert_eq!(fs::read(&archive_path).unwrap(), bytes_before);
        assert!(!data_dir.join(format!("{ARCHIVE_DB_FILENAME}-wal")).exists());
        assert!(!data_dir.join(ARCHIVE_INIT_LOCK_FILENAME).exists());
        let check = Connection::open_with_flags(
            &archive_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .unwrap();
        assert_eq!(read_user_version(&check).unwrap(), 99);
        assert_eq!(read_journal_mode(&check).unwrap(), "delete");
    }

    #[test]
    fn sync_stamp_freshness_respects_interval() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!is_sync_fresh(
            dir.path(),
            1_795_000_000,
            DEFAULT_ARCHIVE_SYNC_INTERVAL_SECS
        ));

        assert!(record_sync_stamp(Some(dir.path()), 1_795_000_000));
        assert!(is_sync_fresh(
            dir.path(),
            1_795_001_799,
            DEFAULT_ARCHIVE_SYNC_INTERVAL_SECS
        ));
        assert!(!is_sync_fresh(
            dir.path(),
            1_795_001_800,
            DEFAULT_ARCHIVE_SYNC_INTERVAL_SECS
        ));
    }
}
