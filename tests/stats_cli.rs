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

fn push_varint(buf: &mut Vec<u8>, mut val: u64) {
    loop {
        let mut byte = (val & 0x7f) as u8;
        val >>= 7;
        if val != 0 {
            byte |= 0x80;
        }
        buf.push(byte);
        if val == 0 {
            break;
        }
    }
}

fn push_field_varint(buf: &mut Vec<u8>, id: u64, val: u64) {
    push_varint(buf, id << 3);
    push_varint(buf, val);
}

fn push_field_bytes(buf: &mut Vec<u8>, id: u64, bytes: &[u8]) {
    push_varint(buf, (id << 3) | 2);
    push_varint(buf, bytes.len() as u64);
    buf.extend_from_slice(bytes);
}

fn build_step_meta_blob(secs: u64) -> Vec<u8> {
    let mut ts = Vec::new();
    push_field_varint(&mut ts, 1, secs);
    let mut step = Vec::new();
    push_field_bytes(&mut step, 1, &ts);
    step
}

struct TestGenSpec<'a> {
    step_idx: u64,
    model_enum: u64,
    resp_model: &'a str,
    disp_name: &'a str,
    input_tok: u64,
    cache_read_tok: u64,
    output_tok: u64,
    thinking_tok: u64,
    dur_secs: u64,
    ttft_ms: u64,
}

fn build_gen_blob(spec: TestGenSpec<'_>) -> Vec<u8> {
    let mut usage = Vec::new();
    push_field_varint(&mut usage, 2, spec.input_tok);
    push_field_varint(&mut usage, 3, spec.output_tok);
    push_field_varint(&mut usage, 5, spec.cache_read_tok);
    push_field_varint(&mut usage, 9, spec.thinking_tok);

    let mut ttft = Vec::new();
    push_field_varint(&mut ttft, 1, spec.ttft_ms / 1_000);
    push_field_varint(&mut ttft, 2, (spec.ttft_ms % 1_000) * 1_000_000);

    let mut dur = Vec::new();
    push_field_varint(&mut dur, 1, spec.dur_secs);

    let mut chat = Vec::new();
    push_field_varint(&mut chat, 3, spec.model_enum);
    push_field_bytes(&mut chat, 4, &usage);
    push_field_bytes(&mut chat, 11, &ttft);
    push_field_bytes(&mut chat, 12, &dur);
    push_field_bytes(&mut chat, 19, spec.resp_model.as_bytes());
    push_field_bytes(&mut chat, 21, spec.disp_name.as_bytes());

    let mut packed = Vec::new();
    push_varint(&mut packed, spec.step_idx);

    let mut root = Vec::new();
    push_field_bytes(&mut root, 1, &chat);
    push_field_bytes(&mut root, 2, &packed);
    root
}

fn seed_sqlite_fixtures(cli_root: &Path, today_secs: u64, old_secs: u64) {
    let uuid = "8d0371e1-5a05-423b-a8bc-ed75995f9a8b";
    let transcript_path = cli_root
        .join("brain")
        .join(uuid)
        .join(".system_generated/logs/transcript.jsonl");
    fs::create_dir_all(transcript_path.parent().unwrap()).unwrap();
    let today_date = unix_secs_to_utc_date(today_secs);
    let covered_dup = format!(
        r#"{{"created_at":"{today_date}T00:01:00Z","step_index":1,"input_tokens":99999,"output_tokens":999,"cache_read_tokens":0}}"#
    );
    fs::write(&transcript_path, format!("{covered_dup}\n")).unwrap();

    let db_path = cli_root.join("conversations").join(format!("{uuid}.db"));
    fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    let conn = Connection::open(&db_path).unwrap();
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
    conn.execute(
        "INSERT INTO steps (idx, metadata) VALUES (1, ?1)",
        [build_step_meta_blob(today_secs)],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO steps (idx, metadata) VALUES (2, ?1)",
        [build_step_meta_blob(old_secs)],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO gen_metadata (idx, data) VALUES (1, ?1)",
        [build_gen_blob(TestGenSpec {
            step_idx: 1,
            model_enum: 1526,
            resp_model: "model-alpha",
            disp_name: "Model Alpha (Medium)",
            input_tok: 10_000,
            cache_read_tok: 20_000,
            output_tok: 200,
            thinking_tok: 120,
            dur_secs: 2,
            ttft_ms: 400,
        })],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO gen_metadata (idx, data) VALUES (2, ?1)",
        [build_gen_blob(TestGenSpec {
            step_idx: 2,
            model_enum: 1437,
            resp_model: "model-beta",
            disp_name: "Model Beta (High)",
            input_tok: 5_000,
            cache_read_tok: 0,
            output_tok: 100,
            thinking_tok: 40,
            dur_secs: 1,
            ttft_ms: 800,
        })],
    )
    .unwrap();
}

