//! Read-only SQLite conversation database reader and UUID validation.
//!
//! Queries `~/.gemini/antigravity-cli/conversations/<conversation_id>.db`
//! for the most recent `gen_metadata` blob and extracts generation speed.

use std::env;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

use crate::protobuf::{extract_max_context_tokens, extract_turn_metrics, TurnMetrics};

/// Session generation metadata sniffed from `gen_metadata`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SessionMetadata {
    /// 5-turn token-weighted average decode speed (`sum(output) / sum(decode_secs)`).
    pub speed: Option<f64>,
    /// Latest single completed turn decode speed (`output / decode_secs`).
    pub last_turn_speed: Option<f64>,
    /// Latest turn end-to-end throughput (`output / (ttft + decode_secs)`).
    pub e2e_speed: Option<f64>,
    /// Latest turn time-to-first-token in seconds.
    pub ttft_secs: Option<f64>,
    /// Server-issued compression context threshold (`max_context_tokens`).
    pub max_context_tokens: Option<u64>,
}

/// Trait abstracting last-turn speed and context limit lookup for testability.
pub trait SpeedReader {
    /// Returns the last completed turn's generation speed in tokens/sec, if available.
    fn last_speed(&self, conversation_id: &str) -> Option<f64>;

    /// Returns both generation speed and server-issued `max_context_tokens`.
    fn session_metadata(&self, conversation_id: &str) -> SessionMetadata {
        let speed = self.last_speed(conversation_id);
        SessionMetadata {
            speed,
            last_turn_speed: speed,
            e2e_speed: None,
            ttft_secs: None,
            max_context_tokens: None,
        }
    }
}

/// Production [`SpeedReader`] backed by the local AGY SQLite conversation database.
#[derive(Debug, Clone, Default)]
pub struct SqliteSpeedReader {
    /// Optional override for the base home directory (used in isolated tests).
    pub home_override: Option<PathBuf>,
}

impl SpeedReader for SqliteSpeedReader {
    fn last_speed(&self, conversation_id: &str) -> Option<f64> {
        self.session_metadata(conversation_id).speed
    }

