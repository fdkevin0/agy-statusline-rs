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
    scan_sqlite_targets_raw, DbFileFingerprint, DbTarget, RawConversationScan, RawTurn,
    SqliteCoverage, UsageEvent,
};
use super::tz::TimeZoneSpec;
use super::SqliteStats;
use crate::db::{inspect_sqlite_path, is_absent_or_real_file, is_real_dir, SqlitePathStatus};
use crate::fsutil::atomic_write;
use crate::paths::{xdg_cache_dir, xdg_data_dir};

pub(crate) const ARCHIVE_DB_FILENAME: &str = "stats_history.db";
pub(super) const SYNC_STAMP_FILENAME: &str = "stats_archive_sync.ts";
pub(super) const DEFAULT_ARCHIVE_SYNC_INTERVAL_SECS: u64 = 1_800;

const ARCHIVE_INIT_LOCK_FILENAME: &str = "stats_history.lock";
const ARCHIVE_SCHEMA_VERSION: i32 = 2;
const CURRENT_SCAN_REV: i64 = 2;
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

fn sqlite_io_err(context: &str, err: rusqlite::Error) -> io::Error {
    io::Error::other(format!("{context}: {err}"))
}

fn unsafe_archive_path_err() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "unsafe stats archive path or sidecar (symlink/non-regular file)",
    )
}

fn acquire_init_lock(parent: &Path) -> io::Result<File> {
    fs::create_dir_all(parent)?;
    let lock_path = parent.join(ARCHIVE_INIT_LOCK_FILENAME);
    if !is_absent_or_real_file(&lock_path) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unsafe stats archive lock path (symlink/non-regular file)",
        ));
    }
    let lock = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;
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

