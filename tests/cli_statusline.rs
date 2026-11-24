#![forbid(unsafe_code)]
//! Tier 2 CLI binary boundary tests for statusline rendering across SQLite,
//! XDG config/pricing cache, wall-clock quota reset, and live Git CLI boundaries.

mod common;

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use rusqlite::Connection;

use agy_statusline::layout::{
    save_layout_config, GlyphMode, LayoutConfig, ThemeStyle, WidgetId,
};

use common::{
    build_gen_blob, isolated_agy_cmd, run_statusline_stdin, unix_now_secs, TestGenSpec,
};

fn seed_alpha_rate(home: &Path) {
    let cache_path = home.join("cache/agy-statusline/models_dev_pricing.json");
    fs::create_dir_all(cache_path.parent().unwrap()).unwrap();
    let cache = serde_json::json!({
        "fetched_at": unix_now_secs(),
        "models": {"model-alpha": {"input_per_mtok": 1.0}}
    });
    fs::write(cache_path, cache.to_string()).unwrap();
}

fn seed_conv_turn(cli_root: &Path, uuid: &str, idx: i64, spec: TestGenSpec<'_>) {
    let db_path = cli_root.join("conversations").join(format!("{uuid}.db"));
    fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    let conn = Connection::open(&db_path).unwrap();
    conn.execute(
        "CREATE TABLE IF NOT EXISTS gen_metadata (idx INTEGER PRIMARY KEY, data BLOB)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO gen_metadata (idx, data) VALUES (?1, ?2)",
        rusqlite::params![idx, build_gen_blob(spec)],
    )
    .unwrap();
}

fn git_cmd(home: &Path, empty_gitconfig: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", empty_gitconfig);
    cmd
}

// ---------------------------------------------------------------------------
// SQLite gen_metadata boundary: median speed, estimated context, thinking tokens
// ---------------------------------------------------------------------------

#[test]
fn tier2_binary_reads_temp_sqlite_db_median_and_estimated_context() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_dir = dir.path().join("config/agy-statusline");
    let conv_dir = dir.path().join(".gemini/antigravity-cli/conversations");
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
    let conn = Connection::open(conv_dir.join(format!("{uuid}.db"))).unwrap();
    conn.execute(
        "CREATE TABLE gen_metadata (idx INTEGER PRIMARY KEY, data BLOB)",
        [],
    )
    .unwrap();
    // 6 completed turns: sorted [20, 40, 50, 60, 80, 100] -> median = 55.0; newest 5 weighted = 66.0.
    let speed_blob = |output_tok| {
        build_gen_blob(TestGenSpec {
            output_tok,
            dur_secs: 1,
            ..Default::default()
        })
    };
    conn.execute(
        "INSERT INTO gen_metadata (idx, data) VALUES (1, ?1)",
        [speed_blob(20)],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO gen_metadata (idx, data) VALUES (2, ?1)",
        [speed_blob(40)],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO gen_metadata (idx, data) VALUES (3, ?1)",
        [speed_blob(50)],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO gen_metadata (idx, data) VALUES (4, ?1)",
        [speed_blob(60)],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO gen_metadata (idx, data) VALUES (5, ?1)",
        [speed_blob(80)],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO gen_metadata (idx, data) VALUES (6, ?1)",
        [speed_blob(100)],
    )
    .unwrap();
    // Newest turn-start row: estimated_tokens_used=128_000, max_context_tokens=512_000.
    let ctx_start = build_gen_blob(TestGenSpec {
        estimated_context_tokens: 128_000,
        max_context_tokens: 512_000,
        ..Default::default()
    });
    conn.execute(
        "INSERT INTO gen_metadata (idx, data) VALUES (7, ?1)",
        [ctx_start],
    )
    .unwrap();
    drop(conn);

    let stdin_json =
        format!(r#"{{"conversation_id":"{uuid}","context_window":{{}},"terminal_width":120}}"#);
    let stdout = run_statusline_stdin(dir.path(), &[], &stdin_json);
    assert!(stdout.contains("25.0% (128K/512K)"), "stdout was: {stdout}");
    assert!(stdout.contains("· 128K/512K"), "stdout was: {stdout}");
    assert!(stdout.contains("55 tok/s"), "stdout was: {stdout}");
}