    fn session_metadata(&self, conversation_id: &str) -> SessionMetadata {
        let Some(home) = (match &self.home_override {
            Some(p) => Some(p.clone()),
            None => resolve_home_dir(),
        }) else {
            return SessionMetadata::default();
        };
        query_session_metadata(&home, conversation_id)
    }
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

/// Resolves the user's home directory from `HOME` or `USERPROFILE`.
pub fn resolve_home_dir() -> Option<PathBuf> {
    env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .or_else(|| env::var_os("USERPROFILE").filter(|v| !v.is_empty()))
        .map(PathBuf::from)
}

/// Queries `<home>/.gemini/antigravity-cli/conversations/<conversation_id>.db`
/// in read-only mode and extracts the latest generation speed.
pub fn query_last_speed(home: &Path, conversation_id: &str) -> Option<f64> {
    query_session_metadata(home, conversation_id).speed
}

/// Queries `<home>/.gemini/antigravity-cli/conversations/<conversation_id>.db`
/// in read-only mode for both generation speed and `max_context_tokens`.
pub fn query_session_metadata(home: &Path, conversation_id: &str) -> SessionMetadata {
    if !is_valid_uuid(conversation_id) {
        return SessionMetadata::default();
    }
    let db_path = home
        .join(".gemini")
        .join("antigravity-cli")
        .join("conversations")
        .join(format!("{conversation_id}.db"));
    query_session_metadata_from_db(&db_path)
}

/// Opens `db_path` in read-only mode and extracts the latest completed `gen_metadata` speed.
pub fn query_last_speed_from_db(db_path: &Path) -> Option<f64> {
    query_session_metadata_from_db(db_path).speed
}

/// Opens `db_path` in read-only mode and extracts speed, TTFT, and `max_context_tokens`.
pub fn query_session_metadata_from_db(db_path: &Path) -> SessionMetadata {
    let mut meta = SessionMetadata::default();
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let Ok(conn) = Connection::open_with_flags(db_path, flags) else {
        return meta;
    };
    let Ok(mut stmt) =
        conn.prepare("SELECT data FROM gen_metadata WHERE data IS NOT NULL ORDER BY idx DESC LIMIT 5")
    else {
        return meta;
    };
    let Ok(mut rows) = stmt.query([]) else {
        return meta;
    };

    let mut turns: Vec<TurnMetrics> = Vec::with_capacity(5);
    while let Ok(Some(row)) = rows.next() {
        if let Ok(blob) = row.get::<_, Vec<u8>>(0) {
            if let Some(tm) = extract_turn_metrics(&blob) {
                turns.push(tm);
            }
            if meta.max_context_tokens.is_none() {
                meta.max_context_tokens = extract_max_context_tokens(&blob);
            }
        }
    }

    if let Some(first) = turns.first() {
        meta.last_turn_speed = Some((first.output_tokens as f64) / first.decode_secs);
        meta.ttft_secs = first.ttft_secs;
        meta.e2e_speed = first
            .ttft_secs
            .map(|t| (first.output_tokens as f64) / (t + first.decode_secs));

        let total_tokens: u64 = turns.iter().map(|t| t.output_tokens).sum();
        let total_secs: f64 = turns.iter().map(|t| t.decode_secs).sum();
        if total_secs > 0.0 {
            meta.speed = Some((total_tokens as f64) / total_secs);
        }
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
        let dir = env::temp_dir().join(format!("agy_statusline_test_{}", std::process::id()));
        let conv_dir = dir.join(".gemini/antigravity-cli/conversations");
        std::fs::create_dir_all(&conv_dir).unwrap();
        let uuid = "8d0371e1-5a05-423b-a8bc-ed75995f9a8b";
        let db_path = conv_dir.join(format!("{uuid}.db"));
        let conn = Connection::open(&db_path).unwrap();
        conn.execute(
            "CREATE TABLE gen_metadata (idx INTEGER PRIMARY KEY, data BLOB)",
            [],
        )
        .unwrap();
        let blob: &[u8] = &[0x0a, 0x08, 0x22, 0x02, 0x18, 0x64, 0x62, 0x02, 0x08, 0x02];
        conn.execute("INSERT INTO gen_metadata (idx, data) VALUES (1, ?1)", [blob])
            .unwrap();
        drop(conn);

        let reader = SqliteSpeedReader {
            home_override: Some(dir.clone()),
        };
        assert_eq!(reader.last_speed(uuid), Some(50.0));
        assert_eq!(reader.last_speed("invalid"), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn falls_back_when_newest_row_is_incomplete() {
        let dir = env::temp_dir().join(format!("agy_statusline_fb_{}", std::process::id()));
        let conv_dir = dir.join(".gemini/antigravity-cli/conversations");
        std::fs::create_dir_all(&conv_dir).unwrap();
        let uuid = "8d0371e1-5a05-423b-a8bc-ed75995f9a8b";
        let db_path = conv_dir.join(format!("{uuid}.db"));
        let conn = Connection::open(&db_path).unwrap();
        conn.execute(
            "CREATE TABLE gen_metadata (idx INTEGER PRIMARY KEY, data BLOB)",
            [],
        )
        .unwrap();
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
            home_override: Some(dir.clone()),
        };
        assert_eq!(reader.last_speed(uuid), Some(50.0));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extracts_both_speed_and_max_context_tokens_from_db() {
        let dir = env::temp_dir().join(format!("agy_statusline_meta_{}", std::process::id()));
        let conv_dir = dir.join(".gemini/antigravity-cli/conversations");
        std::fs::create_dir_all(&conv_dir).unwrap();
        let uuid = "8d0371e1-5a05-423b-a8bc-ed75995f9a8b";
        let db_path = conv_dir.join(format!("{uuid}.db"));
        let conn = Connection::open(&db_path).unwrap();
        conn.execute(
            "CREATE TABLE gen_metadata (idx INTEGER PRIMARY KEY, data BLOB)",
            [],
        )
        .unwrap();
        let speed_blob: &[u8] = &[0x0a, 0x08, 0x22, 0x02, 0x18, 0x64, 0x62, 0x02, 0x08, 0x02];
        let ctx_blob: &[u8] = &[0x0a, 0x08, 0x4a, 0x06, 0x52, 0x04, 0x20, 0x80, 0xa0, 0x1f];
        conn.execute("INSERT INTO gen_metadata (idx, data) VALUES (1, ?1)", [speed_blob])
            .unwrap();
        conn.execute("INSERT INTO gen_metadata (idx, data) VALUES (2, ?1)", [ctx_blob])
            .unwrap();
        drop(conn);

        let reader = SqliteSpeedReader {
            home_override: Some(dir.clone()),
        };
        let meta = reader.session_metadata(uuid);
        assert_eq!(meta.speed, Some(50.0));
        assert_eq!(meta.last_turn_speed, Some(50.0));
        assert_eq!(meta.max_context_tokens, Some(512_000));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