/// Opens (or initializes) `db_path`, returning `Ok(None)` without mutating the file
/// when the archive was created by a newer schema version (`user_version > ARCHIVE_SCHEMA_VERSION`).
fn open_archive_db(db_path: &Path) -> io::Result<Option<Connection>> {
    // Inspect an existing file read-only first so a WAL-mode future-schema DB is never
    // checkpointed or modified on close, and steady-state runs avoid the init lock.
    match inspect_sqlite_path(db_path) {
        SqlitePathStatus::Unsafe => return Err(unsafe_archive_path_err()),
        SqlitePathStatus::Missing => {}
        SqlitePathStatus::Valid => {
            let ro = Connection::open_with_flags(
                db_path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .map_err(|e| sqlite_io_err("failed to inspect stats archive", e))?;
            ro.busy_timeout(Duration::from_millis(ARCHIVE_BUSY_TIMEOUT_MS))
                .map_err(|e| sqlite_io_err("failed to configure stats archive timeout", e))?;
            let version = read_user_version(&ro)
                .map_err(|e| sqlite_io_err("failed to read stats archive schema version", e))?;
            if version > ARCHIVE_SCHEMA_VERSION {
                return Ok(None);
            }
            let is_wal = read_journal_mode(&ro)
                .map_err(|e| sqlite_io_err("failed to read stats archive journal mode", e))?
                .eq_ignore_ascii_case("wal");
            drop(ro);

            if version == ARCHIVE_SCHEMA_VERSION && is_wal {
                let conn = Connection::open_with_flags(
                    db_path,
                    OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
                )
                .map_err(|e| sqlite_io_err("failed to open stats archive", e))?;
                conn.busy_timeout(Duration::from_millis(ARCHIVE_BUSY_TIMEOUT_MS))
                    .map_err(|e| sqlite_io_err("failed to configure stats archive timeout", e))?;
                configure_connection_pragmas(&conn)
                    .map_err(|e| sqlite_io_err("failed to configure stats archive pragmas", e))?;
                return Ok(Some(conn));
            }
        }
    }

    // Serialize cold creation, WAL upgrade, and schema migration across concurrent processes
    // so parallel cold starts never hit SQLite's lock-upgrade SQLITE_BUSY deadlock on
    // `PRAGMA journal_mode = WAL`.
    let parent = db_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "archive path has no parent"))?;
    let _init_lock = acquire_init_lock(parent)?;

    match inspect_sqlite_path(db_path) {
        SqlitePathStatus::Unsafe => return Err(unsafe_archive_path_err()),
        SqlitePathStatus::Missing => {}
        SqlitePathStatus::Valid => {
            let ro = Connection::open_with_flags(
                db_path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .map_err(|e| sqlite_io_err("failed to inspect stats archive", e))?;
            ro.busy_timeout(Duration::from_millis(ARCHIVE_BUSY_TIMEOUT_MS))
                .map_err(|e| sqlite_io_err("failed to configure stats archive timeout", e))?;
            if read_user_version(&ro)
                .map_err(|e| sqlite_io_err("failed to read stats archive schema version", e))?
                > ARCHIVE_SCHEMA_VERSION
            {
                return Ok(None);
            }
        }
    }

    let mut conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| sqlite_io_err("failed to open stats archive", e))?;
    conn.busy_timeout(Duration::from_millis(ARCHIVE_BUSY_TIMEOUT_MS))
        .map_err(|e| sqlite_io_err("failed to configure stats archive timeout", e))?;

    let version = read_user_version(&conn)
        .map_err(|e| sqlite_io_err("failed to read stats archive schema version", e))?;
    if !read_journal_mode(&conn)
        .map_err(|e| sqlite_io_err("failed to read stats archive journal mode", e))?
        .eq_ignore_ascii_case("wal")
    {
        conn.execute_batch("PRAGMA journal_mode = WAL;")
            .map_err(|e| sqlite_io_err("failed to enable WAL on stats archive", e))?;
    }
    configure_connection_pragmas(&conn)
        .map_err(|e| sqlite_io_err("failed to configure stats archive pragmas", e))?;

    if version < ARCHIVE_SCHEMA_VERSION {
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| sqlite_io_err("failed to begin stats archive migration", e))?;
        let tx_version = read_user_version(&tx)
            .map_err(|e| sqlite_io_err("failed to read stats archive schema version", e))?;
        if tx_version == 0 {
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS archived_conversations (
                    root_path        TEXT NOT NULL,
                    uuid             TEXT NOT NULL,
                    db_mtime_secs    INTEGER NOT NULL,
                    db_mtime_nanos   INTEGER NOT NULL,
                    db_ctime_secs    INTEGER NOT NULL DEFAULT 0,
                    db_ctime_nanos   INTEGER NOT NULL DEFAULT 0,
                    db_size          INTEGER NOT NULL,
                    wal_mtime_secs   INTEGER NOT NULL,
                    wal_mtime_nanos  INTEGER NOT NULL,
                    wal_ctime_secs   INTEGER NOT NULL DEFAULT 0,
                    wal_ctime_nanos  INTEGER NOT NULL DEFAULT 0,
                    wal_size         INTEGER NOT NULL,
                    scan_rev         INTEGER NOT NULL DEFAULT 2,
                    synced_at_secs   INTEGER NOT NULL,
                    PRIMARY KEY (root_path, uuid)
                ) WITHOUT ROWID;

                CREATE TABLE IF NOT EXISTS archived_turns (
                    root_path          TEXT NOT NULL,
                    uuid               TEXT NOT NULL,
                    turn_idx           INTEGER NOT NULL,
                    ts_secs            INTEGER,
                    ts_quality         INTEGER NOT NULL DEFAULT 0,
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
                    id_11              TEXT,
                    id_12              TEXT,
                    id_7               TEXT,
                    PRIMARY KEY (root_path, uuid, turn_idx),
                    FOREIGN KEY (root_path, uuid)
                        REFERENCES archived_conversations (root_path, uuid)
                        ON DELETE CASCADE
                ) WITHOUT ROWID;

                PRAGMA user_version = 2;",
            )
            .map_err(|e| sqlite_io_err("failed to initialize stats archive schema", e))?;
        } else if tx_version == 1 {
            // Non-destructive v1 -> v2 upgrade: preserve all archived (including pruned) rows,
            // defaulting scan_rev = 1 so still-live DBs rescan once to backfill identities/retries.
            tx.execute_batch(
                "ALTER TABLE archived_conversations ADD COLUMN db_ctime_secs   INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE archived_conversations ADD COLUMN db_ctime_nanos  INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE archived_conversations ADD COLUMN wal_ctime_secs  INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE archived_conversations ADD COLUMN wal_ctime_nanos INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE archived_conversations ADD COLUMN scan_rev        INTEGER NOT NULL DEFAULT 1;
                 ALTER TABLE archived_turns         ADD COLUMN ts_quality      INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE archived_turns         ADD COLUMN id_11           TEXT;
                 ALTER TABLE archived_turns         ADD COLUMN id_12           TEXT;
                 ALTER TABLE archived_turns         ADD COLUMN id_7            TEXT;
                 PRAGMA user_version = 2;",
            )
            .map_err(|e| sqlite_io_err("failed to migrate stats archive schema v1->v2", e))?;
        }
        tx.commit()
            .map_err(|e| sqlite_io_err("failed to commit stats archive schema", e))?;
    }

    Ok(Some(conn))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StoredFingerprint {
    fingerprint: DbFileFingerprint,
    scan_rev: i64,
}

type FingerprintIndex = HashMap<String, HashMap<String, StoredFingerprint>>;

