#![forbid(unsafe_code)]
//! Property-based (`proptest`) and end-to-end integration tests for `agy-statusline`.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::process::Command;

use proptest::prelude::*;

use agy_statusline::db::{is_valid_uuid, SessionMetadata, SpeedReader};
use agy_statusline::format::format_tokens;
use agy_statusline::git::{GitInfo, SystemGitProbe};
use agy_statusline::layout::{GlyphMode, LayoutConfig, ThemeStyle, VisualStyle, WidgetId};
use agy_statusline::protobuf::{
    extract_gen_metadata_record, extract_metadata_speed, extract_step_created_at_secs,
    MAX_TIMESTAMP_SECS, MAX_TTFT_SECS, MAX_TURN_SPEED_TPS, MIN_TIMESTAMP_SECS,
};
use agy_statusline::render::{build_progress_bar, write_statusline};
use agy_statusline::statusline::{
    process_statusline, ContextWindow, CostInfo, JsonUsage, ModelInfo, QuotaBucket,
    StatuslinePayload, StatuslineRuntime, WorkspaceInfo,
};

use common::{isolated_agy_cmd, visible_line_width, BIN};

struct FixedSpeed(Option<f64>);
impl SpeedReader for FixedSpeed {
    fn session_metadata(&self, _id: &str) -> SessionMetadata {
        SessionMetadata {
            speed: self.0,
            median_speed: self.0,
            last_turn_speed: self.0,
            ..Default::default()
        }
    }
}

