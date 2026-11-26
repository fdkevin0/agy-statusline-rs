//! Read-only SQLite conversation database reader and UUID validation.
//!
//! Queries `<agy_cli_dir>/conversations/<conversation_id>.db` for recent
//! `gen_metadata` rows and extracts decode/e2e speed, TTFT, thinking tokens,
//! and context window metadata.

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};

use crate::agy::protobuf::{
    extract_gen_metadata_summary, is_foreign_cascade_id, RetryAttemptRecord, RetryStatusClass,
    TurnMetrics,
};
use crate::sys::fs::{is_absent_or_real_file, is_real_dir};

const SQLITE_HEADER_BYTES: usize = 100;
const SQLITE_WAL_HEADER_BYTES: u64 = 32;
const SQLITE_MAGIC_PREFIX: &[u8; 16] = b"SQLite format 3\0";

/// Compact summary of recent retry anomalies sniffed from `gen_metadata` (`chat.17`).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RetryAnomalySummary {
    /// Total actionable retry attempts observed across recent own turns.
    pub retry_count: u64,
    /// Actionable retries classified as HTTP 429 / rate-limit / quota exhaustion.
    pub retries_429: u64,
    /// Actionable retries classified as HTTP 503 / capacity / overload.
    pub retries_503: u64,
    /// Actionable retries in all other non-canceled categories (5xx, timeout, disconnect, etc.).
    pub retries_other: u64,
    /// Cumulative retry backoff delay in seconds across recent retries.
    pub total_retry_delay_secs: f64,
    /// Classification of the most recent actionable retry anomaly.
    pub latest_class: RetryStatusClass,
    /// Explicit HTTP status code (`429`, `503`, `500`, `502`, `504`, etc.) of the latest anomaly.
    pub latest_status_code: Option<u16>,
}

/// Session generation metadata sniffed from `gen_metadata`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SessionMetadata {
    /// 5-turn token-weighted average decode speed (`sum(output) / sum(decode_secs)`).
    pub speed: Option<f64>,
    /// Median single-turn decode speed across up to 9 valid completed turns.
    pub median_speed: Option<f64>,
    /// Latest single completed turn decode speed (`output / decode_secs`).
    pub last_turn_speed: Option<f64>,
    /// Latest turn end-to-end throughput (`output / (ttft + decode_secs)`).
    pub e2e_speed: Option<f64>,
    /// Latest turn time-to-first-token in seconds.
    pub ttft_secs: Option<f64>,
    /// Server-issued compression context threshold (`max_context_tokens`).
    pub max_context_tokens: Option<u64>,
    /// Server-estimated context tokens used (`estimated_tokens_used`) from `gen_metadata`.
    pub estimated_context_tokens: Option<u64>,
    /// Thinking output tokens (`thinking_output_tokens`) from the latest completed turn.
    pub thinking_tokens: Option<u64>,
    /// Locally estimated session cost in USD (used when AGY payload omits `cost.total_usd`).
    pub estimated_cost_usd: Option<f64>,
    /// Locally estimated subagent spend in USD (subset of `estimated_cost_usd`).
    pub estimated_subagent_cost_usd: Option<f64>,
    /// Recent retry anomaly summary (`429`/`503`/timeout/5xx) sniffed from `gen_metadata`.
    pub retry_anomaly: Option<RetryAnomalySummary>,
}

/// Trait abstracting session generation metadata lookup for testability.
pub trait SpeedReader {
    /// Returns session generation speed, TTFT, and context window metadata.
    fn session_metadata(&self, conversation_id: &str) -> SessionMetadata;
}

/// Production [`SpeedReader`] backed by an AGY CLI directory (`<cli_dir>/conversations/*.db`).
#[derive(Debug, Clone)]
pub struct SqliteSpeedReader {
    /// AGY CLI data directory containing `conversations/<uuid>.db`.
    pub cli_dir: PathBuf,
}

