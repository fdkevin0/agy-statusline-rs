//! Tier 2 CLI integration tests for quota precision, `reset_time` fallback, and pool routing.

use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

use agy_statusline::layout::{GlyphMode, LayoutConfig, ThemeStyle, WidgetId};

#[test]
fn cli_renders_sub_one_percent_quota_and_expired_reset_time_with_preset_full() {
    let dir = std::env::temp_dir().join(format!("agy_quota_cli_1_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();

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

    let mut child = Command::new(env!("CARGO_BIN_EXE_agy-statusline"))
        .arg("--preset")
        .arg("full")
        .env("HOME", &dir)
        .env("XDG_CONFIG_HOME", dir.join("cfg"))
        .env("XDG_CACHE_HOME", dir.join("cache"))
        .env("NO_COLOR", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();

    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();

    let _ = fs::remove_dir_all(&dir);

    assert!(output.status.success());
    assert!(stdout.contains("3p 5h 0.4%"), "unexpected stdout: {stdout}");
    assert!(!stdout.contains("3p 5h 0%"), "unexpected stdout: {stdout}");
    assert!(
        !stdout.contains('⟳'),
        "unexpected timer in stdout: {stdout}"
    );
}

#[test]
fn cli_renders_sub_tenth_percent_and_carbon_weekly_with_isolated_config() {
    let dir = std::env::temp_dir().join(format!("agy_quota_cli_2_{}", std::process::id()));
    let cfg_dir = dir.join("cfg").join("agy-statusline");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&cfg_dir).unwrap();

    let mut cfg = LayoutConfig::preset_minimal();
    cfg.theme = ThemeStyle::Mono;
    cfg.glyph_mode = GlyphMode::Ascii;
    cfg.no_color = true;
    cfg.rows = vec![vec![WidgetId::Quota5h, WidgetId::QuotaWeekly]];
    fs::write(
        cfg_dir.join("config.json"),
        serde_json::to_string(&cfg).unwrap(),
    )
    .unwrap();

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

    let mut child = Command::new(env!("CARGO_BIN_EXE_agy-statusline"))
        .env("HOME", &dir)
        .env("XDG_CONFIG_HOME", dir.join("cfg"))
        .env("XDG_CACHE_HOME", dir.join("cache"))
        .env("NO_COLOR", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();

    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();

    let _ = fs::remove_dir_all(&dir);

    assert!(output.status.success());
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
    let dir = std::env::temp_dir().join(format!("agy_quota_cli_1510_{}", std::process::id()));
    let cfg_dir = dir.join("cfg").join("agy-statusline");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&cfg_dir).unwrap();

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
    fs::write(
        cfg_dir.join("config.json"),
        serde_json::to_string(&cfg).unwrap(),
    )
    .unwrap();

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

    let mut child = Command::new(env!("CARGO_BIN_EXE_agy-statusline"))
        .env("HOME", &dir)
        .env("XDG_CONFIG_HOME", dir.join("cfg"))
        .env("XDG_CACHE_HOME", dir.join("cache"))
        .env("NO_COLOR", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();

    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();

    let _ = fs::remove_dir_all(&dir);

    assert!(output.status.success());
    assert_eq!(
        stdout,
        "* DONE * DeepSeek R2 (Turbo) * sub 1 * 3p 5h 450 * 3p 7d 72% r:2d\n"
    );
}