proptest! {
    #[test]
    fn prop_extract_gen_metadata_record_never_panics(data in prop::collection::vec(any::<u8>(), 0..128)) {
        let rec = extract_gen_metadata_record(&data);
        let valid = rec.as_ref().is_none_or(|r| {
            r.decode_secs.is_none_or(|s| s.is_finite() && s >= 0.0)
                && r.ttft_secs.is_none_or(|t| t > 0.0 && t < MAX_TTFT_SECS)
                && r.model_cost_usd.is_none_or(|c| c.is_finite() && c > 0.0)
                && r.chat_start_created_at_secs
                    .is_none_or(|ts| (MIN_TIMESTAMP_SECS..=MAX_TIMESTAMP_SECS).contains(&ts))
        });
        prop_assert!(valid);
    }

    #[test]
    fn prop_extract_step_created_at_secs_bounds(data in prop::collection::vec(any::<u8>(), 0..128)) {
        let ts = extract_step_created_at_secs(&data);
        prop_assert!(ts.is_none_or(|t| (MIN_TIMESTAMP_SECS..=MAX_TIMESTAMP_SECS).contains(&t)));
    }

    #[test]
    fn prop_extract_metadata_speed_bounds(data in prop::collection::vec(any::<u8>(), 0..128)) {
        let speed = extract_metadata_speed(&data);
        prop_assert!(speed.is_none_or(|s| s > 0.0 && s < MAX_TURN_SPEED_TPS));
    }

    #[test]
    fn prop_format_tokens_small_no_suffix(value in 0u64..1000) {
        let mut buf = String::new();
        format_tokens(value, &mut buf);
        prop_assert!(!buf.ends_with('K'));
    }

    #[test]
    fn prop_format_tokens_k_range_has_k_suffix(value in 1000u64..999_500) {
        let mut buf = String::new();
        format_tokens(value, &mut buf);
        prop_assert!(buf.ends_with('K'));
    }

    #[test]
    fn prop_format_tokens_m_range_has_m_suffix(value in 999_500u64..999_500_000) {
        let mut buf = String::new();
        format_tokens(value, &mut buf);
        prop_assert!(buf.ends_with('M'));
    }

    #[test]
    fn prop_format_tokens_b_range_has_b_suffix(value in 999_500_000u64..100_000_000_000) {
        let mut buf = String::new();
        format_tokens(value, &mut buf);
        prop_assert!(buf.ends_with('B'));
    }

    #[test]
    fn prop_progress_bar_clamps_and_matches_char_len(pct in -50.0f64..200.0, bar_len in 4usize..40) {
        let (bar, clamped) = build_progress_bar(pct, bar_len);
        prop_assert!((0.0..=100.0).contains(&clamped));
        prop_assert_eq!(bar.chars().count(), bar_len);
    }

    #[test]
    fn prop_responsive_row_never_exceeds_terminal_width(
        style_idx in 0usize..3,
        theme_idx in 0usize..7,
        glyph_idx in 0usize..3,
        term_width in 24usize..120,
    ) {
        let styles = [VisualStyle::Plain, VisualStyle::Pill, VisualStyle::Powerline];
        let themes = [
            ThemeStyle::Default,
            ThemeStyle::Nord,
            ThemeStyle::Catppuccin,
            ThemeStyle::TokyoNight,
            ThemeStyle::Gruvbox,
            ThemeStyle::Light,
            ThemeStyle::Mono,
        ];
        let glyphs = [GlyphMode::Unicode, GlyphMode::Nerd, GlyphMode::Ascii];

        let mut cfg = LayoutConfig::preset_full();
        cfg.rows[0].push(WidgetId::Spacer);
        cfg.rows[0].push(WidgetId::QuotaWeekly);
        cfg.style = styles[style_idx];
        cfg.theme = themes[theme_idx];
        cfg.glyph_mode = glyphs[glyph_idx];
        cfg.responsive = true;

        let mut quota = BTreeMap::new();
        quota.insert(
            "gemini-5h".to_string(),
            QuotaBucket {
                remaining_fraction: Some(0.75),
                reset_in_seconds: Some(7200),
                ..Default::default()
            },
        );
        quota.insert(
            "gemini-weekly".to_string(),
            QuotaBucket {
                remaining_fraction: Some(0.92),
                reset_in_seconds: Some(300_000),
                ..Default::default()
            },
        );

        let payload = StatuslinePayload {
            agent_state: Some("thinking".to_string()),
            cycle_mode: Some("plan".to_string()),
            model: Some(ModelInfo {
                id: Some("gemini-4".to_string()),
                display_name: Some("Gemini 4".to_string()),
                effort: Some("high".to_string()),
            }),
            workspace: Some(WorkspaceInfo {
                current_dir: Some("/work/very-long-workspace-name".to_string()),
                project_dir: None,
            }),
            context_window: Some(ContextWindow {
                used_percentage: Some(42.5),
                current_usage: Some(JsonUsage {
                    input_tokens: Some(50_000),
                    output_tokens: Some(100),
                    cache_read_input_tokens: Some(150_000),
                    cache_creation_input_tokens: Some(1_000),
                }),
                ..Default::default()
            }),
            cost: Some(CostInfo {
                total_usd: Some(1.23),
                subagent_usd: Some(0.25),
                estimated: Some(true),
            }),
            quota: Some(quota),
            terminal_width: Some(term_width),
            ..Default::default()
        };
        let meta = SessionMetadata {
            speed: Some(85.0),
            last_turn_speed: Some(90.0),
            e2e_speed: Some(60.0),
            ttft_secs: Some(1.2),
            max_context_tokens: Some(512_000),
            ..Default::default()
        };
        let git = GitInfo {
            branch: Some("feat/extremely-long-branch-name-that-overflows".to_string()),
            worktree: Some("wt-long".to_string()),
            added: 120,
            deleted: 45,
        };

        let mut out = Vec::new();
        write_statusline(&payload, &meta, &git, &cfg, &mut out).unwrap();
        let raw = String::from_utf8(out).unwrap();
        prop_assert_eq!(raw.lines().count(), 1);
        let visible_w = visible_line_width(&raw);
        prop_assert!(
            visible_w <= term_width,
            "visible_w={visible_w} exceeded term_width={term_width} for {:?}/{:?}/{:?}: {:?}",
            cfg.style,
            cfg.theme,
            cfg.glyph_mode,
            raw
        );
    }
}

#[test]
fn integration_full_json_and_uuid_checks() {
    assert!(is_valid_uuid("01234567-89ab-cdef-0123-456789abcdef"));
    assert!(!is_valid_uuid("01234567-89ab-cdef-0123-456789abcde"));

    let json = r#"{
        "agent_state": "working",
        "model": {"display_name": "Gemini 4", "effort": "high"},
        "vcs": {"branch": "main", "dirty": true},
        "terminal_width": 120,
        "conversation_id": "01234567-89ab-cdef-0123-456789abcdef",
        "context_window": {
            "used_percentage": 35.0,
            "current_usage": {
                "input_tokens": 500,
                "cache_read_input_tokens": 1500,
                "cache_creation_input_tokens": 0
            }
        }
    }"#;
    let reader = FixedSpeed(Some(120.4));
    let git = SystemGitProbe::default();
    let runtime = StatuslineRuntime::minimal(&reader, &git);
    let mut out = Vec::new();
    process_statusline(json, &runtime, &LayoutConfig::default(), &mut out).unwrap();
    let s = String::from_utf8(out).unwrap();
    assert!(s.contains("WORKING"));
    assert!(!s.contains("Gemini 4"));
    assert!(s.contains("main*"));
    assert!(s.contains("75.0%"));
    assert!(s.contains("120 tok/s"));
}