fn load_existing_fingerprints(
    conn: &Connection,
    unique_roots: &[(&'static str, String)],
) -> rusqlite::Result<FingerprintIndex> {
    let mut out: FingerprintIndex = HashMap::with_capacity(unique_roots.len());
    let mut stmt = conn.prepare(
        "SELECT uuid, db_mtime_secs, db_mtime_nanos, db_ctime_secs, db_ctime_nanos, db_size,
                wal_mtime_secs, wal_mtime_nanos, wal_ctime_secs, wal_ctime_nanos, wal_size,
                scan_rev
         FROM archived_conversations
         WHERE root_path = ?1",
    )?;
    for (_, root_key) in unique_roots {
        let mut by_uuid = HashMap::new();
        let mut rows = stmt.query([root_key.as_str()])?;
        while let Some(row) = rows.next()? {
            let uuid: String = row.get(0)?;
            let fp = DbFileFingerprint {
                db_mtime_secs: row.get(1)?,
                db_mtime_nanos: row.get::<_, i64>(2)?.max(0) as u32,
                db_ctime_secs: row.get(3)?,
                db_ctime_nanos: row.get::<_, i64>(4)?.max(0) as u32,
                db_size: row.get::<_, i64>(5)?.max(0) as u64,
                wal_mtime_secs: row.get(6)?,
                wal_mtime_nanos: row.get::<_, i64>(7)?.max(0) as u32,
                wal_ctime_secs: row.get(8)?,
                wal_ctime_nanos: row.get::<_, i64>(9)?.max(0) as u32,
                wal_size: row.get::<_, i64>(10)?.max(0) as u64,
            };
            let scan_rev: i64 = row.get(11)?;
            by_uuid.insert(
                uuid,
                StoredFingerprint {
                    fingerprint: fp,
                    scan_rev,
                },
            );
        }
        out.insert(root_key.clone(), by_uuid);
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
                root_path, uuid,
                db_mtime_secs, db_mtime_nanos, db_ctime_secs, db_ctime_nanos, db_size,
                wal_mtime_secs, wal_mtime_nanos, wal_ctime_secs, wal_ctime_nanos, wal_size,
                scan_rev, synced_at_secs
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
             ON CONFLICT(root_path, uuid) DO UPDATE SET
                db_mtime_secs   = excluded.db_mtime_secs,
                db_mtime_nanos  = excluded.db_mtime_nanos,
                db_ctime_secs   = excluded.db_ctime_secs,
                db_ctime_nanos  = excluded.db_ctime_nanos,
                db_size         = excluded.db_size,
                wal_mtime_secs  = excluded.wal_mtime_secs,
                wal_mtime_nanos = excluded.wal_mtime_nanos,
                wal_ctime_secs  = excluded.wal_ctime_secs,
                wal_ctime_nanos = excluded.wal_ctime_nanos,
                wal_size        = excluded.wal_size,
                scan_rev        = excluded.scan_rev,
                synced_at_secs  = excluded.synced_at_secs",
        )?;
        let mut delete_turns =
            tx.prepare("DELETE FROM archived_turns WHERE root_path = ?1 AND uuid = ?2")?;
        let mut insert_turn = tx.prepare(
            "INSERT INTO archived_turns (
                root_path, uuid, turn_idx, ts_secs, ts_quality,
                model_enum, response_model, model_display_name,
                input_tokens, cache_read_tokens, cache_write_tokens,
                output_tokens, thinking_tokens,
                native_cost_usd, decode_speed, ttft_secs,
                id_11, id_12, id_7
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5,
                ?6, ?7, ?8,
                ?9, ?10, ?11,
                ?12, ?13,
                ?14, ?15, ?16,
                ?17, ?18, ?19
             )",
        )?;

        for conv in scanned {
            let fp = conv.fingerprint;
            upsert_conv.execute(params![
                conv.root_key,
                conv.uuid,
                fp.db_mtime_secs,
                i64::from(fp.db_mtime_nanos),
                fp.db_ctime_secs,
                i64::from(fp.db_ctime_nanos),
                fp.db_size.min(i64::MAX as u64) as i64,
                fp.wal_mtime_secs,
                i64::from(fp.wal_mtime_nanos),
                fp.wal_ctime_secs,
                i64::from(fp.wal_ctime_nanos),
                fp.wal_size.min(i64::MAX as u64) as i64,
                CURRENT_SCAN_REV,
                synced_at,
            ])?;
            delete_turns.execute(params![conv.root_key, conv.uuid])?;
            for (idx, turn) in conv.turns.iter().enumerate() {
                insert_turn.execute(params![
                    conv.root_key,
                    conv.uuid,
                    idx as i64,
                    turn.ts_secs,
                    i64::from(turn.ts_quality),
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
                    turn.id_11.as_deref(),
                    turn.id_12.as_deref(),
                    turn.id_7.as_deref(),
                ])?;
            }
        }
    }
    tx.commit()
}

