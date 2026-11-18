//! Tier 2 binary CLI integration tests for quota token calibration.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

fn isolated_temp_dir(tag: &str) -> PathBuf {
    let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agy_statusline_cal_cli_{tag}_{}_{seq}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn run_binary_with_stdin(base: &Path, payload: &str) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_agy-statusline"))
        .env("HOME", base)
        .env("USERPROFILE", base)
        .env("XDG_CONFIG_HOME", base.join("config"))
        .env("XDG_CACHE_HOME", base.join("cache"))
        .env_remove("ANTIGRAVITY_CLI_HOME")
        .env_remove("AGY_HOME")
        .env_remove("AGY_STATUSLINE_FULL")
        .env("NO_COLOR", "1")
        .args(["--full"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();

    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn cli_calibrates_quota_tokens_across_ticks_and_persists_in_xdg_cache() {
    let base = isolated_temp_dir("ticks");

    let tick1 = r#"{
        "agent_state": "idle",
        "conversation_id": "conv-cli-1",
        "model": {"id": "gemini-4-hydrogen"},
        "context_window": {"total_input_tokens": 10000, "total_output_tokens": 0},
        "quota": {
            "gemini-5h": {"remaining_fraction": 0.90, "reset_in_seconds": 14400}
        }
    }"#;
    let out1 = run_binary_with_stdin(&base, tick1);
    assert!(out1.contains("5h 90%"));
    assert!(!out1.contains("(~"));
    assert!(base.join("cache/agy-statusline/quota_cal.json").is_file());

    // +200K tokens and -10% quota -> calibrates 2.0M pool -> remaining 80% = 1.6M.
    let tick2 = r#"{
        "agent_state": "idle",
        "conversation_id": "conv-cli-1",
        "model": {"id": "gemini-4-hydrogen"},
        "context_window": {"total_input_tokens": 200000, "total_output_tokens": 10000},
        "quota": {
            "gemini-5h": {"remaining_fraction": 0.80, "reset_in_seconds": 14300}
        }
    }"#;
    let out2 = run_binary_with_stdin(&base, tick2);
    assert!(out2.contains("5h 80% (~1.6M) ↻3h58m"));

    // Conversation change re-anchors while preserving 2.0M calibration -> 75% = 1.5M.
    let tick3 = r#"{
        "agent_state": "idle",
        "conversation_id": "conv-cli-2",
        "model": {"id": "gemini-4-hydrogen"},
        "context_window": {"total_input_tokens": 5000, "total_output_tokens": 0},
        "quota": {
            "gemini-5h": {"remaining_fraction": 0.75, "reset_in_seconds": 14200}
        }
    }"#;
    let out3 = run_binary_with_stdin(&base, tick3);
    assert!(out3.contains("5h 75% (~1.5M)"));

    let cal_raw = fs::read_to_string(base.join("cache/agy-statusline/quota_cal.json")).unwrap();
    assert!(cal_raw.contains("\"gemini-5h\""));
    assert!(!cal_raw.contains("\"5h:5h\""));

    let _ = fs::remove_dir_all(&base);
}
