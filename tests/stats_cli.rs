//! Tier 2 CLI binary tests for `agy-statusline stats`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use agy_statusline::stats::{unix_secs_to_utc_date, StatsReport};
use rusqlite::Connection;

static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

fn isolated_temp_home(tag: &str) -> PathBuf {
    let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agy_statusline_stats_cli_{tag}_{}_{seq}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn seed_home_fixtures(home: &Path, today_date: &str, old_date: &str) {
    let transcript_path = home
        .join(".gemini/antigravity-cli/brain/8d0371e1-5a05-423b-a8bc-ed75995f9a8b/.system_generated/logs/transcript.jsonl");
    fs::create_dir_all(transcript_path.parent().unwrap()).unwrap();

    let today_line = format!(
        r#"{{"created_at":"{today_date}T00:01:00Z","step_index":1,"chatModel":{{"model":"gemini-3.8-flash","streamingDuration":"2.0s","timeToFirstToken":"0.4s","usage":{{"inputTokens":10000,"cacheReadTokens":20000,"outputTokens":200,"responseId":"cli-today-1"}}}}}}"#
    );
    let old_line = format!(
        r#"{{"created_at":"{old_date}T00:01:00Z","step_index":1,"chatModel":{{"model":"gemini-4-carbon","streamingDuration":"1.0s","timeToFirstToken":"0.8s","usage":{{"inputTokens":5000,"cacheReadTokens":0,"outputTokens":100,"responseId":"cli-old-1"}}}}}}"#
    );
    fs::write(&transcript_path, format!("{today_line}\n{old_line}\n")).unwrap();

    let db_path =
        home.join(".gemini/antigravity-cli/conversations/8d0371e1-5a05-423b-a8bc-ed75995f9a8b.db");
    fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    let conn = Connection::open(&db_path).unwrap();
    conn.execute(
        "CREATE TABLE gen_metadata (idx INTEGER PRIMARY KEY, data BLOB)",
        [],
    )
    .unwrap();
    let blob: &[u8] = &[
        0x0a, 0x0c, 0x22, 0x02, 0x18, 0x64, 0x5a, 0x02, 0x08, 0x01, 0x62, 0x02, 0x08, 0x02,
    ];
    conn.execute("INSERT INTO gen_metadata (idx, data) VALUES (1, ?1)", [blob])
        .unwrap();
}

#[test]
fn cli_stats_renders_text_no_color_and_json_in_isolated_home() {
    let home = isolated_temp_home("full");
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let today_date = unix_secs_to_utc_date(now_secs);
    let old_date = unix_secs_to_utc_date(now_secs.saturating_sub(15 * 86_400));
    seed_home_fixtures(&home, &today_date, &old_date);

    let text_out = Command::new(env!("CARGO_BIN_EXE_agy-statusline"))
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("NO_COLOR", "1")
        .args(["stats", "--ascii"])
        .output()
        .unwrap();
    assert!(text_out.status.success());
    let text = String::from_utf8(text_out.stdout).unwrap();
    assert!(!text.contains('\x1b'));
    assert!(text.contains("agy-statusline stats"));
    assert!(text.contains("gemini-3.8-flash"));
    assert!(text.contains("gemini-4-carbon"));
    assert!(text.contains("SQLite Turns: 1 turns across 1 DBs"));
    assert!(text.contains("Activity (28d): ["));

    let json_out = Command::new(env!("CARGO_BIN_EXE_agy-statusline"))
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .args(["stats", "--days", "7", "--json"])
        .output()
        .unwrap();
    assert!(json_out.status.success());
    let report: StatsReport = serde_json::from_slice(&json_out.stdout).unwrap();
    assert_eq!(report.filter_days, Some(7));
    assert_eq!(report.totals.requests, 1);
    assert_eq!(report.windows.all_time.requests, 2);
    assert_eq!(report.models.len(), 1);
    assert_eq!(report.models[0].model, "gemini-3.8-flash");
    assert_eq!(report.sqlite.completed_turns, 1);

    let _ = fs::remove_dir_all(&home);
}

#[test]
fn cli_stats_handles_empty_home_and_help() {
    let home = isolated_temp_home("empty");
    let empty_out = Command::new(env!("CARGO_BIN_EXE_agy-statusline"))
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("NO_COLOR", "1")
        .args(["stats", "--json"])
        .output()
        .unwrap();
    assert!(empty_out.status.success());
    let report: StatsReport = serde_json::from_slice(&empty_out.stdout).unwrap();
    assert_eq!(report.totals.requests, 0);
    assert_eq!(report.sqlite.db_files, 0);

    let help_out = Command::new(env!("CARGO_BIN_EXE_agy-statusline"))
        .args(["stats", "--help"])
        .output()
        .unwrap();
    assert!(help_out.status.success());
    let help_text = String::from_utf8(help_out.stdout).unwrap();
    assert!(help_text.contains("Usage: agy-statusline stats"));

    let _ = fs::remove_dir_all(&home);
}