#[test]
fn tier2_cli_reads_real_sqlite_gen_metadata_thinking_tokens() {
    let dir = tempfile::tempdir().unwrap();
    let cli = dir.path().join(".gemini/antigravity-cli");

    let mut cfg = LayoutConfig::preset_minimal();
    cfg.rows = vec![vec![WidgetId::State, WidgetId::Thinking]];
    cfg.theme = ThemeStyle::Default;
    cfg.glyph_mode = GlyphMode::Unicode;
    cfg.smart_hide = true;
    cfg.responsive = false;
    save_layout_config(&dir.path().join("config/agy-statusline/config.json"), &cfg).unwrap();

    let uuid = "11223344-5566-7788-99aa-bbccddeeff00";
    seed_conv_turn(
        &cli,
        uuid,
        1,
        TestGenSpec {
            output_tok: 4_300,
            thinking_tok: 4_200,
            dur_secs: 2,
            ..Default::default()
        },
    );

    let stdin_json =
        format!(r#"{{"conversation_id":"{uuid}","agent_state":"idle","terminal_width":120}}"#);
    let stdout = run_statusline_stdin(dir.path(), &[], &stdin_json);
    assert!(stdout.contains("◆ think 4.2K"), "stdout was: {stdout}");
}

// ---------------------------------------------------------------------------
// Quota wall-clock reset, XDG config loading, and end-to-end AGY 1.5.10 payload
// ---------------------------------------------------------------------------

#[test]
fn cli_renders_sub_one_percent_quota_and_expired_reset_time_with_preset_full() {
    let dir = tempfile::tempdir().unwrap();
    let payload = serde_json::json!({
        "agent_state": "idle",
        "model": {
            "id": "gpt-oss-120b",
            "display_name": "GPT-OSS 120B"
        },
        "quota": {
            "gemini-5h": {
                "remaining_fraction": 0.90,
                "reset_in_seconds": 3600
            },
            "3p-5h": {
                "remaining_fraction": 0.004,
                "reset_time": "2020-01-01T00:00:00Z"
            }
        }
    })
    .to_string();

    let stdout = run_statusline_stdin(dir.path(), &["--preset", "full"], &payload);
    let mut lines = stdout.lines();
    let row1 = lines.next().expect("preset full should render row 1");
    let row2 = lines.next().expect("preset full should render row 2");
    assert_eq!(lines.next(), None, "expected 2 lines, got: {stdout}");
    assert!(row1.contains("READY"), "unexpected row1: {row1}");
    assert!(row1.contains("GPT-OSS 120B"), "unexpected row1: {row1}");
    assert!(row2.contains("3p 5h 0.4%"), "unexpected row2: {row2}");
    assert!(!row2.contains("3p 5h 0%"), "unexpected row2: {row2}");
    assert!(
        !stdout.contains('↻'),
        "unexpected timer in stdout: {stdout}"
    );
}

#[test]
fn cli_renders_sub_tenth_percent_and_carbon_weekly_with_isolated_config() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = LayoutConfig::preset_minimal();
    cfg.theme = ThemeStyle::Mono;
    cfg.glyph_mode = GlyphMode::Ascii;
    cfg.no_color = true;
    cfg.rows = vec![vec![WidgetId::Quota5h, WidgetId::QuotaWeekly]];
    save_layout_config(&dir.path().join("config/agy-statusline/config.json"), &cfg).unwrap();

    let payload = serde_json::json!({
        "agent_state": "working",
        "model": {
            "id": "m670",
            "display_name": "Gemini 4 Carbon"
        },
        "quota": {
            "gemini-5h": {
                "remaining_fraction": 0.0005,
                "reset_in_seconds": 1800
            },
            "gemini-carbon-wtu-weekly": {
                "remaining_fraction": 0.32,
                "reset_time": "2099-01-01T00:00:00Z"
            }
        }
    })
    .to_string();

    let stdout = run_statusline_stdin(dir.path(), &[], &payload);
    assert!(
        stdout.contains("5h <0.1% r:30m"),
        "unexpected stdout: {stdout}"
    );
    assert!(
        stdout.contains("carbon 32% r:"),
        "unexpected stdout: {stdout}"
    );
}

