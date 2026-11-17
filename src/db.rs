//! Read-only SQLite conversation database reader and UUID validation.
//!
//! Queries `~/.gemini/antigravity-cli/conversations/<conversation_id>.db`
//! for the most recent `gen_metadata` blob and extracts generation speed.

use std::env;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

use crate::protobuf::extract_metadata_speed;

/// Trait abstracting last-turn speed lookup for testability without monkey-patching.
pub trait SpeedReader {
    /// Returns the last completed turn's generation speed in tokens/sec, if available.
    fn last_speed(&self, conversation_id: &str) -> Option<f64>;
}

/// Production [`SpeedReader`] backed by the local AGY SQLite conversation database.
#[derive(Debug, Clone, Default)]
pub struct SqliteSpeedReader {
    /// Optional override for the base home directory (used in isolated tests).
    pub home_override: Option<PathBuf>,
}

impl SpeedReader for SqliteSpeedReader {
    fn last_speed(&self, conversation_id: &str) -> Option<f64> {
        let home = match &self.home_override {
            Some(p) => p.clone(),
            None => resolve_home_dir()?,
        };
        query_last_speed(&home, conversation_id)
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
    if !is_valid_uuid(conversation_id) {
        return None;
    }
    let db_path = home
        .join(".gemini")
        .join("antigravity-cli")
        .join("conversations")
        .join(format!("{conversation_id}.db"));
    query_last_speed_from_db(&db_path)
}

/// Opens `db_path` in read-only mode and extracts the latest `gen_metadata` speed.
pub fn query_last_speed_from_db(db_path: &Path) -> Option<f64> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(db_path, flags).ok()?;
    let mut stmt = conn
        .prepare("SELECT data FROM gen_metadata WHERE data IS NOT NULL ORDER BY idx DESC LIMIT 1")
        .ok()?;
    let mut rows = stmt.query([]).ok()?;
    let row = rows.next().ok()??;
    let blob: Vec<u8> = row.get(0).ok()?;
    extract_metadata_speed(&blob)
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
}
