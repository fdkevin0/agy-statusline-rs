#![forbid(unsafe_code)]
//! Tier 2 CLI binary tests for `agy-statusline stats`.

mod common;

use std::fs;
use std::path::Path;

use agy_statusline::stats::{unix_secs_to_utc_date, ConversationStatsReport, StatsReport};
use rusqlite::Connection;

use common::{build_gen_blob, build_step_meta_blob, isolated_agy_cmd, unix_now_secs, TestGenSpec};

fn seed_sqlite_fixtures(cli_root: &Path, today_secs: u64, old_secs: u64) {
    let uuid = "8d0371e1-5a05-423b-a8bc-ed75995f9a8b";
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
            ..Default::default()
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
            ..Default::default()
        })],
    )
    .unwrap();
}

fn seed_alpha_rate(home: &Path) {
    let cache = serde_json::json!({
        "fetched_at": unix_now_secs(),
        "models": {"model-alpha": {"input_per_mtok": 1.0}}
    });
    let pricing_cache = home.join("cache/agy-statusline/models_dev_pricing.json");
    fs::create_dir_all(pricing_cache.parent().unwrap()).unwrap();
    fs::write(pricing_cache, cache.to_string()).unwrap();
}

#[test]
fn cli_stats_renders_text_no_color_and_json_in_isolated_home() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let now_secs = unix_now_secs();
    let old_secs = now_secs.saturating_sub(15 * 86_400);
    seed_sqlite_fixtures(&home.join(".gemini/antigravity-cli"), now_secs, old_secs);

    let text_out = isolated_agy_cmd(home)
        .env("NO_COLOR", "1")
        .args(["stats", "--ascii"])
        .output()
        .unwrap();
    assert!(text_out.status.success());
    let text = String::from_utf8(text_out.stdout).unwrap();
    assert!(!text.contains('\x1b'));
    assert!(text.is_ascii(), "expected pure 7-bit ASCII output:\n{text}");
    assert!(text.contains("agy-statusline stats | Local"));
    assert!(text.contains("(UTC)"));
    assert!(text.contains("Model Alpha"));
    assert!(text.contains("Model Beta"));
    assert!(!text.contains("Est. Cost"));
    assert!(!text.contains("unpriced"));
    assert!(text.contains("SQLite Turns: 2 turns across 1 DBs"));
    assert!(text.contains("Activity (20w): peak"));
    assert!(text.contains("Less .. == -- ## ** More"));
    assert!(text.contains("  Mon  "));

    let json_out = isolated_agy_cmd(home)
        .args(["stats", "--days", "7", "--json"])
        .output()
        .unwrap();
    assert!(json_out.status.success());
    let report: StatsReport = serde_json::from_slice(&json_out.stdout).unwrap();
    assert_eq!(report.filter_days, Some(7));
    assert_eq!(report.totals.requests, 1);
    assert_eq!(report.totals.thinking_tokens, 120);
    assert_eq!(report.windows.all_time.requests, 2);
    assert_eq!(report.weekly.len(), 20);
    assert_eq!(report.models.len(), 1);
    assert_eq!(report.models[0].model, "Model Alpha");
    assert_eq!(report.sqlite.completed_turns, 2);
}

#[test]
fn cli_stats_prices_custom_cli_home_from_cache() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let custom_cli = home.join("custom-agy-cli");
    let now_secs = unix_now_secs();
    seed_sqlite_fixtures(&custom_cli, now_secs, now_secs.saturating_sub(15 * 86_400));

    seed_alpha_rate(home);
    let out = isolated_agy_cmd(home)
        .env("ANTIGRAVITY_CLI_HOME", &custom_cli)
        .args(["stats", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let report: StatsReport = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report.totals.priced_usd, 0.01);
    assert_eq!(report.totals.unpriced_requests, 1);
}