/// Syncs modified `targets` into `conn` and returns `(scanned, total_stored, archived_only)`.
fn sync_changed_targets(
    conn: &mut Connection,
    unique_roots: &[(&'static str, String)],
    targets: &[DbTarget],
    now_unix_secs: u64,
) -> rusqlite::Result<(u64, u64, u64)> {
    let mut existing = load_existing_fingerprints(conn, unique_roots)?;
    let changed_targets: Vec<_> = targets
        .iter()
        .filter(|t| {
            existing
                .get(t.root_key.as_str())
                .and_then(|by_uuid| by_uuid.get(t.uuid.as_str()))
                .is_none_or(|prev| {
                    prev.scan_rev != CURRENT_SCAN_REV || prev.fingerprint != t.fingerprint
                })
        })
        .cloned()
        .collect();
    let scanned = scan_sqlite_targets_raw(&changed_targets);
    upsert_scanned_conversations(conn, &scanned, now_unix_secs)?;

    for conv in &scanned {
        existing.entry(conv.root_key.clone()).or_default().insert(
            conv.uuid.clone(),
            StoredFingerprint {
                fingerprint: conv.fingerprint,
                scan_rev: CURRENT_SCAN_REV,
            },
        );
    }
    let live_keys: HashSet<(&str, &str)> = targets
        .iter()
        .map(|t| (t.root_key.as_str(), t.uuid.as_str()))
        .collect();
    let mut total_db_files = 0u64;
    let mut archived_db_files = 0u64;
    for (root_key, by_uuid) in &existing {
        total_db_files += by_uuid.len() as u64;
        for uuid in by_uuid.keys() {
            if !live_keys.contains(&(root_key.as_str(), uuid.as_str())) {
                archived_db_files += 1;
            }
        }
    }
    Ok((scanned.len() as u64, total_db_files, archived_db_files))
}

struct StoredConversation {
    source: &'static str,
    uuid: String,
    turns: Vec<RawTurn>,
}

fn load_stored_conversations(
    conn: &Connection,
    unique_roots: &[(&'static str, String)],
) -> rusqlite::Result<Vec<StoredConversation>> {
    let mut conversations = Vec::new();
    let mut conv_stmt = conn.prepare(
        "SELECT uuid FROM archived_conversations WHERE root_path = ?1 ORDER BY uuid ASC",
    )?;
    let mut turn_stmt = conn.prepare(
        "SELECT uuid, ts_secs, ts_quality, model_enum, response_model, model_display_name,
                input_tokens, cache_read_tokens, cache_write_tokens,
                output_tokens, thinking_tokens,
                native_cost_usd, decode_speed, ttft_secs,
                id_11, id_12, id_7
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
                ts_quality: row.get::<_, i64>(2)?.clamp(0, i64::from(u8::MAX)) as u8,
                model_enum: row.get::<_, i64>(3)?.max(0) as u64,
                response_model: row.get(4)?,
                model_display_name: row.get(5)?,
                input_tokens: row.get::<_, i64>(6)?.max(0) as u64,
                cache_read_tokens: row.get::<_, i64>(7)?.max(0) as u64,
                cache_write_tokens: row.get::<_, i64>(8)?.max(0) as u64,
                output_tokens: row.get::<_, i64>(9)?.max(0) as u64,
                thinking_tokens: row.get::<_, i64>(10)?.max(0) as u64,
                native_cost_usd: row.get(11)?,
                decode_speed: row.get(12)?,
                ttft_secs: row.get(13)?,
                id_11: row.get(14)?,
                id_12: row.get(15)?,
                id_7: row.get(16)?,
            };
            turns_by_uuid.entry(uuid).or_default().push(turn);
        }

        let mut conv_rows = conv_stmt.query([root_key.as_str()])?;
        while let Some(row) = conv_rows.next()? {
            let uuid: String = row.get(0)?;
            let turns = turns_by_uuid.remove(&uuid).unwrap_or_default();
            conversations.push(StoredConversation {
                source,
                uuid,
                turns,
            });
        }
    }

    Ok(conversations)
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
    atomic_write(
        &dir.join(SYNC_STAMP_FILENAME),
        format!("{now_unix_secs}\n").as_bytes(),
    )
    .is_ok()
}

fn project_live_targets(
    targets: &[DbTarget],
    tz: &TimeZoneSpec,
    pricing: &PricingCatalog,
) -> (SqliteStats, Vec<UsageEvent>, SqliteCoverage) {
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
/// (when `data_dir` is `Some`) and projects all live + archived conversations through
/// `tz` and `pricing`.
///
/// Falls back transparently to a live-only scan when `data_dir` is `None` or the archive
/// database cannot be opened or updated.
pub(super) fn scan_sqlite_conversations_with_archive(
    roots: &[(&'static str, PathBuf)],
    data_dir: Option<&Path>,
    cache_dir: Option<&Path>,
    now_unix_secs: u64,
    tz: &TimeZoneSpec,
    pricing: &PricingCatalog,
) -> (SqliteStats, Vec<UsageEvent>, SqliteCoverage) {
    let targets = discover_sqlite_db_targets(roots);
    let Some(dir) = data_dir else {
        return project_live_targets(&targets, tz, pricing);
    };
    let archive_path = dir.join(ARCHIVE_DB_FILENAME);

    // Avoid creating an empty archive DB on a fresh home with no conversations.
    if targets.is_empty() && matches!(inspect_sqlite_path(&archive_path), SqlitePathStatus::Missing)
    {
        return (SqliteStats::default(), Vec::new(), SqliteCoverage::default());
    }

    let unique_roots = deduplicate_roots(roots);
    let Ok(Some(mut conn)) = open_archive_db(&archive_path) else {
        return project_live_targets(&targets, tz, pricing);
    };
    let Ok((_, _, archived_db_files)) =
        sync_changed_targets(&mut conn, &unique_roots, &targets, now_unix_secs)
    else {
        return project_live_targets(&targets, tz, pricing);
    };
    let Ok(stored) = load_stored_conversations(&conn, &unique_roots) else {
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

    if targets.is_empty() && matches!(inspect_sqlite_path(&archive_path), SqlitePathStatus::Missing)
    {
        record_sync_stamp(cache_dir, now_unix_secs);
        return Ok(ArchiveSyncSummary {
            archive_path,
            live_db_files: 0,
            scanned_db_files: 0,
            total_db_files: 0,
            archived_db_files: 0,
        });
    }

    let mut conn = open_archive_db(&archive_path)?.ok_or_else(|| {
        io::Error::other(format!(
            "stats archive at {} uses a newer unsupported schema version",
            archive_path.display()
        ))
    })?;
    let (scanned_db_files, total_db_files, archived_db_files) =
        sync_changed_targets(&mut conn, &unique_roots, &targets, now_unix_secs)
            .map_err(|e| sqlite_io_err("failed to update stats archive", e))?;

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

fn maybe_spawn_background_archive_sync_for_roots(
    roots: &[(&'static str, PathBuf)],
    cache_dir: Option<&Path>,
    now_unix_secs: u64,
) {
    let Some(dir) = cache_dir else {
        return;
    };
    if is_sync_fresh(dir, now_unix_secs, DEFAULT_ARCHIVE_SYNC_INTERVAL_SECS) {
        return;
    }
    if !roots
        .iter()
        .any(|(_, r)| is_real_dir(&r.join("conversations")))
    {
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

/// Spawns a detached background `agy-statusline stats sync` process when any local
/// `conversations/` directory exists and the sync stamp is missing or past the interval.
pub fn maybe_spawn_background_archive_sync(
    home: Option<&Path>,
    cache_dir: Option<&Path>,
    now_unix_secs: u64,
) {
    let roots = resolve_roots(home);
    maybe_spawn_background_archive_sync_for_roots(&roots, cache_dir, now_unix_secs);
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
            Some(&data_dir),
            Some(&cache_dir),
            1_795_200_000,
            &tz,
            &pricing,
        );
        assert_eq!(stats1.db_files, 2);
        assert_eq!(stats1.archived_db_files, 0);
        assert_eq!(stats1.completed_turns, 2);
        assert_eq!(events1.len(), 2);
        assert!(covered1.conversations.contains(&("cli", u1.to_string())));

        // Simulate AGY pruning the oldest conversation DB on startup.
        fs::remove_file(&db1).unwrap();

        let (stats2, events2, covered2) = scan_sqlite_conversations_with_archive(
            &roots,
            Some(&data_dir),
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
        assert!(covered2.conversations.contains(&("cli", u1.to_string())));
        assert!(covered2.conversations.contains(&("cli", u2.to_string())));
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
            Some(&data_dir),
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
            Some(&data_dir),
            None,
            1_795_200_000,
            &tz,
            &pricing,
        );
        let (_, events_b, _) = scan_sqlite_conversations_with_archive(
            &[("cli", root_b.clone())],
            Some(&data_dir),
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
            Some(&data_dir),
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
            Some(&data_dir),
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
            &[("cli", cli_root.clone())],
            Some(&data_dir),
            None,
            1_795_200_000,
            &TimeZoneSpec::utc(),
            &PricingCatalog::default(),
        );
        assert_eq!(stats.db_files, 1);
        assert_eq!(stats.archived_db_files, 0);
        assert_eq!(events.len(), 1);

        // Verify delete-mode newer-schema DB bytes, journal mode, and sidecars were untouched.
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
        drop(check);

        // Also verify a WAL-mode newer-schema DB with an active uncheckpointed `-wal` is
        // never checkpointed or mutated.
        let wal_data_dir = dir.path().join("data_wal");
        fs::create_dir_all(&wal_data_dir).unwrap();
        let wal_archive = wal_data_dir.join(ARCHIVE_DB_FILENAME);
        let wal_sidecar = wal_data_dir.join(format!("{ARCHIVE_DB_FILENAME}-wal"));
        let wal_conn = Connection::open(&wal_archive).unwrap();
        wal_conn
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA wal_autocheckpoint = 0;
                 CREATE TABLE future_v99 (id INTEGER PRIMARY KEY);
                 INSERT INTO future_v99 VALUES (1);
                 PRAGMA user_version = 99;",
            )
            .unwrap();
        let wal_db_before = fs::read(&wal_archive).unwrap();
        let wal_log_before = fs::read(&wal_sidecar).unwrap();
        assert!(!wal_log_before.is_empty());

        let (wal_stats, wal_events, _) = scan_sqlite_conversations_with_archive(
            &[("cli", cli_root)],
            Some(&wal_data_dir),
            None,
            1_795_200_000,
            &TimeZoneSpec::utc(),
            &PricingCatalog::default(),
        );
        assert_eq!(wal_stats.db_files, 1);
        assert_eq!(wal_stats.archived_db_files, 0);
        assert_eq!(wal_events.len(), 1);
        assert_eq!(fs::read(&wal_archive).unwrap(), wal_db_before);
        assert_eq!(fs::read(&wal_sidecar).unwrap(), wal_log_before);
        assert!(!wal_data_dir.join(ARCHIVE_INIT_LOCK_FILENAME).exists());
        drop(wal_conn);
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

    #[test]
    fn record_sync_stamp_writes_atomically_and_updates_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let nested_cache = dir.path().join("nested").join("cache");

        assert!(record_sync_stamp(Some(&nested_cache), 1_795_000_000));
        assert_eq!(
            fs::read_to_string(nested_cache.join(SYNC_STAMP_FILENAME)).unwrap(),
            "1795000000\n"
        );

        assert!(record_sync_stamp(Some(&nested_cache), 1_795_000_500));
        assert_eq!(
            fs::read_to_string(nested_cache.join(SYNC_STAMP_FILENAME)).unwrap(),
            "1795000500\n"
        );
        // Atomic rename leaves only the stamp file, with no leftover temp files.
        assert_eq!(fs::read_dir(&nested_cache).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn open_archive_db_rejects_symlinks_for_db_sidecars_and_lock() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let external = dir.path().join("external");
        fs::create_dir_all(&external).unwrap();

        // 1. Symlinked stats_history.db pointing to an existing external file.
        let d_sym = dir.path().join("sym_db");
        fs::create_dir_all(&d_sym).unwrap();
        let target_db = external.join("target.db");
        fs::write(&target_db, b"untouched-db").unwrap();
        let sym_archive = d_sym.join(ARCHIVE_DB_FILENAME);
        symlink(&target_db, &sym_archive).unwrap();
        let err = open_archive_db(&sym_archive).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(fs::read(&target_db).unwrap(), b"untouched-db");

        // 2. Dangling symlink stats_history.db must not create the target.
        let d_dangling = dir.path().join("dangling_db");
        fs::create_dir_all(&d_dangling).unwrap();
        let dangling_target = external.join("must_not_create.db");
        let dangling_archive = d_dangling.join(ARCHIVE_DB_FILENAME);
        symlink(&dangling_target, &dangling_archive).unwrap();
        let err = open_archive_db(&dangling_archive).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(!dangling_target.exists());

        // 3. Symlinked -wal sidecar (with absent archive DB) must not create DB or modify target.
        let d_wal = dir.path().join("sym_wal");
        fs::create_dir_all(&d_wal).unwrap();
        let target_wal = external.join("target.wal");
        fs::write(&target_wal, b"untouched-wal").unwrap();
        let wal_archive = d_wal.join(ARCHIVE_DB_FILENAME);
        symlink(&target_wal, d_wal.join(format!("{ARCHIVE_DB_FILENAME}-wal"))).unwrap();
        let err = open_archive_db(&wal_archive).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(!wal_archive.exists());
        assert_eq!(fs::read(&target_wal).unwrap(), b"untouched-wal");

        // 4. Symlinked -shm sidecar (with real archive DB) must be rejected without mutating target.
        let d_shm = dir.path().join("sym_shm");
        fs::create_dir_all(&d_shm).unwrap();
        let target_shm = external.join("target.shm");
        fs::write(&target_shm, b"untouched-shm").unwrap();
        let shm_archive = d_shm.join(ARCHIVE_DB_FILENAME);
        fs::write(&shm_archive, b"not-a-db").unwrap();
        symlink(&target_shm, d_shm.join(format!("{ARCHIVE_DB_FILENAME}-shm"))).unwrap();
        let err = open_archive_db(&shm_archive).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(fs::read(&target_shm).unwrap(), b"untouched-shm");

        // 5. Symlinked stats_history.lock must be rejected before creating stats_history.db.
        let d_lock = dir.path().join("sym_lock");
        fs::create_dir_all(&d_lock).unwrap();
        let target_lock = external.join("target.lock");
        fs::write(&target_lock, b"untouched-lock").unwrap();
        symlink(&target_lock, d_lock.join(ARCHIVE_INIT_LOCK_FILENAME)).unwrap();
        let lock_archive = d_lock.join(ARCHIVE_DB_FILENAME);
        let err = open_archive_db(&lock_archive).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(!lock_archive.exists());
        assert_eq!(fs::read(&target_lock).unwrap(), b"untouched-lock");
    }

    #[cfg(unix)]
    #[test]
    fn background_archive_sync_ignores_symlinked_conversations_dir() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let real_conv = dir.path().join("real_conversations");
        let cli_root = dir.path().join("cli");
        let cache_dir = dir.path().join("cache");
        fs::create_dir_all(&real_conv).unwrap();
        fs::create_dir_all(&cli_root).unwrap();
        symlink(&real_conv, cli_root.join("conversations")).unwrap();

        let roots = [("cli", cli_root)];
        maybe_spawn_background_archive_sync_for_roots(&roots, Some(&cache_dir), 1_795_200_000);

        // Ignored symlinked conversations/ returns before recording the throttle stamp or spawning.
        assert!(!cache_dir.join(SYNC_STAMP_FILENAME).exists());
    }

    #[test]
    fn archive_migrates_v1_to_v2_preserving_pruned_rows_and_backfilling_live_identities() {
        use crate::protobuf::{encode_step_blob, StepBlobSpec, UsageSpec};

        let dir = tempfile::tempdir().unwrap();
        let cli_root = dir.path().join("cli");
        let desk_root = dir.path().join("desktop");
        let data_dir = dir.path().join("data");
        fs::create_dir_all(&data_dir).unwrap();

        let u_pruned = "11111111-1111-4111-8111-111111111111";
        let u_live_cli = "22222222-2222-4222-8222-222222222222";
        let u_live_desk = "33333333-3333-4333-8333-333333333333";

        // Create live CLI DB containing a turn with id_11="shared-id" and an uncovered compaction step.
        let cli_db = cli_root
            .join("conversations")
            .join(format!("{u_live_cli}.db"));
        let cli_conn = create_live_db(&cli_db);
        cli_conn
            .execute(
                "INSERT INTO steps (idx, metadata) VALUES (1, ?1)",
                [encode_step_meta_blob(1_795_100_000)],
            )
            .unwrap();
        cli_conn
            .execute(
                "INSERT INTO steps (idx, metadata) VALUES (2, ?1)",
                [encode_step_blob(StepBlobSpec {
                    created_at_secs: Some(1_795_100_010),
                    usage: Some(UsageSpec {
                        model_enum: 1526,
                        input_tok: 500,
                        output_tok: 50,
                        ..Default::default()
                    }),
                    ..Default::default()
                })],
            )
            .unwrap();
        cli_conn
            .execute(
                "INSERT INTO gen_metadata (idx, data) VALUES (1, ?1)",
                [encode_gen_blob(GenBlobSpec {
                    step_indices: vec![1],
                    model_enum: 1526,
                    resp_model: Some("model-alpha"),
                    disp_name: Some("Model Alpha"),
                    input_tok: 2_000,
                    output_tok: 200,
                    id_11: Some("shared-id"),
                    dur_secs: 2,
                    ..Default::default()
                })],
            )
            .unwrap();
        drop(cli_conn);

        // Create live Desktop DB with a mirrored copy of the same turn sharing id_11="shared-id".
        let desk_db = desk_root
            .join("conversations")
            .join(format!("{u_live_desk}.db"));
        let desk_conn = create_live_db(&desk_db);
        desk_conn
            .execute(
                "INSERT INTO steps (idx, metadata) VALUES (1, ?1)",
                [encode_step_meta_blob(1_795_100_000)],
            )
            .unwrap();
        desk_conn
            .execute(
                "INSERT INTO gen_metadata (idx, data) VALUES (1, ?1)",
                [encode_gen_blob(GenBlobSpec {
                    step_indices: vec![1],
                    model_enum: 1526,
                    resp_model: Some("model-alpha"),
                    disp_name: Some("Model Alpha"),
                    input_tok: 2_000,
                    output_tok: 200,
                    id_11: Some("shared-id"),
                    dur_secs: 2,
                    ..Default::default()
                })],
            )
            .unwrap();
        drop(desk_conn);

        let cli_root_key = root_path_key(&cli_root);
        let cli_fp = super::super::scan::db_file_fingerprint(&cli_db).unwrap();

        // Seed an authentic v1 archive DB containing 1 pruned conversation and 1 live CLI DB
        // matching the exact current mtime/size (proving scan_rev=1 triggers a one-time v2 rescan
        // even when mtime/size are unchanged and even on platforms where ctime == 0).
        let archive_path = data_dir.join(ARCHIVE_DB_FILENAME);
        let v1 = Connection::open(&archive_path).unwrap();
        v1.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE archived_conversations (
                 root_path        TEXT NOT NULL,
                 uuid             TEXT NOT NULL,
                 db_mtime_secs    INTEGER NOT NULL,
                 db_mtime_nanos   INTEGER NOT NULL,
                 db_size          INTEGER NOT NULL,
                 wal_mtime_secs   INTEGER NOT NULL,
                 wal_mtime_nanos  INTEGER NOT NULL,
                 wal_size         INTEGER NOT NULL,
                 synced_at_secs   INTEGER NOT NULL,
                 PRIMARY KEY (root_path, uuid)
             ) WITHOUT ROWID;
             CREATE TABLE archived_turns (
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
                 PRIMARY KEY (root_path, uuid, turn_idx),
                 FOREIGN KEY (root_path, uuid)
                     REFERENCES archived_conversations (root_path, uuid)
                     ON DELETE CASCADE
             ) WITHOUT ROWID;
             PRAGMA user_version = 1;",
        )
        .unwrap();
        v1.execute(
            "INSERT INTO archived_conversations VALUES (?1, ?2, 100, 0, 4096, 0, 0, 0, 1000)",
            params![cli_root_key, u_pruned],
        )
        .unwrap();
        v1.execute(
            "INSERT INTO archived_turns VALUES (?1, ?2, 0, 1795000000, 100, 'pruned-model', 'Pruned Model', 900, 0, 0, 90, 0, NULL, 90.0, 0.3)",
            params![cli_root_key, u_pruned],
        )
        .unwrap();
        v1.execute(
            "INSERT INTO archived_conversations VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 1000)",
            params![
                cli_root_key,
                u_live_cli,
                cli_fp.db_mtime_secs,
                i64::from(cli_fp.db_mtime_nanos),
                cli_fp.db_size as i64,
                cli_fp.wal_mtime_secs,
                i64::from(cli_fp.wal_mtime_nanos),
                cli_fp.wal_size as i64,
            ],
        )
        .unwrap();
        v1.execute(
            "INSERT INTO archived_turns VALUES (?1, ?2, 0, 1795100000, 1526, 'model-alpha', 'Model Alpha', 2000, 0, 0, 200, 0, NULL, 100.0, NULL)",
            params![cli_root_key, u_live_cli],
        )
        .unwrap();
        drop(v1);

        let roots = [("cli", cli_root.clone()), ("desktop", desk_root.clone())];
        let summary1 = sync_roots_into_archive(&roots, &data_dir, None, 1_795_200_000).unwrap();
        assert_eq!(summary1.live_db_files, 2);
        assert_eq!(summary1.scanned_db_files, 2);
        assert_eq!(summary1.total_db_files, 3);
        assert_eq!(summary1.archived_db_files, 1);

        // Verify user_version upgraded to 2 without losing the pruned v1 row.
        let check = Connection::open(&archive_path).unwrap();
        assert_eq!(read_user_version(&check).unwrap(), ARCHIVE_SCHEMA_VERSION);
        drop(check);

        // Second sync sees scan_rev == 2 and unchanged fingerprints -> 0 rescans.
        let summary2 = sync_roots_into_archive(&roots, &data_dir, None, 1_795_200_010).unwrap();
        assert_eq!(summary2.scanned_db_files, 0);

        // Projecting across both roots preserves pruned v1 turn, backfills compaction step,
        // and cross-root Union-Find deduplicates the shared CLI/Desktop turn!
        let (stats, events, covered) = scan_sqlite_conversations_with_archive(
            &roots,
            Some(&data_dir),
            None,
            1_795_200_020,
            &TimeZoneSpec::utc(),
            &PricingCatalog::default(),
        );
        assert_eq!(stats.db_files, 3);
        assert_eq!(stats.archived_db_files, 1);
        // 1 pruned turn + 1 deduplicated shared turn (not 2!) = 2 completed speed turns.
        assert_eq!(stats.completed_turns, 2);
        // 1 pruned turn + 1 deduplicated shared turn + 1 backfilled compaction step = 3 events.
        assert_eq!(events.len(), 3);
        assert!(covered.response_ids.contains("shared-id"));

        // Verify a ctime-only change triggers a rescan on the next sync.
        let mutate = Connection::open(&archive_path).unwrap();
        mutate
            .execute(
                "UPDATE archived_conversations SET db_ctime_secs = db_ctime_secs - 1 WHERE root_path = ?1 AND uuid = ?2",
                params![cli_root_key, u_live_cli],
            )
            .unwrap();
        drop(mutate);
        let summary3 = sync_roots_into_archive(&roots, &data_dir, None, 1_795_200_030).unwrap();
        assert_eq!(summary3.scanned_db_files, 1);
    }
}