impl SqliteSpeedReader {
    /// Constructs a reader targeting an explicit AGY CLI data directory.
    pub fn new(cli_dir: impl Into<PathBuf>) -> Self {
        Self {
            cli_dir: cli_dir.into(),
        }
    }
}

impl SpeedReader for SqliteSpeedReader {
    fn session_metadata(&self, conversation_id: &str) -> SessionMetadata {
        query_session_metadata_in(&self.cli_dir, conversation_id)
    }
}

pub(crate) fn sidecar_path(db_path: &Path, suffix: &str) -> PathBuf {
    let mut os = db_path.as_os_str().to_os_string();
    os.push(suffix);
    PathBuf::from(os)
}

/// Safety classification for a SQLite database file and its `-wal`/`-shm` sidecars.
#[derive(Debug)]
enum SqlitePathStatus {
    /// Regular non-symlink database file whose `-wal`/`-shm` sidecars are absent or regular files.
    Valid(fs::Metadata),
    /// Database file is absent and its `-wal`/`-shm` sidecars are absent or regular files.
    Missing,
    /// Database path or a `-wal`/`-shm` sidecar is a symlink, non-regular file, or unreadable.
    Unsafe,
}

/// Inspects `db_path` and its `-wal`/`-shm` sidecars without following symlinks.
fn inspect_sqlite_path(db_path: &Path) -> SqlitePathStatus {
    let base = match fs::symlink_metadata(db_path) {
        Ok(meta) => {
            let ft = meta.file_type();
            if ft.is_symlink() || !ft.is_file() {
                return SqlitePathStatus::Unsafe;
            }
            SqlitePathStatus::Valid(meta)
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => SqlitePathStatus::Missing,
        Err(_) => return SqlitePathStatus::Unsafe,
    };
    if !is_absent_or_real_file(&sidecar_path(db_path, "-wal"))
        || !is_absent_or_real_file(&sidecar_path(db_path, "-shm"))
    {
        return SqlitePathStatus::Unsafe;
    }
    base
}

/// Returns `db_path`'s `symlink_metadata` when `db_path` is a safe regular file with safe sidecars.
pub(crate) fn safe_sqlite_metadata(db_path: &Path) -> Option<fs::Metadata> {
    match inspect_sqlite_path(db_path) {
        SqlitePathStatus::Valid(meta) => Some(meta),
        SqlitePathStatus::Missing | SqlitePathStatus::Unsafe => None,
    }
}

pub(crate) fn is_safe_sqlite_file(db_path: &Path) -> bool {
    safe_sqlite_metadata(db_path).is_some()
}

/// Resolves `<cli_dir>/conversations/<conversation_id>.db`, falling back to lowercase
/// `<conversation_id_lower>.db` on case-sensitive filesystems only when the exact-case
/// file is absent.
///
/// Rejects symlinks and non-regular files for `conversations/`, the target `.db`,
/// and any existing `-wal`/`-shm` sidecars.
pub(crate) fn resolve_conversation_db_path(
    cli_dir: &Path,
    conversation_id: &str,
) -> Option<PathBuf> {
    if !is_valid_uuid(conversation_id) {
        return None;
    }
    let conv_dir = cli_dir.join("conversations");
    if !is_real_dir(&conv_dir) {
        return None;
    }
    let exact = conv_dir.join(format!("{conversation_id}.db"));
    match inspect_sqlite_path(&exact) {
        SqlitePathStatus::Valid(_) => Some(exact),
        SqlitePathStatus::Unsafe => None,
        SqlitePathStatus::Missing => {
            let lower = conversation_id.to_ascii_lowercase();
            if lower != conversation_id {
                let candidate = conv_dir.join(format!("{lower}.db"));
                is_safe_sqlite_file(&candidate).then_some(candidate)
            } else {
                None
            }
        }
    }
}

/// Validates canonical `8-4-4-4-12` lowercase/uppercase hexadecimal UUID strings.
///
/// # Examples
///
/// ```
/// use agy_statusline::agy::db::is_valid_uuid;
///
/// assert!(is_valid_uuid("8d0371e1-5a05-423b-a8bc-ed75995f9a8b"));
/// assert!(!is_valid_uuid("------------------------------------"));
/// assert!(!is_valid_uuid("not-a-uuid"));
/// ```
pub fn is_valid_uuid(id: &str) -> bool {
    let bytes = id.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    for (i, &b) in bytes.iter().enumerate() {
        let is_hyphen_pos = matches!(i, 8 | 13 | 18 | 23);
        if is_hyphen_pos {
            if b != b'-' {
                return false;
            }
        } else if !b.is_ascii_hexdigit() {
            return false;
        }
    }
    true
}

/// Extracts the canonical lowercase UUID from a `<uuid>.db` file path when its stem is a valid UUID.
pub(crate) fn conversation_id_from_db_path(db_path: &Path) -> Option<String> {
    let stem = db_path.file_stem()?.to_str()?.trim();
    is_valid_uuid(stem).then(|| stem.to_ascii_lowercase())
}

/// Zero-lock 100-byte SQLite header readiness check run before `sqlite3_open_v2`.
///
/// Rejects symlinks, files shorter than 100 bytes, non-SQLite magic headers, non-empty or
/// symlinked rollback `<db>-journal` sidecars, and uninitialized databases whose schema has
/// neither been committed to the main DB (`page_count >= 2 && schema_cookie > 0`) nor written
/// to a non-empty WAL sidecar (`wal_size > 32`).
pub fn is_sqlite_db_ready_for_read(db_path: &Path) -> bool {
    let Some(meta) = safe_sqlite_metadata(db_path) else {
        return false;
    };
    if meta.len() < SQLITE_HEADER_BYTES as u64 {
        return false;
    }

    match fs::symlink_metadata(sidecar_path(db_path, "-journal")) {
        Ok(journal_meta) => {
            let ft = journal_meta.file_type();
            if ft.is_symlink() || !ft.is_file() || journal_meta.len() > 0 {
                return false;
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(_) => return false,
    }

    let Ok(mut file) = fs::File::open(db_path) else {
        return false;
    };
    let mut header = [0u8; SQLITE_HEADER_BYTES];
    if file.read_exact(&mut header).is_err() {
        return false;
    }
    if &header[0..16] != SQLITE_MAGIC_PREFIX {
        return false;
    }

    let page_count = u32::from_be_bytes([header[28], header[29], header[30], header[31]]);
    let schema_cookie = u32::from_be_bytes([header[40], header[41], header[42], header[43]]);
    if page_count >= 2 && schema_cookie > 0 {
        return true;
    }

    match fs::symlink_metadata(sidecar_path(db_path, "-wal")) {
        Ok(wal_meta) => {
            let ft = wal_meta.file_type();
            !ft.is_symlink() && ft.is_file() && wal_meta.len() > SQLITE_WAL_HEADER_BYTES
        }
        Err(_) => false,
    }
}

/// Opens a SQLite database in read-only, no-mutex mode with a 25ms busy timeout,
/// guarded by the zero-lock 100-byte header readiness check.
pub(crate) fn open_readonly_db(path: &Path) -> rusqlite::Result<Connection> {
    if !is_sqlite_db_ready_for_read(path) {
        return Err(rusqlite::Error::InvalidPath(path.to_path_buf()));
    }
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(path, flags)?;
    let _ = conn.busy_timeout(Duration::from_millis(25));
    Ok(conn)
}

/// Computes the exact median of finite values in `values`, returning `None` if empty.
pub(crate) fn compute_median(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let mid = sorted.len() / 2;
    if sorted.len().is_multiple_of(2) {
        Some((sorted[mid - 1] + sorted[mid]) / 2.0)
    } else {
        Some(sorted[mid])
    }
}

/// Extracts speed, TTFT, context window, and retry anomaly metadata from an already-open
/// read-only connection, inferring the expected conversation UUID from the DB filename to skip
/// cloned fork/side-question rows.
pub(crate) fn query_session_metadata_from_conn(conn: &Connection) -> SessionMetadata {
    let expected_id = conn
        .path()
        .and_then(|p| conversation_id_from_db_path(Path::new(p)));
    query_session_metadata_from_conn_for_id(conn, expected_id.as_deref())
}

fn record_retry_anomaly(summary: &mut RetryAnomalySummary, attempt: &RetryAttemptRecord) {
    if !attempt.is_anomaly() {
        return;
    }
    if summary.retry_count == 0 {
        summary.latest_class = attempt.status_class;
        summary.latest_status_code = attempt.status_code;
    }
    summary.retry_count = summary.retry_count.saturating_add(1);
    match attempt.status_class {
        RetryStatusClass::RateLimit429 => {
            summary.retries_429 = summary.retries_429.saturating_add(1);
        }
        RetryStatusClass::Capacity503 => {
            summary.retries_503 = summary.retries_503.saturating_add(1);
        }
        _ => {
            summary.retries_other = summary.retries_other.saturating_add(1);
        }
    }
    if let Some(delay) = attempt
        .retry_delay_secs
        .filter(|d| d.is_finite() && *d > 0.0)
    {
        summary.total_retry_delay_secs += delay;
    }
}

fn query_session_metadata_from_conn_for_id(
    conn: &Connection,
    expected_conversation_id: Option<&str>,
) -> SessionMetadata {
    let mut meta = SessionMetadata::default();
    let Ok(mut stmt) = conn
        .prepare("SELECT data FROM gen_metadata WHERE data IS NOT NULL ORDER BY idx DESC LIMIT 32")
    else {
        return meta;
    };
    let Ok(mut rows) = stmt.query([]) else {
        return meta;
    };

    let mut turns: Vec<TurnMetrics> = Vec::with_capacity(9);
    let mut own_rows_seen = 0usize;
    let mut retry_summary = RetryAnomalySummary::default();
    while let Ok(Some(row)) = rows.next() {
        let Ok(blob) = row.get::<_, Vec<u8>>(0) else {
            continue;
        };
        let Some(summary) = extract_gen_metadata_summary(&blob) else {
            continue;
        };
        if is_foreign_cascade_id(
            summary.origin_cascade_id.as_deref(),
            expected_conversation_id,
        ) {
            continue;
        }
        own_rows_seen += 1;
        if let Some(tm) = summary.turn {
            turns.push(tm);
        }
        meta.max_context_tokens = meta.max_context_tokens.or(summary.max_context_tokens);
        meta.estimated_context_tokens = meta
            .estimated_context_tokens
            .or(summary.estimated_context_tokens);
        for attempt in summary.retry_attempts.iter().rev() {
            record_retry_anomaly(&mut retry_summary, attempt);
        }
        if own_rows_seen >= 9 {
            break;
        }
    }

    if retry_summary.retry_count > 0 {
        meta.retry_anomaly = Some(retry_summary);
    }

    if let Some(first) = turns.first() {
        meta.last_turn_speed = Some((first.output_tokens as f64) / first.decode_secs);
        meta.ttft_secs = first.ttft_secs;
        meta.e2e_speed = first
            .ttft_secs
            .map(|t| (first.output_tokens as f64) / (t + first.decode_secs));
        meta.thinking_tokens = first.thinking_tokens;

        let total_tokens: u64 = turns
            .iter()
            .take(5)
            .map(|t| t.output_tokens)
            .fold(0u64, u64::saturating_add);
        let total_secs: f64 = turns.iter().take(5).map(|t| t.decode_secs).sum();
        if total_secs > 0.0 {
            meta.speed = Some((total_tokens as f64) / total_secs);
        }

        let speeds: Vec<f64> = turns
            .iter()
            .map(|t| (t.output_tokens as f64) / t.decode_secs)
            .collect();
        meta.median_speed = compute_median(&speeds);
    }
    meta
}

/// Resolves `<cli_dir>/conversations/<conversation_id>.db` and extracts session metadata.
pub(crate) fn query_session_metadata_in(cli_dir: &Path, conversation_id: &str) -> SessionMetadata {
    let Some(db_path) = resolve_conversation_db_path(cli_dir, conversation_id) else {
        return SessionMetadata::default();
    };
    let Ok(conn) = open_readonly_db(&db_path) else {
        return SessionMetadata::default();
    };
    query_session_metadata_from_conn_for_id(&conn, Some(conversation_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agy::protobuf::{encode_gen_blob, encode_speed_1s_blob, GenBlobSpec};

    #[test]
    fn uuid_validation() {
        assert!(is_valid_uuid("8d0371e1-5a05-423b-a8bc-ed75995f9a8b"));
        assert!(is_valid_uuid("8D0371E1-5A05-423B-A8BC-ED75995F9A8B"));
        assert!(!is_valid_uuid("------------------------------------"));
        assert!(!is_valid_uuid("8d0371e15a05-423b-a8bc-ed75995f9a8b0"));
        assert!(!is_valid_uuid("8d0371e1-5a05-423b-a8bc-ed75995f9a8g"));
    }

    #[test]
    fn query_real_sqlite_db_in_temp_dir() {
        let (dir, conn, uuid) = create_temp_conv_db();
        let blob = encode_gen_blob(GenBlobSpec {
            output_tok: 100,
            dur_secs: 2,
            ..Default::default()
        });
        conn.execute(
            "INSERT INTO gen_metadata (idx, data) VALUES (1, ?1)",
            [blob],
        )
        .unwrap();
        drop(conn);

        let reader = SqliteSpeedReader::new(dir.path());
        assert_eq!(reader.session_metadata(uuid).speed, Some(50.0));
        assert_eq!(reader.session_metadata("invalid").speed, None);
    }

    #[test]
    fn falls_back_when_newest_row_is_incomplete() {
        let (dir, conn, uuid) = create_temp_conv_db();
        let valid = encode_gen_blob(GenBlobSpec {
            output_tok: 100,
            dur_secs: 2,
            ..Default::default()
        });
        let incomplete: &[u8] = &[0x12, 0x00];
        conn.execute(
            "INSERT INTO gen_metadata (idx, data) VALUES (1, ?1)",
            [valid],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO gen_metadata (idx, data) VALUES (2, ?1)",
            [incomplete],
        )
        .unwrap();
        drop(conn);

        let reader = SqliteSpeedReader::new(dir.path());
        assert_eq!(reader.session_metadata(uuid).speed, Some(50.0));
    }

    fn create_temp_conv_db() -> (tempfile::TempDir, Connection, &'static str) {
        let dir = tempfile::tempdir().unwrap();
        let conv_dir = dir.path().join("conversations");
        std::fs::create_dir_all(&conv_dir).unwrap();
        let uuid = "8d0371e1-5a05-423b-a8bc-ed75995f9a8b";
        let db_path = conv_dir.join(format!("{uuid}.db"));
        let conn = Connection::open(&db_path).unwrap();
        conn.execute(
            "CREATE TABLE gen_metadata (idx INTEGER PRIMARY KEY, data BLOB)",
            [],
        )
        .unwrap();
        (dir, conn, uuid)
    }

    #[test]
    fn median_speed_odd_nine_turns_and_weighted_five_turns() {
        let (dir, conn, uuid) = create_temp_conv_db();
        conn.execute(
            "INSERT INTO gen_metadata (idx, data) VALUES (0, ?1)",
            [encode_speed_1s_blob(5)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO gen_metadata (idx, data) VALUES (1, ?1)",
            [encode_speed_1s_blob(10)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO gen_metadata (idx, data) VALUES (2, ?1)",
            [encode_speed_1s_blob(20)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO gen_metadata (idx, data) VALUES (3, ?1)",
            [encode_speed_1s_blob(30)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO gen_metadata (idx, data) VALUES (4, ?1)",
            [encode_speed_1s_blob(40)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO gen_metadata (idx, data) VALUES (5, ?1)",
            [encode_speed_1s_blob(80)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO gen_metadata (idx, data) VALUES (6, ?1)",
            [encode_speed_1s_blob(90)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO gen_metadata (idx, data) VALUES (7, ?1)",
            [encode_speed_1s_blob(100)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO gen_metadata (idx, data) VALUES (8, ?1)",
            [encode_speed_1s_blob(110)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO gen_metadata (idx, data) VALUES (9, ?1)",
            [encode_speed_1s_blob(120)],
        )
        .unwrap();
        drop(conn);

        let reader = SqliteSpeedReader::new(dir.path());
        let meta = reader.session_metadata(uuid);
        assert_eq!(meta.speed, Some(100.0));
        assert_eq!(meta.median_speed, Some(80.0));
        assert_eq!(meta.last_turn_speed, Some(120.0));
    }

    #[test]
    fn median_speed_even_turns_averages_middle_two() {
        let (dir, conn, uuid) = create_temp_conv_db();
        conn.execute(
            "INSERT INTO gen_metadata (idx, data) VALUES (1, ?1)",
            [encode_speed_1s_blob(100)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO gen_metadata (idx, data) VALUES (2, ?1)",
            [encode_speed_1s_blob(20)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO gen_metadata (idx, data) VALUES (3, ?1)",
            [encode_speed_1s_blob(60)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO gen_metadata (idx, data) VALUES (4, ?1)",
            [encode_speed_1s_blob(40)],
        )
        .unwrap();
        drop(conn);

        let reader = SqliteSpeedReader::new(dir.path());
        let meta = reader.session_metadata(uuid);
        assert_eq!(meta.median_speed, Some(50.0));
        assert_eq!(meta.speed, Some(55.0));
    }

    #[test]
    fn populates_estimated_context_and_thinking_tokens() {
        let (dir, conn, uuid) = create_temp_conv_db();
        let turn_with_thinking = encode_gen_blob(GenBlobSpec {
            output_tok: 350,
            thinking_tok: 250,
            dur_secs: 2,
            ..Default::default()
        });
        let ctx_start = encode_gen_blob(GenBlobSpec {
            estimated_context_tokens: 64_000,
            max_context_tokens: 512_000,
            ..Default::default()
        });
        conn.execute(
            "INSERT INTO gen_metadata (idx, data) VALUES (1, ?1)",
            [turn_with_thinking],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO gen_metadata (idx, data) VALUES (2, ?1)",
            [ctx_start],
        )
        .unwrap();
        drop(conn);

        let reader = SqliteSpeedReader::new(dir.path());
        let meta = reader.session_metadata(uuid);
        assert_eq!(meta.estimated_context_tokens, Some(64_000));
        assert_eq!(meta.max_context_tokens, Some(512_000));
        assert_eq!(meta.thinking_tokens, Some(250));
        assert_eq!(meta.speed, Some(175.0));
    }

    #[test]
    fn compute_median_handles_empty_odd_and_even() {
        assert_eq!(compute_median(&[]), None);
        assert_eq!(compute_median(&[30.0, 10.0, 20.0]), Some(20.0));
        assert_eq!(compute_median(&[40.0, 10.0, 20.0, 30.0]), Some(25.0));
    }

    #[test]
    fn resolve_conversation_db_path_falls_back_to_lowercase_uuid() {
        let (dir, conn, uuid) = create_temp_conv_db();
        drop(conn);
        let cli_dir = dir.path();
        let upper = uuid.to_ascii_uppercase();

        let exact = resolve_conversation_db_path(cli_dir, uuid).unwrap();
        let via_upper = resolve_conversation_db_path(cli_dir, &upper).unwrap();
        assert_eq!(exact, via_upper);
        assert!(resolve_conversation_db_path(cli_dir, "not-a-uuid").is_none());
    }

    #[test]
    fn resolve_conversation_db_path_accepts_regular_sidecars_and_rejects_non_files() {
        let (dir, conn, uuid) = create_temp_conv_db();
        drop(conn);
        let cli_dir = dir.path();
        let conv_dir = cli_dir.join("conversations");

        fs::write(conv_dir.join(format!("{uuid}.db-wal")), b"wal").unwrap();
        fs::write(conv_dir.join(format!("{uuid}.db-shm")), b"shm").unwrap();
        assert!(resolve_conversation_db_path(cli_dir, uuid).is_some());

        let dir_uuid = "11111111-1111-4111-8111-111111111111";
        fs::create_dir(conv_dir.join(format!("{dir_uuid}.db"))).unwrap();
        assert!(resolve_conversation_db_path(cli_dir, dir_uuid).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn resolve_conversation_db_path_rejects_symlinks_for_dir_db_and_sidecars() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let cli_dir = dir.path().join("cli");
        let conv_dir = cli_dir.join("conversations");
        let external = dir.path().join("external");
        fs::create_dir_all(&conv_dir).unwrap();
        fs::create_dir_all(&external).unwrap();

        let target_db = external.join("target.db");
        fs::write(&target_db, b"sqlite").unwrap();
        let target_sidecar = external.join("sidecar");
        fs::write(&target_sidecar, b"sidecar").unwrap();

        // Symlinked <uuid>.db is rejected (exact and uppercase lookup).
        let sym_uuid = "22222222-2222-4222-8222-222222222222";
        symlink(&target_db, conv_dir.join(format!("{sym_uuid}.db"))).unwrap();
        assert!(resolve_conversation_db_path(&cli_dir, sym_uuid).is_none());
        assert!(resolve_conversation_db_path(&cli_dir, &sym_uuid.to_ascii_uppercase()).is_none());

        // Real <uuid>.db with symlinked -wal sidecar is rejected.
        let wal_uuid = "33333333-3333-4333-8333-333333333333";
        fs::write(conv_dir.join(format!("{wal_uuid}.db")), b"sqlite").unwrap();
        symlink(&target_sidecar, conv_dir.join(format!("{wal_uuid}.db-wal"))).unwrap();
        assert!(resolve_conversation_db_path(&cli_dir, wal_uuid).is_none());

        // Real <uuid>.db with symlinked -shm sidecar is rejected.
        let shm_uuid = "44444444-4444-4444-8444-444444444444";
        fs::write(conv_dir.join(format!("{shm_uuid}.db")), b"sqlite").unwrap();
        symlink(&target_sidecar, conv_dir.join(format!("{shm_uuid}.db-shm"))).unwrap();
        assert!(resolve_conversation_db_path(&cli_dir, shm_uuid).is_none());

        // Symlinked conversations/ directory is rejected.
        let sym_cli = dir.path().join("sym_cli");
        let real_conv = dir.path().join("real_conv");
        fs::create_dir_all(&sym_cli).unwrap();
        fs::create_dir_all(&real_conv).unwrap();
        let ok_uuid = "55555555-5555-4555-8555-555555555555";
        fs::write(real_conv.join(format!("{ok_uuid}.db")), b"sqlite").unwrap();
        symlink(&real_conv, sym_cli.join("conversations")).unwrap();
        assert!(resolve_conversation_db_path(&sym_cli, ok_uuid).is_none());

        // Uppercase symlink or poisoned sidecar must never fall back to a lowercase real DB.
        let mixed_lower = "66666666-6666-4666-8666-66666666666a";
        let mixed_upper = mixed_lower.to_ascii_uppercase();
        fs::write(conv_dir.join(format!("{mixed_lower}.db")), b"sqlite").unwrap();
        symlink(&target_db, conv_dir.join(format!("{mixed_upper}.db"))).unwrap();
        assert!(resolve_conversation_db_path(&cli_dir, &mixed_upper).is_none());

        let wal_mixed_lower = "77777777-7777-4777-8777-77777777777b";
        let wal_mixed_upper = wal_mixed_lower.to_ascii_uppercase();
        fs::write(conv_dir.join(format!("{wal_mixed_lower}.db")), b"sqlite").unwrap();
        fs::write(conv_dir.join(format!("{wal_mixed_upper}.db")), b"sqlite").unwrap();
        symlink(
            &target_sidecar,
            conv_dir.join(format!("{wal_mixed_upper}.db-wal")),
        )
        .unwrap();
        assert!(resolve_conversation_db_path(&cli_dir, &wal_mixed_upper).is_none());

        // Missing DB with clean sidecars is Missing; dangling symlink and
        // missing DB with symlinked sidecars are Unsafe.
        let missing_db = conv_dir.join("absent.db");
        assert!(matches!(
            inspect_sqlite_path(&missing_db),
            SqlitePathStatus::Missing
        ));
        assert!(!is_safe_sqlite_file(&missing_db));

        let dangling_db = conv_dir.join("dangling.db");
        symlink(external.join("does_not_exist.db"), &dangling_db).unwrap();
        assert!(matches!(
            inspect_sqlite_path(&dangling_db),
            SqlitePathStatus::Unsafe
        ));

        let poisoned_missing = conv_dir.join("poisoned_missing.db");
        symlink(&target_sidecar, sidecar_path(&poisoned_missing, "-wal")).unwrap();
        assert!(matches!(
            inspect_sqlite_path(&poisoned_missing),
            SqlitePathStatus::Unsafe
        ));
    }

    #[test]
    fn query_session_metadata_skips_foreign_fork_rows_and_tracks_retry_anomaly() {
        use crate::agy::protobuf::RetrySpec;

        let (dir, conn, uuid) = create_temp_conv_db();
        let parent_uuid = "11111111-1111-4111-8111-111111111111";

        let own_blob = encode_gen_blob(GenBlobSpec {
            origin_cascade_id: Some(uuid),
            output_tok: 200,
            dur_secs: 2,
            ttft_secs: 1,
            max_context_tokens: 256_000,
            estimated_context_tokens: 32_000,
            retry_records: vec![
                RetrySpec {
                    error: Some("HTTP 429 rate limit"),
                    reason_code: 429,
                    delay_secs: 1,
                    delay_nanos: 0,
                    ..Default::default()
                },
                RetrySpec {
                    error: Some("HTTP 503 overloaded"),
                    reason_code: 503,
                    delay_secs: 2,
                    delay_nanos: 500_000_000,
                    ..Default::default()
                },
            ],
            ..Default::default()
        });
        let foreign_blob = encode_gen_blob(GenBlobSpec {
            origin_cascade_id: Some(parent_uuid),
            output_tok: 900,
            dur_secs: 1,
            max_context_tokens: 999_000,
            estimated_context_tokens: 888_000,
            retry_records: vec![RetrySpec {
                error: Some("HTTP 500 internal"),
                reason_code: 500,
                delay_secs: 9,
                delay_nanos: 0,
                ..Default::default()
            }],
            ..Default::default()
        });
        conn.execute(
            "INSERT INTO gen_metadata (idx, data) VALUES (1, ?1)",
            [own_blob],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO gen_metadata (idx, data) VALUES (2, ?1)",
            [foreign_blob],
        )
        .unwrap();
        drop(conn);

        let reader = SqliteSpeedReader::new(dir.path());
        let meta = reader.session_metadata(uuid);
        assert_eq!(meta.speed, Some(100.0));
        assert_eq!(meta.max_context_tokens, Some(256_000));
        assert_eq!(meta.estimated_context_tokens, Some(32_000));
        let anomaly = meta.retry_anomaly.unwrap();
        assert_eq!(anomaly.retry_count, 2);
        assert_eq!(anomaly.retries_429, 1);
        assert_eq!(anomaly.retries_503, 1);
        assert_eq!(anomaly.retries_other, 0);
        assert!((anomaly.total_retry_delay_secs - 3.5).abs() < 1e-6);
        assert_eq!(anomaly.latest_class, RetryStatusClass::Capacity503);
        assert_eq!(anomaly.latest_status_code, Some(503));
    }
}