#[test]
fn cli_handles_agy_1510_negative_reset_disabled_credits_completed_subagents_and_novel_models() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = LayoutConfig::preset_minimal();
    cfg.theme = ThemeStyle::Mono;
    cfg.glyph_mode = GlyphMode::Ascii;
    cfg.no_color = true;
    cfg.rows = vec![vec![
        WidgetId::State,
        WidgetId::Model,
        WidgetId::Subagents,
        WidgetId::Quota5h,
        WidgetId::QuotaWeekly,
    ]];
    save_layout_config(&dir.path().join("config/agy-statusline/config.json"), &cfg).unwrap();

    let payload = serde_json::json!({
        "agent_state": "completed",
        "model": {
            "id": "deepseek-r2",
            "display_name": "DeepSeek R2 (Turbo)",
            "effort": "turbo"
        },
        "subagents": [
            {"name": "worker-a", "status": "running"},
            {"name": "worker-b", "status": "completed"}
        ],
        "quota": {
            "gemini-5h": {
                "remaining_fraction": 0.90,
                "reset_in_seconds": 3600
            },
            "3p-disabled-5h": {
                "remaining_fraction": 0.01,
                "disabled": true
            },
            "3p-5h": {
                "remaining_amount": 450,
                "reset_in_seconds": -12
            },
            "3p-weekly": {
                "remaining_fraction": 0.72,
                "reset_in_seconds": 172800
            }
        }
    })
    .to_string();

    let stdout = run_statusline_stdin(dir.path(), &[], &payload);
    assert_eq!(
        stdout,
        "* DONE * DeepSeek R2 (Turbo) * sub 1 (worker-a) * 3p 5h 450 * 3p 7d 72% r:2d\n"
    );
}

// ---------------------------------------------------------------------------
// Cost estimation boundary: pricing cache, SQLite turns, descendant subagents
// ---------------------------------------------------------------------------

#[test]
fn cost_widget_labels_partial_and_unpriced_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let cli = home.join(".gemini/antigravity-cli");
    let uuid = "8d0371e1-5a05-423b-a8bc-ed75995f9a8b";

    seed_alpha_rate(home);
    seed_conv_turn(
        &cli,
        uuid,
        1,
        TestGenSpec {
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
            ..Default::default()
        },
    );
    seed_conv_turn(
        &cli,
        uuid,
        2,
        TestGenSpec {
            step_idx: 2,
            model_enum: 1437,
            resp_model: "model-beta",
            disp_name: "Model Beta (High)",
            input_tok: 5_000,
            output_tok: 100,
            thinking_tok: 40,
            dur_secs: 1,
            ttft_ms: 800,
            ..Default::default()
        },
    );

    let mut payload = serde_json::json!({
        "model": {"id": "model-alpha"},
        "context_window": {"current_usage": {"input_tokens": 1_000_000}}
    });
    let no_conv_out = run_statusline_stdin(home, &["--full"], &payload.to_string());
    assert!(
        !no_conv_out.contains('$'),
        "unexpected cost without conversation_id: {no_conv_out}"
    );

    payload["conversation_id"] = uuid.into();
    let partial_out = run_statusline_stdin(home, &["--full"], &payload.to_string());
    assert!(
        partial_out.contains("~$0.01 (1 unpriced)"),
        "stdout was: {partial_out}"
    );

    let conn = Connection::open(cli.join(format!("conversations/{uuid}.db"))).unwrap();
    conn.execute("DELETE FROM gen_metadata WHERE idx = 1", [])
        .unwrap();
    drop(conn);

    let unpriced_out = run_statusline_stdin(home, &["--full"], &payload.to_string());
    assert!(
        !unpriced_out.contains('$'),
        "unexpected cost in stdout: {unpriced_out}"
    );
}