#[test]
fn live_git_cli_probe_extracts_branch_and_diff_stats() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();

    let init_ok = Command::new("git")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .args(["init"])
        .arg(repo)
        .output()
        .unwrap();
    assert!(init_ok.status.success());

    let branch_ok = Command::new("git")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .args(["-C"])
        .arg(repo)
        .args(["checkout", "-b", "feat-live"])
        .output()
        .unwrap();
    assert!(branch_ok.status.success());

    fs::write(repo.join("notes.txt"), "line1\nline2\n").unwrap();
    let add_ok = Command::new("git")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .args(["-C"])
        .arg(repo)
        .args(["add", "notes.txt"])
        .output()
        .unwrap();
    assert!(add_ok.status.success());

    let commit_ok = Command::new("git")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .args(["-C"])
        .arg(repo)
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
    assert!(commit_ok.status.success());

    fs::write(repo.join("notes.txt"), "line1\nchanged\nadded_line\n").unwrap();

    let payload = serde_json::json!({
        "agent_state": "idle",
        "cwd": repo,
        "vcs": { "type": "git" }
    })
    .to_string();

    let mut cfg = LayoutConfig::preset_minimal();
    cfg.theme = ThemeStyle::Mono;
    cfg.show_git_diff = true;
    let probe = SystemGitProbe {
        cache_dir: Some(repo.join(".tmp_cache")),
    };

    let reader = FixedSpeed(None);
    let runtime = StatuslineRuntime::minimal(&reader, &probe);
    let mut out = Vec::new();
    process_statusline(&payload, &runtime, &cfg, &mut out).unwrap();
    let rendered = String::from_utf8(out).unwrap();
    assert!(rendered.contains("feat-live* +2 -1"));
}

#[test]
fn tier2_apply_and_doctor_honor_antigravity_cli_home_and_agy_home() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();
    let cli_home = base.join("custom_cli_home");
    let agy_home = base.join("custom_agy_home");

    // ANTIGRAVITY_CLI_HOME takes precedence over AGY_HOME and HOME.
    let apply_cli = isolated_agy_cmd(base)
        .env("ANTIGRAVITY_CLI_HOME", &cli_home)
        .env("AGY_HOME", &agy_home)
        .env("NO_COLOR", "1")
        .args(["apply", "--yes"])
        .output()
        .unwrap();
    assert!(apply_cli.status.success());
    assert!(cli_home.join("settings.json").is_file());
    assert!(!agy_home.join("settings.json").exists());
    assert!(!base.join(".gemini/antigravity-cli/settings.json").exists());

    // AGY_HOME is used when ANTIGRAVITY_CLI_HOME is unset.
    let apply_agy = isolated_agy_cmd(base)
        .env_remove("ANTIGRAVITY_CLI_HOME")
        .env("AGY_HOME", &agy_home)
        .env("NO_COLOR", "1")
        .args(["apply", "--yes"])
        .output()
        .unwrap();
    assert!(apply_agy.status.success());
    assert!(agy_home.join("settings.json").is_file());

    let doctor_out = isolated_agy_cmd(base)
        .env_remove("ANTIGRAVITY_CLI_HOME")
        .env("AGY_HOME", &agy_home)
        .env("NO_COLOR", "1")
        .args(["doctor"])
        .output()
        .unwrap();
    assert!(doctor_out.status.success());
    let doctor_text = String::from_utf8(doctor_out.stdout).unwrap();
    assert!(
        doctor_text.contains(&agy_home.join("settings.json").display().to_string()),
        "doctor stdout: {doctor_text}"
    );
    assert!(doctor_text.contains("enabled=true"));
}

#[test]
fn cli_version_command_and_flags() {
    let expected = format!("{} {}\n", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));

    let out_cmd = Command::new(BIN).arg("version").output().unwrap();
    assert!(out_cmd.status.success());
    assert_eq!(String::from_utf8(out_cmd.stdout).unwrap(), expected);

    let out_long = Command::new(BIN).arg("--version").output().unwrap();
    assert!(out_long.status.success());
    assert_eq!(String::from_utf8(out_long.stdout).unwrap(), expected);

    let out_short = Command::new(BIN).arg("-V").output().unwrap();
    assert!(out_short.status.success());
    assert_eq!(String::from_utf8(out_short.stdout).unwrap(), expected);
}
