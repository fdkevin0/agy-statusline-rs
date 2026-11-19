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
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let now_secs = unix_now_secs();
    let old_secs = now_secs.saturating_sub(15 * 86_400);
    seed_sqlite_fixtures(&home.join(".gemini/antigravity-cli"), now_secs, old_secs);

    let text_out = isolated_agy_cmd(home)
        .env("TZ", "UTC")
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
        .env("TZ", "UTC")
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

    let mut cmd = isolated_agy_cmd(home);
    seed_alpha_rate(home);
    let out = cmd
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
fn cli_stats_handles_empty_home_and_help() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();

    let empty_out = isolated_agy_cmd(home)
        .env("TZ", "UTC")
        .env("NO_COLOR", "1")
        .args(["stats", "--json"])
        .output()
        .unwrap();
    assert!(empty_out.status.success());
    let report: StatsReport = serde_json::from_slice(&empty_out.stdout).unwrap();
    assert_eq!(report.totals.requests, 0);
    assert_eq!(report.sqlite.db_files, 0);

    let help_out = isolated_agy_cmd(home)
        .args(["stats", "--help"])
        .output()
        .unwrap();
    assert!(help_out.status.success());
    let help_text = String::from_utf8(help_out.stdout).unwrap();
    assert!(help_text.contains("Usage: agy-statusline stats"));
}

fn seed_alpha_rate(home: &Path) {
    let cache = serde_json::json!({
        "fetched_at": unix_now_secs(),
        "models": {"model-alpha": {"input_per_mtok": 1.0}}
    });
    fs::write(home.join("cache/agy-statusline/models_dev_pricing.json"), cache.to_string()).unwrap();
}

#[test]
fn cost_widget_labels_partial_and_latest_turn_estimates() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let cli = home.join(".gemini/antigravity-cli");
    let uuid = "8d0371e1-5a05-423b-a8bc-ed75995f9a8b";
    seed_sqlite_fixtures(&cli, 1_600_000_000, 1_600_000_000);
    let mut cmd = isolated_agy_cmd(home);
    cmd.arg("--full").env("NO_COLOR", "1");
    seed_alpha_rate(home);
    let input = home.join("payload.json");
    let mut render = |payload: &serde_json::Value| {
        fs::write(&input, payload.to_string()).unwrap();
        let out = cmd.stdin(fs::File::open(&input).unwrap()).output().unwrap();
        assert!(out.status.success());
        String::from_utf8(out.stdout).unwrap()
    };
    let mut payload = serde_json::json!({
        "model": {"id": "model-alpha"},
        "context_window": {"current_usage": {"input_tokens": 1000000}}
    });
    assert!(render(&payload).contains("~$1.00 (latest turn)"));
    payload["conversation_id"] = uuid.into();
    assert!(render(&payload).contains("~$0.01 (1 unpriced)"));
    let conn = Connection::open(cli.join(format!("conversations/{uuid}.db"))).unwrap();
    conn.execute("DELETE FROM gen_metadata WHERE idx = 1", []).unwrap();
    assert!(!render(&payload).contains('$'));
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

#[test]
fn cost_widget_attributes_descendant_subagent_dbs_from_conversation_summaries() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let cli = home.join(".gemini/antigravity-cli");
    let parent = "11111111-1111-4111-8111-111111111111";
    let child = "22222222-2222-4222-8222-222222222222";
    let grandchild = "33333333-3333-4333-8333-333333333333";

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
            cache_read_tok: 0,
            output_tok: 0,
            thinking_tok: 0,
            dur_secs: 1,
            ttft_ms: 200,
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
            cache_read_tok: 0,
            output_tok: 0,
            thinking_tok: 0,
            dur_secs: 1,
            ttft_ms: 200,
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
            cache_read_tok: 0,
            output_tok: 0,
            thinking_tok: 0,
            dur_secs: 1,
            ttft_ms: 200,
        },
    );

    let mut cmd = isolated_agy_cmd(home);
    cmd.arg("--full").env("NO_COLOR", "1");
    seed_alpha_rate(home);
    let input = home.join("payload.json");
    let payload = serde_json::json!({
        "conversation_id": parent,
        "terminal_width": 240
    });
    fs::write(&input, payload.to_string()).unwrap();

    // Without conversation_summaries.db, only parent spend ($0.10) is shown.
    let out_parent_only = cmd
        .stdin(fs::File::open(&input).unwrap())
        .output()
        .unwrap();
    assert!(out_parent_only.status.success());
    let text_parent_only = String::from_utf8(out_parent_only.stdout).unwrap();
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

    let out_with_subs = cmd
        .stdin(fs::File::open(&input).unwrap())
        .output()
        .unwrap();
    assert!(out_with_subs.status.success());
    let text_with_subs = String::from_utf8(out_with_subs.stdout).unwrap();
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
            cache_read_tok: 0,
            output_tok: 0,
            thinking_tok: 0,
            dur_secs: 1,
            ttft_ms: 200,
        },
    );
    let out_partial = cmd
        .stdin(fs::File::open(&input).unwrap())
        .output()
        .unwrap();
    assert!(out_partial.status.success());
    let text_partial = String::from_utf8(out_partial.stdout).unwrap();
    assert!(
        text_partial.contains("~$0.45 (sub $0.35, 1 unpriced)"),
        "expected ~$0.45 (sub $0.35, 1 unpriced) in: {text_partial}"
    );
}
