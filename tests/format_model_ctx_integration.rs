//! Tier 2 CLI integration tests for token K-formatting, canonical effort matching, and >100% context overflow.

use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

use agy_statusline::layout::{GlyphMode, LayoutConfig, ThemeStyle, WidgetId};

#[test]
fn cli_renders_minimax_effort_and_context_overflow_with_tenth_k() {
    let dir = std::env::temp_dir().join(format!("agy_fmt_ctx_1_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();

    let payload = serde_json::json!({
        "agent_state": "idle",
        "model": {
            "id": "minimax-m2",
            "display_name": "MiniMax M2",
            "effort": "max"
        },
        "context_window": {
            "context_window_size": 1500,
            "current_usage": {
                "input_tokens": 536,
                "cache_read_input_tokens": 1000,
                "cache_creation_input_tokens": 0
            }
        }
    })
    .to_string();

    let mut child = Command::new(env!("CARGO_BIN_EXE_agy-statusline"))
        .arg("--preset")
        .arg("minimal")
        .env("HOME", &dir)
        .env("USERPROFILE", &dir)
        .env("XDG_CONFIG_HOME", dir.join("cfg"))
        .env("XDG_CACHE_HOME", dir.join("cache"))
        .env_remove("ANTIGRAVITY_CLI_HOME")
        .env_remove("AGY_HOME")
        .env_remove("AGY_STATUSLINE_FULL")
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
        stdout.contains("MiniMax M2 · max"),
        "unexpected stdout: {stdout}"
    );
    assert!(
        stdout.contains("ctx 102.4% (1.5K)"),
        "unexpected stdout: {stdout}"
    );
}

#[test]
fn cli_omits_duplicate_xhigh_effort_and_formats_rounded_k_tokens() {
    let dir = std::env::temp_dir().join(format!("agy_fmt_ctx_2_{}", std::process::id()));
    let cfg_dir = dir.join("cfg").join("agy-statusline");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&cfg_dir).unwrap();

    let mut cfg = LayoutConfig::preset_minimal();
    cfg.theme = ThemeStyle::Mono;
    cfg.glyph_mode = GlyphMode::Ascii;
    cfg.no_color = true;
    cfg.bar_length = 6;
    cfg.rows = vec![vec![
        WidgetId::Model,
        WidgetId::CtxBar,
        WidgetId::CtxTokens,
        WidgetId::CacheRead,
    ]];
    fs::write(
        cfg_dir.join("config.json"),
        serde_json::to_string(&cfg).unwrap(),
    )
    .unwrap();

    let payload = serde_json::json!({
        "agent_state": "thinking",
        "model": {
            "id": "gpt-oss-120b",
            "display_name": "GPT-OSS 120B (Extra High)",
            "effort": "xhigh"
        },
        "context_window": {
            "context_window_size": 10000,
            "current_usage": {
                "input_tokens": 8550,
                "cache_read_input_tokens": 1950,
                "cache_creation_input_tokens": 0
            }
        }
    })
    .to_string();

    let mut child = Command::new(env!("CARGO_BIN_EXE_agy-statusline"))
        .env("HOME", &dir)
        .env("USERPROFILE", &dir)
        .env("XDG_CONFIG_HOME", dir.join("cfg"))
        .env("XDG_CACHE_HOME", dir.join("cache"))
        .env_remove("ANTIGRAVITY_CLI_HOME")
        .env_remove("AGY_HOME")
        .env_remove("AGY_STATUSLINE_FULL")
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
        "GPT-OSS 120B (Extra High) * ctx ###### 105.0% (10K) * 11K/10K * r: 2K\n"
    );
}