#[test]
fn cli_stats_handles_empty_home() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();

    let empty_out = isolated_agy_cmd(home)
        .env("NO_COLOR", "1")
        .args(["stats", "--json"])
        .output()
        .unwrap();
    assert!(empty_out.status.success());
    let report: StatsReport = serde_json::from_slice(&empty_out.stdout).unwrap();
    assert_eq!(report.totals.requests, 0);
    assert_eq!(report.sqlite.db_files, 0);
}

#[test]
fn cli_stats_plain_lists_daily_usage_switches_metrics_and_rejects_tui_without_tty() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let now_secs = unix_now_secs();
    let old_secs = now_secs.saturating_sub(3 * 86_400);
    seed_sqlite_fixtures(&home.join(".gemini/antigravity-cli"), now_secs, old_secs);
    seed_alpha_rate(home);

    let today_date = unix_secs_to_utc_date(now_secs);
    let old_date = unix_secs_to_utc_date(old_secs);

    let io_out = isolated_agy_cmd(home)
        .env("NO_COLOR", "1")
        .args(["stats", "--plain", "--ascii", "--metric", "io"])
        .output()
        .unwrap();
    assert!(io_out.status.success(), "{io_out:?}");
    let io_text = String::from_utf8(io_out.stdout).unwrap();
    assert!(io_text.contains("Metric In+Out"), "{io_text}");
    assert!(io_text.contains("Activity (20w | In+Out):"), "{io_text}");
    assert!(io_text.contains("Daily (2d | In+Out):"), "{io_text}");
    assert!(io_text.contains(&today_date), "{io_text}");
    assert!(io_text.contains(&old_date), "{io_text}");

    let cost_out = isolated_agy_cmd(home)
        .env("NO_COLOR", "1")
        .args(["stats", "--plain", "--ascii", "--metric", "cost"])
        .output()
        .unwrap();
    assert!(cost_out.status.success(), "{cost_out:?}");
    let cost_text = String::from_utf8(cost_out.stdout).unwrap();
    assert!(cost_text.contains("Daily (2d | Cost ($)):"), "{cost_text}");
    assert!(cost_text.contains("~$0.01"), "{cost_text}");
    assert!(cost_text.contains("66.7%"), "{cost_text}");
    assert!(cost_text.contains("0.0%"), "{cost_text}");

    let tui_err = isolated_agy_cmd(home)
        .env("NO_COLOR", "1")
        .args(["stats", "--tui"])
        .output()
        .unwrap();
    assert!(!tui_err.status.success());
    let stderr = String::from_utf8_lossy(&tui_err.stderr);
    assert!(stderr.contains("requires a TTY"), "{stderr}");
}