#[test]
fn cli_stats_renders_text_no_color_and_json_in_isolated_home() {
    let home = isolated_temp_home("full");
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let old_secs = now_secs.saturating_sub(15 * 86_400);
    seed_sqlite_fixtures(&home.join(".gemini/antigravity-cli"), now_secs, old_secs);

    let text_out = Command::new(env!("CARGO_BIN_EXE_agy-statusline"))
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("TZ", "UTC")
        .env_remove("ANTIGRAVITY_CLI_HOME")
        .env_remove("AGY_HOME")
        .env_remove("AGY_STATUSLINE_FULL")
        .env("NO_COLOR", "1")
        .args(["stats", "--ascii"])
        .output()
        .unwrap();
    assert!(text_out.status.success());
    let text = String::from_utf8(text_out.stdout).unwrap();
    assert!(!text.contains('\x1b'));
    assert!(text.contains("agy-statusline stats · Local"));
    assert!(text.contains("(UTC)"));
    assert!(text.contains("Model Alpha"));
    assert!(text.contains("Model Beta"));
    assert!(!text.contains("Est. Cost"));
    assert!(!text.contains("unpriced"));
    assert!(text.contains("SQLite Turns: 2 turns across 1 DBs"));
    assert!(text.contains("Activity (28d): ["));

    let json_out = Command::new(env!("CARGO_BIN_EXE_agy-statusline"))
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("TZ", "UTC")
        .env_remove("ANTIGRAVITY_CLI_HOME")
        .env_remove("AGY_HOME")
        .env_remove("AGY_STATUSLINE_FULL")
        .args(["stats", "--days", "7", "--json"])
        .output()
        .unwrap();
    assert!(json_out.status.success());
    let report: StatsReport = serde_json::from_slice(&json_out.stdout).unwrap();
    assert_eq!(report.filter_days, Some(7));
    assert_eq!(report.totals.requests, 1);
    assert_eq!(report.totals.thinking_tokens, 120);
    assert_eq!(report.windows.all_time.requests, 2);
    assert_eq!(report.models.len(), 1);
    assert_eq!(report.models[0].model, "Model Alpha");
    assert_eq!(report.sqlite.completed_turns, 2);

    let _ = fs::remove_dir_all(&home);
}

#[test]
fn cli_stats_supports_agy_home_override_and_optional_pricing_json() {
    let home = isolated_temp_home("override");
    let custom_cli = home.join("custom-agy-cli");
    let xdg_cfg = home.join("xdg-cfg");
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    seed_sqlite_fixtures(&custom_cli, now_secs, now_secs.saturating_sub(15 * 86_400));

    let pricing_file = xdg_cfg.join("agy-statusline/pricing.json");
    fs::create_dir_all(pricing_file.parent().unwrap()).unwrap();
    fs::write(
        &pricing_file,
        r#"{"models":{"model-alpha":{"input_per_mtok":2.0,"cache_read_per_mtok":0.2,"output_per_mtok":10.0}}}"#,
    )
    .unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_agy-statusline"))
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("XDG_CONFIG_HOME", &xdg_cfg)
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("ANTIGRAVITY_CLI_HOME", &custom_cli)
        .env_remove("AGY_HOME")
        .env_remove("AGY_STATUSLINE_FULL")
        .env("TZ", "CST-8")
        .env("NO_COLOR", "1")
        .args(["stats", "--ascii"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("Est. Cost"));
    assert!(text.contains("Model Alpha"));

    let _ = fs::remove_dir_all(&home);
}

#[test]
fn cli_stats_handles_empty_home_and_help() {
    let home = isolated_temp_home("empty");
    let empty_out = Command::new(env!("CARGO_BIN_EXE_agy-statusline"))
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("TZ", "UTC")
        .env_remove("ANTIGRAVITY_CLI_HOME")
        .env_remove("AGY_HOME")
        .env_remove("AGY_STATUSLINE_FULL")
        .env("NO_COLOR", "1")
        .args(["stats", "--json"])
        .output()
        .unwrap();
    assert!(empty_out.status.success());
    let report: StatsReport = serde_json::from_slice(&empty_out.stdout).unwrap();
    assert_eq!(report.totals.requests, 0);
    assert_eq!(report.sqlite.db_files, 0);

    let help_out = Command::new(env!("CARGO_BIN_EXE_agy-statusline"))
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env_remove("ANTIGRAVITY_CLI_HOME")
        .env_remove("AGY_HOME")
        .env_remove("AGY_STATUSLINE_FULL")
        .args(["stats", "--help"])
        .output()
        .unwrap();
    assert!(help_out.status.success());
    let help_text = String::from_utf8(help_out.stdout).unwrap();
    assert!(help_text.contains("Usage: agy-statusline stats"));

    let _ = fs::remove_dir_all(&home);
}
