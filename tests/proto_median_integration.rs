//! Integration and Tier 2 live-binary tests for PR 4 (`feat/p1-protobuf-median-speed`).

use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

use rusqlite::Connection;

use agy_statusline::db::SessionMetadata;
use agy_statusline::git::GitInfo;
use agy_statusline::layout::{LayoutConfig, SpeedMode, ThemeStyle, WidgetId};
use agy_statusline::render::write_statusline;
use agy_statusline::statusline::{ContextWindow, JsonUsage, StatuslinePayload};

#[test]
fn write_statusline_falls_back_to_estimated_context_tokens() {
    let mut cfg = LayoutConfig::preset_minimal();
    cfg.theme = ThemeStyle::Mono;
    cfg.rows = vec![vec![WidgetId::CtxBar, WidgetId::CtxTokens]];

    let payload_no_usage = StatuslinePayload {
        context_window: Some(ContextWindow {
            context_window_size: Some(200_000),
            used_percentage: None,
            current_usage: None,
            ..Default::default()
        }),
        ..Default::default()
    };
    let meta_est = SessionMetadata {
        estimated_context_tokens: Some(128_000),
        max_context_tokens: Some(512_000),
        ..Default::default()
    };

    let mut out_est = Vec::new();
    write_statusline(
        &payload_no_usage,
        &meta_est,
        &GitInfo::default(),
        &cfg,
        &mut out_est,
    )
    .unwrap();
    let rendered_est = String::from_utf8(out_est).unwrap();
    assert!(rendered_est.contains("25.0%"));
    assert!(rendered_est.contains("(512K)"));
    assert!(rendered_est.contains("128K/512K"));

    // Actual current_usage takes precedence over estimated_context_tokens.
    let payload_with_usage = StatuslinePayload {
        context_window: Some(ContextWindow {
            current_usage: Some(JsonUsage {
                input_tokens: Some(56_000),
                output_tokens: None,
                cache_read_input_tokens: Some(200_000),
                cache_creation_input_tokens: Some(0),
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut out_actual = Vec::new();
    write_statusline(
        &payload_with_usage,
        &meta_est,
        &GitInfo::default(),
        &cfg,
        &mut out_actual,
    )
    .unwrap();
    let rendered_actual = String::from_utf8(out_actual).unwrap();
    assert!(rendered_actual.contains("50.0%"));
    assert!(rendered_actual.contains("256K/512K"));

    // Zero estimated_context_tokens is ignored.
    let meta_zero = SessionMetadata {
        estimated_context_tokens: Some(0),
        max_context_tokens: Some(512_000),
        ..Default::default()
    };
    let mut out_zero = Vec::new();
    write_statusline(
        &payload_no_usage,
        &meta_zero,
        &GitInfo::default(),
        &cfg,
        &mut out_zero,
    )
    .unwrap();
    let rendered_zero = String::from_utf8(out_zero).unwrap();
    assert!(rendered_zero.contains("0.0%"));
}

#[test]
fn write_statusline_renders_median_speed_and_falls_back_to_weighted() {
    let mut cfg = LayoutConfig::preset_minimal();
    cfg.theme = ThemeStyle::Mono;
    cfg.speed_mode = SpeedMode::Median;
    cfg.rows = vec![vec![WidgetId::Speed]];

    let payload = StatuslinePayload::default();
    let meta_median = SessionMetadata {
        speed: Some(50.0),
        median_speed: Some(73.0),
        ..Default::default()
    };
    let mut out = Vec::new();
    write_statusline(&payload, &meta_median, &GitInfo::default(), &cfg, &mut out).unwrap();
    assert_eq!(String::from_utf8(out).unwrap(), "⚡ 73 tok/s\n");

    let meta_fallback = SessionMetadata {
        speed: Some(50.0),
        median_speed: None,
        ..Default::default()
    };
    let mut out_fb = Vec::new();
    write_statusline(&payload, &meta_fallback, &GitInfo::default(), &cfg, &mut out_fb).unwrap();
    assert_eq!(String::from_utf8(out_fb).unwrap(), "⚡ 50 tok/s\n");
}

fn speed_1s_blob(output_tokens: u8) -> [u8; 10] {
    [0x0a, 0x08, 0x22, 0x02, 0x18, output_tokens, 0x62, 0x02, 0x08, 0x01]
}

#[test]
fn tier2_binary_reads_temp_sqlite_db_median_and_estimated_context() {
    let dir = std::env::temp_dir().join(format!("agy_proto_tier2_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let xdg_cfg = dir.join(".config");
    let cfg_dir = xdg_cfg.join("agy-statusline");
    let conv_dir = dir.join(".gemini/antigravity-cli/conversations");
    fs::create_dir_all(&cfg_dir).unwrap();
    fs::create_dir_all(&conv_dir).unwrap();

    let cfg_json = r#"{
        "rows": [["ctx_bar", "ctx_tokens", "speed"], [], []],
        "theme": "mono",
        "speed_mode": "median",
        "responsive": false
    }"#;
    fs::write(cfg_dir.join("config.json"), cfg_json).unwrap();

    let uuid = "01234567-89ab-cdef-0123-456789abcdef";
    let db_path = conv_dir.join(format!("{uuid}.db"));
    let conn = Connection::open(&db_path).unwrap();
    conn.execute(
        "CREATE TABLE gen_metadata (idx INTEGER PRIMARY KEY, data BLOB)",
        [],
    )
    .unwrap();
    // 6 completed turns: sorted [20, 40, 50, 60, 80, 100] -> median = 55.0; newest 5 weighted = 66.0.
    conn.execute("INSERT INTO gen_metadata (idx, data) VALUES (1, ?1)", [speed_1s_blob(20)]).unwrap();
    conn.execute("INSERT INTO gen_metadata (idx, data) VALUES (2, ?1)", [speed_1s_blob(40)]).unwrap();
    conn.execute("INSERT INTO gen_metadata (idx, data) VALUES (3, ?1)", [speed_1s_blob(50)]).unwrap();
    conn.execute("INSERT INTO gen_metadata (idx, data) VALUES (4, ?1)", [speed_1s_blob(60)]).unwrap();
    conn.execute("INSERT INTO gen_metadata (idx, data) VALUES (5, ?1)", [speed_1s_blob(80)]).unwrap();
    conn.execute("INSERT INTO gen_metadata (idx, data) VALUES (6, ?1)", [speed_1s_blob(100)]).unwrap();
    // Newest turn-start row: estimated_tokens_used=128_000, max_context_tokens=512_000.
    let ctx_start: &[u8] = &[
        0x0a, 0x0c, 0x4a, 0x0a, 0x52, 0x08, 0x08, 0x80, 0xe8, 0x07, 0x20, 0x80, 0xa0, 0x1f,
    ];
    conn.execute("INSERT INTO gen_metadata (idx, data) VALUES (7, ?1)", [ctx_start])
        .unwrap();
    drop(conn);

    let stdin_json = format!(
        r#"{{"conversation_id":"{uuid}","context_window":{{}},"terminal_width":120}}"#
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_agy-statusline"))
        .env("HOME", &dir)
        .env("USERPROFILE", &dir)
        .env("XDG_CONFIG_HOME", &xdg_cfg)
        .env("XDG_CACHE_HOME", dir.join("cache"))
        .env_remove("ANTIGRAVITY_CLI_HOME")
        .env_remove("AGY_HOME")
        .env_remove("AGY_STATUSLINE_FULL")
        .env("NO_COLOR", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(stdin_json.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("25.0% (512K)"), "stdout was: {stdout}");
    assert!(stdout.contains("128K/512K"), "stdout was: {stdout}");
    assert!(stdout.contains("55 tok/s"), "stdout was: {stdout}");

    let _ = fs::remove_dir_all(&dir);
}
