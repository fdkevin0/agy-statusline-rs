//! Read-only SQLite conversation database reader and UUID validation.
//!
//! Queries `<agy_cli_dir>/conversations/<conversation_id>.db` for recent
//! `gen_metadata` rows and extracts decode/e2e speed, TTFT, thinking tokens,
//! and context window metadata.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};

use crate::paths::resolve_agy_cli_dir;
use crate::protobuf::{extract_gen_metadata_summary, TurnMetrics};

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
}

/// Trait abstracting session generation metadata lookup for testability.
pub trait SpeedReader {
    /// Returns session generation speed, TTFT, and context window metadata.
    fn session_metadata(&self, conversation_id: &str) -> SessionMetadata;
}

/// Production [`SpeedReader`] backed by the local AGY SQLite conversation database.
#[derive(Debug, Clone, Default)]
pub struct SqliteSpeedReader {
    /// Optional override for the base home directory (used in isolated tests).
    pub home_override: Option<PathBuf>,
}

impl SpeedReader for SqliteSpeedReader {
    fn session_metadata(&self, conversation_id: &str) -> SessionMetadata {
        let Some(cli_dir) = self
            .home_override
            .as_ref()
            .map(|home| home.join(".gemini").join("antigravity-cli"))
            .or_else(resolve_agy_cli_dir)
        else {
            return SessionMetadata::default();
        };
        let Some(db_path) = resolve_conversation_db_path(&cli_dir, conversation_id) else {
            return SessionMetadata::default();
        };
        query_session_metadata_from_db(&db_path)
    }
}

/// Resolves `<cli_dir>/conversations/<conversation_id>.db`, falling back to lowercase
/// `<conversation_id_lower>.db` on case-sensitive filesystems.
pub(crate) fn resolve_conversation_db_path(cli_dir: &Path, conversation_id: &str) -> Option<PathBuf> {
    if !is_valid_uuid(conversation_id) {
        return None;
    }
    let conv_dir = cli_dir.join("conversations");
    let exact = conv_dir.join(format!("{conversation_id}.db"));
    if exact.is_file() {
        return Some(exact);
    }
    let lower = conversation_id.to_ascii_lowercase();
    if lower != conversation_id {
        let lower_path = conv_dir.join(format!("{lower}.db"));
        if lower_path.is_file() {
            return Some(lower_path);
        }
    }
    None
}

/// Validates canonical `8-4-4-4-12` lowercase/uppercase hexadecimal UUID strings.
///
/// # Examples
///
/// ```
/// use agy_statusline::db::is_valid_uuid;
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