#[cfg(target_os = "linux")]
#[test]
fn cli_stats_tui_persists_selected_metric_in_config_across_runs() {
    use std::io::{Read, Write};
    use std::process::Stdio;

    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().canonicalize().unwrap();
    let now_secs = unix_now_secs();
    seed_sqlite_fixtures(
        &home.join(".gemini/antigravity-cli"),
        now_secs,
        now_secs.saturating_sub(2 * 86_400),
    );
    seed_alpha_rate(&home);

    let bin = common::BIN;
    let cmd_str = format!("stty rows 36 cols 120 && exec '{bin}' stats --ascii");
    let mut tui = common::isolated_cmd("/usr/bin/script", &home)
        .env("NO_COLOR", "1")
        .env("PATH", "/usr/bin:/bin")
        .env("TERM", "xterm-256color")
        .env("SHELL", "/bin/sh")
        .args(["-qec", &cmd_str, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();

    // First byte confirms alternate-screen raw mode started; '2' selects Cost ($), 'q' exits.
    tui.stdout.as_mut().unwrap().read_exact(&mut [0]).unwrap();
    tui.stdin.as_mut().unwrap().write_all(b"2q").unwrap();
    let output = tui.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");

    let cfg_path = home.join("config/agy-statusline/config.json");
    let cfg_raw = fs::read_to_string(&cfg_path).unwrap();
    assert!(
        cfg_raw.contains("\"stats_metric\": \"cost\""),
        "expected persisted stats_metric in config.json, got:\n{cfg_raw}"
    );

    // Subsequent `stats --plain` without `--metric` remembers the saved `cost` metric.
    let next_out = isolated_agy_cmd(&home)
        .env("NO_COLOR", "1")
        .args(["stats", "--plain", "--ascii"])
        .output()
        .unwrap();
    assert!(next_out.status.success(), "{next_out:?}");
    let next_text = String::from_utf8(next_out.stdout).unwrap();
    assert!(next_text.contains("Metric Cost ($)"), "{next_text}");
    assert!(next_text.contains("Daily (2d | Cost ($)):"), "{next_text}");
}

#[test]
fn cli_stats_conversation_positional_reports_parent_subagent_and_errors() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let cli_root = home.join(".gemini/antigravity-cli");
    let now_secs = unix_now_secs();
    let earlier_secs = now_secs.saturating_sub(120);
    let root_uuid = "8d0371e1-5a05-423b-a8bc-ed75995f9a8b";
    let sub_uuid = "9a1482f2-6b16-434c-99cd-fe86aa60ab9c";

    seed_sqlite_fixtures(&cli_root, now_secs, earlier_secs);
    seed_alpha_rate(home);

    // Seed a child subagent conversation and link it via conversation_summaries.db.
    let sub_db = cli_root
        .join("conversations")
        .join(format!("{sub_uuid}.db"));
    let sub_conn = Connection::open(&sub_db).unwrap();
    sub_conn
        .execute(
            "CREATE TABLE steps (idx INTEGER PRIMARY KEY, metadata BLOB)",
            [],
        )
        .unwrap();
    sub_conn
        .execute(
            "CREATE TABLE gen_metadata (idx INTEGER PRIMARY KEY, data BLOB)",
            [],
        )
        .unwrap();
    sub_conn
        .execute(
            "INSERT INTO steps (idx, metadata) VALUES (1, ?1)",
            [build_step_meta_blob(now_secs.saturating_sub(60))],
        )
        .unwrap();
    sub_conn
        .execute(
            "INSERT INTO gen_metadata (idx, data) VALUES (1, ?1)",
            [build_gen_blob(TestGenSpec {
                step_idx: 1,
                model_enum: 1526,
                resp_model: "model-alpha",
                disp_name: "Model Alpha (Medium)",
                input_tok: 2_000,
                cache_read_tok: 8_000,
                output_tok: 150,
                thinking_tok: 50,
                dur_secs: 1,
                ttft_ms: 300,
                ..Default::default()
            })],
        )
        .unwrap();
    drop(sub_conn);

    let sum_conn = Connection::open(cli_root.join("conversation_summaries.db")).unwrap();
    sum_conn
        .execute(
            "CREATE TABLE conversation_summaries (\
                conversation_id TEXT PRIMARY KEY,\
                title TEXT NOT NULL DEFAULT '',\
                parent_conversation_id TEXT NOT NULL DEFAULT '',\
                agent_name TEXT NOT NULL DEFAULT ''\
            )",
            [],
        )
        .unwrap();
    sum_conn
        .execute(
            "INSERT INTO conversation_summaries (conversation_id, title, parent_conversation_id, agent_name) VALUES (?1, ?2, '', '')",
            [root_uuid, "Cache Rate Investigation"],
        )
        .unwrap();
    sum_conn
        .execute(
            "INSERT INTO conversation_summaries (conversation_id, title, parent_conversation_id, agent_name) VALUES (?1, '', ?2, 'research')",
            [sub_uuid, root_uuid],
        )
        .unwrap();
    drop(sum_conn);

    // Full UUID positional argument (plain ASCII output).
    let text_out = isolated_agy_cmd(home)
        .env("NO_COLOR", "1")
        .args(["stats", root_uuid, "--ascii"])
        .output()
        .unwrap();
    assert!(text_out.status.success(), "{text_out:?}");
    let text = String::from_utf8(text_out.stdout).unwrap();
    assert!(
        text.contains(&format!("Conversation {root_uuid} | cli | UTC")),
        "{text}"
    );
    assert!(text.contains("Cache Rate Investigation"), "{text}");
    assert!(text.contains("peak ctx 30K"), "{text}");
    assert!(text.contains("Sub (1)"), "{text}");
    assert!(text.contains("Subagents (1):"), "{text}");
    assert!(text.contains("9a1482f2 [research]"), "{text}");
    assert!(text.contains("Models (2):"), "{text}");
    assert!(text.contains("Turns (3):"), "{text}");
    assert!(text.contains("sub:Model Alpha"), "{text}");
    assert!(
        text.contains("SQLite: 3 speed samples across 2 DBs"),
        "{text}"
    );

    // Unique prefix positional argument (JSON output).
    let json_out = isolated_agy_cmd(home)
        .args(["stats", "8D0371E1", "--json"])
        .output()
        .unwrap();
    assert!(json_out.status.success(), "{json_out:?}");
    let report: ConversationStatsReport = serde_json::from_slice(&json_out.stdout).unwrap();
    assert_eq!(report.conversation_id, root_uuid);
    assert_eq!(report.title.as_deref(), Some("Cache Rate Investigation"));
    assert_eq!(report.duration_secs, Some(120));
    assert_eq!(report.peak_context_tokens, Some(30_000));
    assert_eq!(report.mid_session_misses, 0);
    assert_eq!(report.totals.requests, 3);
    assert_eq!(report.main.requests, 2);
    assert_eq!(report.subagents_summary.requests, 1);
    assert_eq!(report.subagents.len(), 1);
    assert_eq!(report.subagents[0].agent_name.as_deref(), Some("research"));
    assert_eq!(report.turns.len(), 3);
    assert!(!report.turns[0].is_subagent);
    assert!(report.turns[1].is_subagent);
    assert!(!report.turns[2].is_subagent);
    assert_eq!(report.totals.cache_read_tokens, 28_000);
    assert_eq!(report.totals.input_tokens, 17_000);
    assert_eq!(report.totals.priced_usd, 0.012);

    // Missing conversation returns a clean non-zero error.
    let missing_out = isolated_agy_cmd(home)
        .args(["stats", "00000000-0000-4000-8000-000000000000"])
        .output()
        .unwrap();
    assert!(!missing_out.status.success());
    let stderr = String::from_utf8_lossy(&missing_out.stderr);
    assert!(stderr.contains("not found"), "{stderr}");

    // Forcing --tui on a non-TTY fails cleanly without entering raw mode.
    let tui_err = isolated_agy_cmd(home)
        .env("NO_COLOR", "1")
        .args(["stats", "8d0371e1", "--tui"])
        .output()
        .unwrap();
    assert!(!tui_err.status.success());
    let tui_stderr = String::from_utf8_lossy(&tui_err.stderr);
    assert!(tui_stderr.contains("requires a TTY"), "{tui_stderr}");
}

#[cfg(target_os = "linux")]
#[test]
fn cli_stats_conversation_tui_renders_and_exits_cleanly_in_pty() {
    use std::io::{Read, Write};
    use std::process::Stdio;

    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().canonicalize().unwrap();
    let now_secs = unix_now_secs();
    seed_sqlite_fixtures(
        &home.join(".gemini/antigravity-cli"),
        now_secs,
        now_secs.saturating_sub(120),
    );
    seed_alpha_rate(&home);

    let bin = common::BIN;
    let cmd_str = format!("stty rows 32 cols 120 && exec '{bin}' stats 8d0371e1 --ascii");
    let mut tui = common::isolated_cmd("/usr/bin/script", &home)
        .env("NO_COLOR", "1")
        .env("PATH", "/usr/bin:/bin")
        .env("TERM", "xterm-256color")
        .env("SHELL", "/bin/sh")
        .args(["-qec", &cmd_str, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();

    // First byte confirms alternate-screen raw mode started; 's' cycles sort, 'c' opens picker,
    // Enter selects the conversation, 'q' exits cleanly.
    tui.stdout.as_mut().unwrap().read_exact(&mut [0]).unwrap();
    tui.stdin.as_mut().unwrap().write_all(b"sc\rq").unwrap();
    let output = tui.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let rendered = String::from_utf8_lossy(&output.stdout);
    assert!(rendered.contains("Conversation 8d0371e1"), "{rendered}");
    assert!(rendered.contains("1. Turns"), "{rendered}");

    // Global `stats` TUI also supports opening the conversation picker ('c'), drilling in (Enter),
    // returning back ('b'), and exiting ('q').
    let global_cmd = format!("stty rows 32 cols 120 && exec '{bin}' stats --ascii");
    let mut global_tui = common::isolated_cmd("/usr/bin/script", &home)
        .env("NO_COLOR", "1")
        .env("PATH", "/usr/bin:/bin")
        .env("TERM", "xterm-256color")
        .env("SHELL", "/bin/sh")
        .args(["-qec", &global_cmd, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    global_tui
        .stdout
        .as_mut()
        .unwrap()
        .read_exact(&mut [0])
        .unwrap();
    global_tui
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"c\rbq")
        .unwrap();
    let global_out = global_tui.wait_with_output().unwrap();
    assert!(global_out.status.success(), "{global_out:?}");
    let global_rendered = String::from_utf8_lossy(&global_out.stdout);
    assert!(
        global_rendered.contains("agy-statusline conversations | 1/1"),
        "{global_rendered}"
    );
    assert!(global_rendered.contains("1. Turns (2"), "{global_rendered}");
    assert!(global_rendered.contains("b Back"), "{global_rendered}");

    // Direct `conversations` (`conv`) entry opens full-screen picker -> Enter drills in -> b returns -> q exits.
    let conv_cmd = format!("stty rows 32 cols 120 && exec '{bin}' conv --ascii");
    let mut conv_tui = common::isolated_cmd("/usr/bin/script", &home)
        .env("NO_COLOR", "1")
        .env("PATH", "/usr/bin:/bin")
        .env("TERM", "xterm-256color")
        .env("SHELL", "/bin/sh")
        .args(["-qec", &conv_cmd, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    conv_tui
        .stdout
        .as_mut()
        .unwrap()
        .read_exact(&mut [0])
        .unwrap();
    conv_tui.stdin.as_mut().unwrap().write_all(b"\rbq").unwrap();
    let conv_out = conv_tui.wait_with_output().unwrap();
    assert!(conv_out.status.success(), "{conv_out:?}");
    let conv_rendered = String::from_utf8_lossy(&conv_out.stdout);
    assert!(
        conv_rendered.contains("agy-statusline conversations | 1/1"),
        "{conv_rendered}"
    );
    assert!(conv_rendered.contains("1. Turns (2"), "{conv_rendered}");
    assert!(conv_rendered.contains("Loaded 8d0371e1"), "{conv_rendered}");
    assert!(conv_rendered.contains("* 8d0371e1"), "{conv_rendered}");
}

#[test]
fn cli_conversations_lists_filters_json_and_rejects_non_tty_tui() {
    use agy_statusline::stats::ConversationListItem;

    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let cli_root = home.join(".gemini/antigravity-cli");
    let now_secs = unix_now_secs();
    let root_uuid = "8d0371e1-5a05-423b-a8bc-ed75995f9a8b";
    let sub_uuid = "9a1482f2-6b16-434c-99cd-fe86aa60ab9c";

    seed_sqlite_fixtures(&cli_root, now_secs, now_secs.saturating_sub(120));
    let _ = Connection::open(
        cli_root
            .join("conversations")
            .join(format!("{sub_uuid}.db")),
    )
    .unwrap();

    let sum_conn = Connection::open(cli_root.join("conversation_summaries.db")).unwrap();
    sum_conn
        .execute(
            "CREATE TABLE conversation_summaries (\
                conversation_id TEXT PRIMARY KEY,\
                title TEXT NOT NULL DEFAULT '',\
                parent_conversation_id TEXT NOT NULL DEFAULT '',\
                agent_name TEXT NOT NULL DEFAULT ''\
            )",
            [],
        )
        .unwrap();
    sum_conn
        .execute(
            "INSERT INTO conversation_summaries VALUES (?1, 'Cache Rate Audit', '', '')",
            [root_uuid],
        )
        .unwrap();
    sum_conn
        .execute(
            "INSERT INTO conversation_summaries VALUES (?1, 'Helper', ?2, 'research')",
            [sub_uuid, root_uuid],
        )
        .unwrap();
    drop(sum_conn);

    // Default static/piped listing hides subagents and prints clean ASCII table.
    let plain_out = isolated_agy_cmd(home)
        .env("NO_COLOR", "1")
        .args(["conversations", "--ascii"])
        .output()
        .unwrap();
    assert!(plain_out.status.success(), "{plain_out:?}");
    let plain_text = String::from_utf8(plain_out.stdout).unwrap();
    assert!(
        plain_text.contains("agy-statusline conversations | 1 found"),
        "{plain_text}"
    );
    assert!(plain_text.contains("8d0371e1"), "{plain_text}");
    assert!(plain_text.contains("Cache Rate Audit"), "{plain_text}");
    assert!(plain_text.contains("+1"), "{plain_text}");
    assert!(!plain_text.contains("9a1482f2"), "{plain_text}");

    // Alias `sessions --subagents --json` includes subagents and deserializes cleanly.
    let json_out = isolated_agy_cmd(home)
        .args(["sessions", "--subagents", "--json"])
        .output()
        .unwrap();
    assert!(json_out.status.success(), "{json_out:?}");
    let items: Vec<ConversationListItem> = serde_json::from_slice(&json_out.stdout).unwrap();
    assert_eq!(items.len(), 2);

    // Query filter matches specific conversation via `conv` alias.
    let filtered_out = isolated_agy_cmd(home)
        .args(["conv", "research", "--subagents", "--json"])
        .output()
        .unwrap();
    assert!(filtered_out.status.success(), "{filtered_out:?}");
    let filtered: Vec<ConversationListItem> = serde_json::from_slice(&filtered_out.stdout).unwrap();
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].conversation_id, sub_uuid);

    // Non-TTY `--tui` fails cleanly.
    let tui_err = isolated_agy_cmd(home)
        .args(["conversations", "--tui"])
        .output()
        .unwrap();
    assert!(!tui_err.status.success());
    let stderr = String::from_utf8_lossy(&tui_err.stderr);
    assert!(stderr.contains("requires a TTY"), "{stderr}");
}

#[cfg(target_os = "linux")]
#[test]
fn cli_conv_tui_persists_sort_prefs_and_restores_on_next_launch() {
    use std::io::{Read, Write};
    use std::process::Stdio;

    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().canonicalize().unwrap();
    let now_secs = unix_now_secs();
    seed_sqlite_fixtures(
        &home.join(".gemini/antigravity-cli"),
        now_secs,
        now_secs.saturating_sub(120),
    );
    seed_alpha_rate(&home);

    let bin = common::BIN;
    let conv_cmd = format!("stty rows 36 cols 120 && exec '{bin}' conv --ascii");
    let mut tui = common::isolated_cmd("/usr/bin/script", &home)
        .env("NO_COLOR", "1")
        .env("PATH", "/usr/bin:/bin")
        .env("TERM", "xterm-256color")
        .env("SHELL", "/bin/sh")
        .args(["-qec", &conv_cmd, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();

    // First byte confirms TUI started; 'S' flips conv list sort, Enter opens detail,
    // 'S' flips turn sort (# ^ -> # v), 'q' quits.
    tui.stdout.as_mut().unwrap().read_exact(&mut [0]).unwrap();
    tui.stdin.as_mut().unwrap().write_all(b"S\rSq").unwrap();
    let out1 = tui.wait_with_output().unwrap();
    assert!(out1.status.success(), "{out1:?}");

    let cfg_path = home.join("config/agy-statusline/config.json");
    let cfg_raw = fs::read_to_string(&cfg_path).unwrap();
    assert!(
        cfg_raw.contains("\"conv_list_sort\": \"modified:asc\""),
        "expected persisted conv_list_sort, got:\n{cfg_raw}"
    );
    assert!(
        cfg_raw.contains("\"conv_turn_sort\": \"turn:desc\""),
        "expected persisted conv_turn_sort, got:\n{cfg_raw}"
    );

    // Re-opening the conversation restores the remembered `# v` sort order.
    let detail_cmd = format!("stty rows 36 cols 120 && exec '{bin}' stats 8d0371e1 --ascii");
    let mut tui2 = common::isolated_cmd("/usr/bin/script", &home)
        .env("NO_COLOR", "1")
        .env("PATH", "/usr/bin:/bin")
        .env("TERM", "xterm-256color")
        .env("SHELL", "/bin/sh")
        .args(["-qec", &detail_cmd, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    tui2.stdout.as_mut().unwrap().read_exact(&mut [0]).unwrap();
    tui2.stdin.as_mut().unwrap().write_all(b"q").unwrap();
    let out2 = tui2.wait_with_output().unwrap();
    assert!(out2.status.success(), "{out2:?}");
    let stripped = strip_ansi_escapes::strip_str(String::from_utf8_lossy(&out2.stdout));
    assert!(
        stripped.contains("# v"),
        "expected restored turn:desc sort:\n{stripped}"
    );
    assert!(
        stripped.contains("* Live"),
        "expected Live indicator:\n{stripped}"
    );
}

#[test]
fn cli_stats_persists_global_scan_cache_and_surfaces_quota_and_diagnostics() {
    use std::collections::BTreeMap;

    use agy_statusline::agy::quota::{save_quota_snapshot, QuotaSnapshot, QuotaSource};
    use agy_statusline::statusline::QuotaBucket;

    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let cli_root = home.join(".gemini/antigravity-cli");
    let uuid = "77777777-7777-4777-8777-777777777777";
    let now_secs = unix_now_secs();

    let db_path = cli_root.join("conversations").join(format!("{uuid}.db"));
    fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    let conn = Connection::open(&db_path).unwrap();
    conn.execute(
        "CREATE TABLE steps (idx INTEGER PRIMARY KEY, step_type INTEGER, metadata BLOB)",
        [],
    )
    .unwrap();
    conn.execute(
        "CREATE TABLE gen_metadata (idx INTEGER PRIMARY KEY, data BLOB)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO steps (idx, step_type, metadata) VALUES (1, 1, ?1)",
        [build_step_meta_blob(now_secs)],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO steps (idx, step_type, metadata) VALUES (2, 23, NULL)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO steps (idx, step_type, metadata) VALUES (3, 17, NULL)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO gen_metadata (idx, data) VALUES (1, ?1)",
        [build_gen_blob(TestGenSpec {
            step_idx: 1,
            model_enum: 1526,
            resp_model: "model-alpha",
            disp_name: "Model Alpha",
            input_tok: 10_000,
            cache_read_tok: 30_000,
            output_tok: 400,
            dur_secs: 2,
            ttft_ms: 500,
            retry_error: Some("HTTP 429 RESOURCE_EXHAUSTED"),
            retry_delay_secs: 8,
            ..Default::default()
        })],
    )
    .unwrap();
    drop(conn);

    // Seed cached quota snapshot in isolated XDG_CACHE_HOME/agy-statusline.
    let cache_dir = home.join("cache/agy-statusline");
    fs::create_dir_all(&cache_dir).unwrap();
    let snap = QuotaSnapshot {
        updated_at: now_secs.saturating_sub(180),
        source: QuotaSource::HubRpc,
        plan_tier: Some("Ultra".to_string()),
        ultra_tier_label: Some("20x".to_string()),
        email: Some("dev@example.com".to_string()),
        ai_credits: Some(1250),
        quota: BTreeMap::from([
            (
                "gemini-5h".to_string(),
                QuotaBucket {
                    remaining_fraction: Some(0.65),
                    reset_in_seconds: Some(7_200),
                    ..Default::default()
                },
            ),
            (
                "gemini-argon-weekly".to_string(),
                QuotaBucket {
                    remaining_fraction: Some(0.55),
                    reset_in_seconds: Some(345_600),
                    ..Default::default()
                },
            ),
            (
                "3p-5h".to_string(),
                QuotaBucket {
                    remaining_fraction: Some(0.40),
                    reset_in_seconds: Some(3_600),
                    ..Default::default()
                },
            ),
        ]),
        history: BTreeMap::new(),
    };
    save_quota_snapshot(&cache_dir, &snap).unwrap();

    // Global `stats --plain --ascii`: writes persistent `stats_scan_cache.json` and renders Quota + Diagnostics.
    let plain_out = isolated_agy_cmd(home)
        .env("NO_COLOR", "1")
        .args(["stats", "--plain", "--ascii"])
        .output()
        .unwrap();
    assert!(plain_out.status.success(), "{plain_out:?}");
    let plain_text = String::from_utf8(plain_out.stdout).unwrap();
    assert!(
        plain_text.contains("Quota (Ultra 20x | AI credits 1250 | cached "),
        "{plain_text}"
    );
    assert!(plain_text.contains("Gemini 5h 65%"), "{plain_text}");
    assert!(plain_text.contains("Argon 7d 55%"), "{plain_text}");
    assert!(plain_text.contains("Claude/3P 5h 40%"), "{plain_text}");
    assert!(
        plain_text.contains("Diagnostics:  1 retry (1x429, 8.0s delay) | 1 compaction | 1 error"),
        "{plain_text}"
    );
    assert!(
        cache_dir.join("stats_scan_cache.json").is_file(),
        "expected stats_scan_cache.json to be persisted in isolated XDG_CACHE_HOME"
    );

    // Global `stats --json`: exposes structured `quota` and `diagnostics`.
    let json_out = isolated_agy_cmd(home)
        .args(["stats", "--json"])
        .output()
        .unwrap();
    assert!(json_out.status.success(), "{json_out:?}");
    let report: StatsReport = serde_json::from_slice(&json_out.stdout).unwrap();
    let quota = report
        .quota
        .expect("expected quota summary in StatsReport JSON");
    assert_eq!(quota.plan_tier.as_deref(), Some("Ultra 20x"));
    assert_eq!(quota.ai_credits, Some(1250));
    assert!(quota.from_cache);
    assert_eq!(quota.pools.len(), 3);
    assert_eq!(report.diagnostics.retry_count, 1);
    assert_eq!(report.diagnostics.retries_429, 1);
    assert_eq!(report.diagnostics.compaction_steps, 1);
    assert_eq!(report.diagnostics.error_steps, 1);

    // Single-conversation `stats <uuid>` plain + JSON surfaces conversation diagnostics.
    let conv_plain = isolated_agy_cmd(home)
        .env("NO_COLOR", "1")
        .args(["stats", uuid, "--plain", "--ascii"])
        .output()
        .unwrap();
    assert!(conv_plain.status.success(), "{conv_plain:?}");
    let conv_text = String::from_utf8(conv_plain.stdout).unwrap();
    assert!(conv_text.contains("1 compaction"), "{conv_text}");
    assert!(
        conv_text.contains("1 retry (1x429, 8.0s delay)"),
        "{conv_text}"
    );
    assert!(conv_text.contains("1 error"), "{conv_text}");

    let conv_json = isolated_agy_cmd(home)
        .args(["stats", uuid, "--json"])
        .output()
        .unwrap();
    assert!(conv_json.status.success(), "{conv_json:?}");
    let conv_report: ConversationStatsReport = serde_json::from_slice(&conv_json.stdout).unwrap();
    assert_eq!(conv_report.diagnostics.total.retries_429, 1);
    assert_eq!(conv_report.diagnostics.total.compaction_steps, 1);
    assert_eq!(conv_report.diagnostics.total.error_steps, 1);
}