#[test]
fn cost_widget_attributes_descendant_subagent_dbs_from_conversation_summaries() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let cli = home.join(".gemini/antigravity-cli");
    let parent = "11111111-1111-4111-8111-111111111111";
    let child = "22222222-2222-4222-8222-222222222222";
    let grandchild = "33333333-3333-4333-8333-333333333333";

    seed_alpha_rate(home);
    seed_conv_turn(
        &cli,
        parent,
        1,
        TestGenSpec {
            step_idx: 1,
            model_enum: 1526,
            resp_model: "model-alpha",
            disp_name: "Model Alpha",
            input_tok: 100_000,
            dur_secs: 1,
            ttft_ms: 200,
            ..Default::default()
        },
    );
    seed_conv_turn(
        &cli,
        child,
        1,
        TestGenSpec {
            step_idx: 1,
            model_enum: 1526,
            resp_model: "model-alpha",
            disp_name: "Model Alpha",
            input_tok: 200_000,
            dur_secs: 1,
            ttft_ms: 200,
            ..Default::default()
        },
    );
    seed_conv_turn(
        &cli,
        grandchild,
        1,
        TestGenSpec {
            step_idx: 1,
            model_enum: 1526,
            resp_model: "model-alpha",
            disp_name: "Model Alpha",
            input_tok: 150_000,
            dur_secs: 1,
            ttft_ms: 200,
            ..Default::default()
        },
    );

    let payload = serde_json::json!({
        "conversation_id": parent,
        "terminal_width": 240
    })
    .to_string();

    // Without conversation_summaries.db, only parent spend ($0.10) is shown.
    let text_parent_only = run_statusline_stdin(home, &["--full"], &payload);
    assert!(
        text_parent_only.contains("~$0.10"),
        "expected ~$0.10 in: {text_parent_only}"
    );
    assert!(
        !text_parent_only.contains("(sub "),
        "unexpected subagent suffix in: {text_parent_only}"
    );

    // Link parent -> child -> grandchild in conversation_summaries.db.
    let summaries = Connection::open(cli.join("conversation_summaries.db")).unwrap();
    summaries
        .execute(
            "CREATE TABLE conversation_summaries (
                conversation_id TEXT PRIMARY KEY,
                parent_conversation_id TEXT,
                nesting_depth INTEGER
            )",
            [],
        )
        .unwrap();
    summaries
        .execute(
            "INSERT INTO conversation_summaries (conversation_id, parent_conversation_id, nesting_depth) VALUES (?1, ?2, 1)",
            [child, parent],
        )
        .unwrap();
    summaries
        .execute(
            "INSERT INTO conversation_summaries (conversation_id, parent_conversation_id, nesting_depth) VALUES (?1, ?2, 2)",
            [grandchild, child],
        )
        .unwrap();
    drop(summaries);

    let text_with_subs = run_statusline_stdin(home, &["--full"], &payload);
    assert!(
        text_with_subs.contains("~$0.45 (sub $0.35)"),
        "expected ~$0.45 (sub $0.35) in: {text_with_subs}"
    );

    // Adding an unpriced turn in the grandchild renders combined sub + unpriced suffix.
    seed_conv_turn(
        &cli,
        grandchild,
        2,
        TestGenSpec {
            step_idx: 2,
            model_enum: 1437,
            resp_model: "model-beta",
            disp_name: "Model Beta",
            input_tok: 50_000,
            dur_secs: 1,
            ttft_ms: 200,
            ..Default::default()
        },
    );
    let text_partial = run_statusline_stdin(home, &["--full"], &payload);
    assert!(
        text_partial.contains("~$0.45 (sub $0.35, 1 unpriced)"),
        "expected ~$0.45 (sub $0.35, 1 unpriced) in: {text_partial}"
    );
}

// ---------------------------------------------------------------------------
// Live Git CLI boundary
// ---------------------------------------------------------------------------

#[test]
fn live_git_cli_probe_extracts_branch_and_diff_stats() {
    let dir = tempfile::tempdir().unwrap();
    let empty_gitconfig = dir.path().join("empty.gitconfig");
    fs::write(&empty_gitconfig, "").unwrap();

    let repo = dir.path().join("repo");
    fs::create_dir_all(&repo).unwrap();

    let init_ok = git_cmd(dir.path(), &empty_gitconfig)
        .args(["init"])
        .arg(&repo)
        .output()
        .unwrap();
    assert!(init_ok.status.success(), "{init_ok:?}");

    let branch_ok = git_cmd(dir.path(), &empty_gitconfig)
        .args(["-C"])
        .arg(&repo)
        .args(["checkout", "-b", "feat-live"])
        .output()
        .unwrap();
    assert!(branch_ok.status.success(), "{branch_ok:?}");

    fs::write(repo.join("notes.txt"), "line1\nline2\n").unwrap();
    let add_ok = git_cmd(dir.path(), &empty_gitconfig)
        .args(["-C"])
        .arg(&repo)
        .args(["add", "notes.txt"])
        .output()
        .unwrap();
    assert!(add_ok.status.success(), "{add_ok:?}");

    let commit_ok = git_cmd(dir.path(), &empty_gitconfig)
        .args(["-C"])
        .arg(&repo)
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-m",
            "init",
        ])
        .output()
        .unwrap();
    assert!(commit_ok.status.success(), "{commit_ok:?}");

    fs::write(repo.join("notes.txt"), "line1\nchanged\nadded_line\n").unwrap();

    let payload = serde_json::json!({
        "agent_state": "idle",
        "cwd": repo,
        "vcs": { "type": "git" }
    })
    .to_string();

    let mut child = isolated_agy_cmd(dir.path())
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", &empty_gitconfig)
        .env("NO_COLOR", "1")
        .args(["--preset", "minimal"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let rendered = String::from_utf8(output.stdout).unwrap();
    assert!(
        rendered.contains("feat-live* +2 -1"),
        "rendered stdout was: {rendered}"
    );
}