/// Opens a SQLite database in read-only, no-mutex mode with a 25ms busy timeout.
pub(crate) fn open_readonly_db(path: &Path) -> rusqlite::Result<Connection> {
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

/// Opens `db_path` in read-only mode and extracts speed, TTFT, and context window metadata.
fn query_session_metadata_from_db(db_path: &Path) -> SessionMetadata {
    let mut meta = SessionMetadata::default();
    let Ok(conn) = open_readonly_db(db_path) else {
        return meta;
    };
    let Ok(mut stmt) =
        conn.prepare("SELECT data FROM gen_metadata WHERE data IS NOT NULL ORDER BY idx DESC LIMIT 9")
    else {
        return meta;
    };
    let Ok(mut rows) = stmt.query([]) else {
        return meta;
    };

    let mut turns: Vec<TurnMetrics> = Vec::with_capacity(9);
    while let Ok(Some(row)) = rows.next() {
        if let Ok(blob) = row.get::<_, Vec<u8>>(0) {
            if let Some(summary) = extract_gen_metadata_summary(&blob) {
                if let Some(tm) = summary.turn {
                    turns.push(tm);
                }
                meta.max_context_tokens = meta.max_context_tokens.or(summary.max_context_tokens);
                meta.estimated_context_tokens = meta
                    .estimated_context_tokens
                    .or(summary.estimated_context_tokens);
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

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
        let blob: &[u8] = &[0x0a, 0x08, 0x22, 0x02, 0x18, 0x64, 0x62, 0x02, 0x08, 0x02];
        conn.execute("INSERT INTO gen_metadata (idx, data) VALUES (1, ?1)", [blob])
            .unwrap();
        drop(conn);

        let reader = SqliteSpeedReader {
            home_override: Some(dir.path().to_path_buf()),
        };
        assert_eq!(reader.session_metadata(uuid).speed, Some(50.0));
        assert_eq!(reader.session_metadata("invalid").speed, None);
    }

    #[test]
    fn falls_back_when_newest_row_is_incomplete() {
        let (dir, conn, uuid) = create_temp_conv_db();
        let valid: &[u8] = &[0x0a, 0x08, 0x22, 0x02, 0x18, 0x64, 0x62, 0x02, 0x08, 0x02];
        let incomplete: &[u8] = &[0x12, 0x00];
        conn.execute("INSERT INTO gen_metadata (idx, data) VALUES (1, ?1)", [valid])
            .unwrap();
        conn.execute(
            "INSERT INTO gen_metadata (idx, data) VALUES (2, ?1)",
            [incomplete],
        )
        .unwrap();
        drop(conn);

        let reader = SqliteSpeedReader {
            home_override: Some(dir.path().to_path_buf()),
        };
        assert_eq!(reader.session_metadata(uuid).speed, Some(50.0));
    }

    fn speed_1s_blob(output_tokens: u8) -> [u8; 10] {
        [0x0a, 0x08, 0x22, 0x02, 0x18, output_tokens, 0x62, 0x02, 0x08, 0x01]
    }

    fn create_temp_conv_db() -> (tempfile::TempDir, Connection, &'static str) {
        let dir = tempfile::tempdir().unwrap();
        let conv_dir = dir.path().join(".gemini/antigravity-cli/conversations");
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
        conn.execute("INSERT INTO gen_metadata (idx, data) VALUES (0, ?1)", [speed_1s_blob(5)]).unwrap();
        conn.execute("INSERT INTO gen_metadata (idx, data) VALUES (1, ?1)", [speed_1s_blob(10)]).unwrap();
        conn.execute("INSERT INTO gen_metadata (idx, data) VALUES (2, ?1)", [speed_1s_blob(20)]).unwrap();
        conn.execute("INSERT INTO gen_metadata (idx, data) VALUES (3, ?1)", [speed_1s_blob(30)]).unwrap();
        conn.execute("INSERT INTO gen_metadata (idx, data) VALUES (4, ?1)", [speed_1s_blob(40)]).unwrap();
        conn.execute("INSERT INTO gen_metadata (idx, data) VALUES (5, ?1)", [speed_1s_blob(80)]).unwrap();
        conn.execute("INSERT INTO gen_metadata (idx, data) VALUES (6, ?1)", [speed_1s_blob(90)]).unwrap();
        conn.execute("INSERT INTO gen_metadata (idx, data) VALUES (7, ?1)", [speed_1s_blob(100)]).unwrap();
        conn.execute("INSERT INTO gen_metadata (idx, data) VALUES (8, ?1)", [speed_1s_blob(110)]).unwrap();
        conn.execute("INSERT INTO gen_metadata (idx, data) VALUES (9, ?1)", [speed_1s_blob(120)]).unwrap();
        drop(conn);

        let reader = SqliteSpeedReader {
            home_override: Some(dir.path().to_path_buf()),
        };
        let meta = reader.session_metadata(uuid);
        assert_eq!(meta.speed, Some(100.0));
        assert_eq!(meta.median_speed, Some(80.0));
        assert_eq!(meta.last_turn_speed, Some(120.0));
    }

    #[test]
    fn median_speed_even_turns_averages_middle_two() {
        let (dir, conn, uuid) = create_temp_conv_db();
        conn.execute("INSERT INTO gen_metadata (idx, data) VALUES (1, ?1)", [speed_1s_blob(100)]).unwrap();
        conn.execute("INSERT INTO gen_metadata (idx, data) VALUES (2, ?1)", [speed_1s_blob(20)]).unwrap();
        conn.execute("INSERT INTO gen_metadata (idx, data) VALUES (3, ?1)", [speed_1s_blob(60)]).unwrap();
        conn.execute("INSERT INTO gen_metadata (idx, data) VALUES (4, ?1)", [speed_1s_blob(40)]).unwrap();
        drop(conn);

        let reader = SqliteSpeedReader {
            home_override: Some(dir.path().to_path_buf()),
        };
        let meta = reader.session_metadata(uuid);
        assert_eq!(meta.median_speed, Some(50.0));
        assert_eq!(meta.speed, Some(55.0));
    }

    #[test]
    fn populates_estimated_context_and_thinking_tokens() {
        let (dir, conn, uuid) = create_temp_conv_db();
        // idx 1: output=100, thinking=250, dur=2s
        let turn_with_thinking: &[u8] = &[
            0x0a, 0x0b, 0x22, 0x05, 0x18, 0x64, 0x48, 0xfa, 0x01, 0x62, 0x02, 0x08, 0x02,
        ];
        // idx 2: estimated_tokens_used=64_000, max_context_tokens=512_000 (no usage/duration yet)
        let ctx_start: &[u8] = &[
            0x0a, 0x0c, 0x4a, 0x0a, 0x52, 0x08, 0x08, 0x80, 0xf4, 0x03, 0x20, 0x80, 0xa0, 0x1f,
        ];
        conn.execute("INSERT INTO gen_metadata (idx, data) VALUES (1, ?1)", [turn_with_thinking])
            .unwrap();
        conn.execute("INSERT INTO gen_metadata (idx, data) VALUES (2, ?1)", [ctx_start])
            .unwrap();
        drop(conn);

        let reader = SqliteSpeedReader {
            home_override: Some(dir.path().to_path_buf()),
        };
        let meta = reader.session_metadata(uuid);
        assert_eq!(meta.estimated_context_tokens, Some(64_000));
        assert_eq!(meta.max_context_tokens, Some(512_000));
        assert_eq!(meta.thinking_tokens, Some(250));
        assert_eq!(meta.speed, Some(50.0));
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
        let cli_dir = dir.path().join(".gemini/antigravity-cli");
        let upper = uuid.to_ascii_uppercase();

        let exact = resolve_conversation_db_path(&cli_dir, uuid).unwrap();
        let via_upper = resolve_conversation_db_path(&cli_dir, &upper).unwrap();
        assert_eq!(exact, via_upper);
        assert!(resolve_conversation_db_path(&cli_dir, "not-a-uuid").is_none());
    }
}
