#![forbid(unsafe_code)]
//! Tier 2 CLI binary tests for `agy-statusline stats`.

mod common;

use std::fs;
use std::path::Path;

use agy_statusline::stats::{unix_secs_to_utc_date, StatsReport};
use rusqlite::Connection;

use common::{
    build_gen_blob, build_step_meta_blob, isolated_agy_cmd, unix_now_secs, TestGenSpec,
};

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
    assert!(text.contains("agy-statusline stats · Local"));
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
fn cli_stats_prices_custom_agy_home_from_cache_and_ignores_legacy_rates() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let custom_cli = home.join("custom-agy-cli");
    let now_secs = unix_now_secs();
    seed_sqlite_fixtures(&custom_cli, now_secs, now_secs.saturating_sub(15 * 86_400));

    let pricing_file = home.join("config/agy-statusline/pricing.json");
    fs::create_dir_all(pricing_file.parent().unwrap()).unwrap();
    fs::write(
        &pricing_file,
        r#"{"models":{"model-alpha":{"input_per_mtok":2.0,"cache_read_per_mtok":0.2,"output_per_mtok":10.0},"model-beta":{"input_per_mtok":5.0,"output_per_mtok":20.0}}}"#,
    )
    .unwrap();

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
    assert_eq!(report.sqlite.archived_db_files, 0);
    assert!(!home.join("data/agy-statusline/stats_history.db").exists());
}

#[test]
fn cli_stats_preserves_archived_history_after_pruning_and_supports_sync_subcommand() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let cli_root = home.join(".gemini/antigravity-cli");
    let now_secs = unix_now_secs();
    let old_secs = now_secs.saturating_sub(20 * 86_400);
    seed_sqlite_fixtures(&cli_root, now_secs, old_secs);
    seed_alpha_rate(home);

    let pruned_uuid = "8d0371e1-5a05-423b-a8bc-ed75995f9a8b";
    let kept_uuid = "9e1482f2-6b16-534c-b9cd-fe86006a0b9c";
    let kept_db = cli_root.join("conversations").join(format!("{kept_uuid}.db"));
    let kept_conn = Connection::open(&kept_db).unwrap();
    kept_conn
        .execute(
            "CREATE TABLE steps (idx INTEGER PRIMARY KEY, metadata BLOB)",
            [],
        )
        .unwrap();
    kept_conn
        .execute(
            "CREATE TABLE gen_metadata (idx INTEGER PRIMARY KEY, data BLOB)",
            [],
        )
        .unwrap();
    kept_conn
        .execute(
            "INSERT INTO steps (idx, metadata) VALUES (1, ?1)",
            [build_step_meta_blob(now_secs)],
        )
        .unwrap();
    kept_conn
        .execute(
            "INSERT INTO gen_metadata (idx, data) VALUES (1, ?1)",
            [build_gen_blob(TestGenSpec {
                step_idx: 1,
                model_enum: 1526,
                resp_model: "model-alpha",
                disp_name: "Model Alpha (High)",
                input_tok: 4_000,
                cache_read_tok: 0,
                output_tok: 100,
                thinking_tok: 20,
                dur_secs: 1,
                ttft_ms: 500,
                ..Default::default()
            })],
        )
        .unwrap();
    drop(kept_conn);

    // Build the command first (which seeds offline cache stamps), then remove the pre-seeded
    // sync stamp so this test proves `stats sync` creates the stamp itself.
    let mut sync_cmd = isolated_agy_cmd(home);
    let stamp_path = home.join("cache/agy-statusline/stats_archive_sync.ts");
    fs::remove_file(&stamp_path).unwrap();
    assert!(!stamp_path.exists());

    // Explicit `stats sync` archives both live DBs and writes the sync stamp.
    let sync_out = sync_cmd.args(["stats", "sync"]).output().unwrap();
    assert!(sync_out.status.success(), "{sync_out:?}");
    let sync_text = String::from_utf8(sync_out.stdout).unwrap();
    assert!(
        sync_text.contains("Synced 2 updated / 2 live DBs (2 total, 0 archived)"),
        "{sync_text}"
    );
    assert!(home.join("data/agy-statusline/stats_history.db").is_file());
    assert!(stamp_path.is_file());

    // `stats sync --if-stale` skips immediately while the stamp is fresh.
    let stale_out = isolated_agy_cmd(home)
        .args(["stats", "sync", "--if-stale"])
        .output()
        .unwrap();
    assert!(stale_out.status.success(), "{stale_out:?}");
    let stale_text = String::from_utf8(stale_out.stdout).unwrap();
    assert!(stale_text.contains("Stats archive is fresh"), "{stale_text}");

    // Simulate AGY pruning the older conversation DB and its brain directory.
    fs::remove_file(
        cli_root
            .join("conversations")
            .join(format!("{pruned_uuid}.db")),
    )
    .unwrap();
    fs::remove_dir_all(cli_root.join("brain").join(pruned_uuid)).unwrap();

    let json_out = isolated_agy_cmd(home)
        .args(["stats", "--json"])
        .output()
        .unwrap();
    assert!(json_out.status.success(), "{json_out:?}");
    let report: StatsReport = serde_json::from_slice(&json_out.stdout).unwrap();
    assert_eq!(report.sqlite.db_files, 2);
    assert_eq!(report.sqlite.archived_db_files, 1);
    assert_eq!(report.sqlite.completed_turns, 3);
    assert_eq!(report.windows.all_time.requests, 3);
    assert_eq!(report.windows.all_time.input_tokens, 19_000);
    assert_eq!(report.windows.all_time.thinking_tokens, 180);

    let plain_out = isolated_agy_cmd(home)
        .env("NO_COLOR", "1")
        .args(["stats", "--plain", "--ascii"])
        .output()
        .unwrap();
    assert!(plain_out.status.success(), "{plain_out:?}");
    let plain_text = String::from_utf8(plain_out.stdout).unwrap();
    assert!(
        plain_text.contains("SQLite Turns: 3 turns across 2 DBs (1 archived)"),
        "{plain_text}"
    );
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
