//! Integration tests for PR 3 (`feat/p1-responsive-compaction`).

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

use unicode_width::UnicodeWidthStr;

use agy_statusline::ansi::ansi_to_ratatui_lines;
use agy_statusline::db::SessionMetadata;
use agy_statusline::git::GitInfo;
use agy_statusline::layout::{GlyphMode, LayoutConfig, ThemeStyle, VisualStyle, WidgetId};
use agy_statusline::render::write_statusline;
use agy_statusline::statusline::{
    ContextWindow, CostInfo, JsonUsage, ModelInfo, QuotaBucket, StatuslinePayload, WorkspaceInfo,
};

fn visible_line_width(rendered: &str) -> usize {
    let lines = ansi_to_ratatui_lines(rendered);
    lines
        .first()
        .map_or(0, |l| l.spans.iter().map(|s| s.content.as_ref().width()).sum())
}

fn render_mono(payload: &StatuslinePayload, meta: &SessionMetadata, git: &GitInfo, cfg: &LayoutConfig) -> String {
    let mut out = Vec::new();
    write_statusline(payload, meta, git, cfg, &mut out).unwrap();
    String::from_utf8(out).unwrap()
}

fn base_payload(width: usize) -> StatuslinePayload {
    let mut quota = BTreeMap::new();
    quota.insert(
        "gemini-5h".to_string(),
        QuotaBucket {
            remaining_fraction: Some(0.84),
            reset_in_seconds: Some(11_400),
            reset_time: None,
        },
    );
    quota.insert(
        "gemini-weekly".to_string(),
        QuotaBucket {
            remaining_fraction: Some(0.92),
            reset_in_seconds: Some(432_000),
            reset_time: None,
        },
    );
    StatuslinePayload {
        agent_state: Some("thinking".to_string()),
        cycle_mode: Some("plan".to_string()),
        model: Some(ModelInfo {
            id: None,
            display_name: Some("Gemini 3.1 Pro Preview".to_string()),
            effort: Some("high".to_string()),
        }),
        workspace: Some(WorkspaceInfo {
            current_dir: Some("/work/agy-statusline".to_string()),
            project_dir: Some("/work/agy-statusline".to_string()),
        }),
        context_window: Some(ContextWindow {
            current_usage: Some(JsonUsage {
                input_tokens: Some(56_000),
                output_tokens: None,
                cache_read_input_tokens: Some(200_000),
                cache_creation_input_tokens: Some(0),
            }),
            ..Default::default()
        }),
        cost: Some(CostInfo {
            total_usd: Some(0.42),
            subagent_usd: None,
            estimated: Some(true),
        }),
        quota: Some(quota),
        terminal_width: Some(width),
        ..Default::default()
    }
}

#[test]
fn stage1_vcs_strips_diff_stats_then_caps_at_20_cols() {
    let mut cfg = LayoutConfig::preset_minimal();
    cfg.theme = ThemeStyle::Mono;
    cfg.rows = vec![vec![WidgetId::Vcs, WidgetId::Speed]];
    let meta = SessionMetadata {
        speed: Some(64.0),
        ..Default::default()
    };

    // Strip diff stats (+120 -45) so branch + Speed both fit at width 28.
    let git_short = GitInfo {
        branch: Some("feat-branch".to_string()),
        added: 120,
        deleted: 45,
        ..Default::default()
    };
    let s1 = render_mono(&base_payload(28), &meta, &git_short, &cfg);
    assert!(s1.contains("feat-branch*"));
    assert!(!s1.contains("+120"));
    assert!(s1.contains("64 tok/s"));
    assert!(visible_line_width(&s1) <= 28);

    // Long branch still overflows after stripping diff stats -> capped at 20 cols with ellipsis.
    let git_long = GitInfo {
        branch: Some("feature/extremely-long-branch-name".to_string()),
        added: 10,
        deleted: 2,
        ..Default::default()
    };
    let s2 = render_mono(&base_payload(35), &meta, &git_long, &cfg);
    assert!(!s2.contains("+10"));
    assert!(s2.contains('…'));
    assert!(s2.contains("64 tok/s"));
    assert!(visible_line_width(&s2) <= 35);
}

#[test]
fn stage2_quota_strips_timer_before_dropping_widgets() {
    let mut cfg = LayoutConfig::preset_minimal();
    cfg.theme = ThemeStyle::Mono;
    cfg.rows = vec![vec![
        WidgetId::Vcs,
        WidgetId::Speed,
        WidgetId::Cost,
        WidgetId::Quota5h,
        WidgetId::QuotaWeekly,
    ]];
    let git = GitInfo {
        branch: Some("main".to_string()),
        ..Default::default()
    };
    let meta = SessionMetadata {
        speed: Some(64.0),
        ..Default::default()
    };

    // Uncompacted: "⎇ main · ⚡ 64 tok/s · ~$0.42 · 5h 84% ↻3h10m · 7d 92% ↻5d" (58 cols)
    // After Stage 2: "⎇ main · ⚡ 64 tok/s · ~$0.42 · 5h 84% · 7d 92%" (47 cols)
    let out = render_mono(&base_payload(48), &meta, &git, &cfg);
    assert!(out.contains("64 tok/s"));
    assert!(out.contains("~$0.42"));
    assert!(out.contains("5h 84%"));
    assert!(out.contains("7d 92%"));
    assert!(!out.contains("3h10m"));
    assert!(!out.contains("5d"));
    assert!(visible_line_width(&out) <= 48);
}

#[test]
fn stage3_88col_row_compacts_ctx_bar_instead_of_dropping_speed_cost_vcs() {
    let mut cfg = LayoutConfig::preset_minimal();
    cfg.theme = ThemeStyle::Mono;
    cfg.rows = vec![vec![
        WidgetId::State,
        WidgetId::Model,
        WidgetId::Vcs,
        WidgetId::CtxBar,
        WidgetId::Speed,
        WidgetId::Cost,
        WidgetId::Quota5h,
    ]];
    let git = GitInfo {
        branch: Some("feat/ui".to_string()),
        added: 12,
        deleted: 3,
        ..Default::default()
    };
    let meta = SessionMetadata {
        speed: Some(82.0),
        max_context_tokens: Some(512_000),
        ..Default::default()
    };

    // At 90 cols (within 80-95 col window), Stages 1-4 compact Vcs/Quota/CtxBar/Model
    // so Speed, Cost, Vcs, Quota5h, and ctx 50.0% all survive (uncompacted = 133 cols, compacted = 89 cols).
    let out = render_mono(&base_payload(90), &meta, &git, &cfg);
    assert!(out.contains("feat/ui*"));
    assert!(out.contains("82 tok/s"));
    assert!(out.contains("~$0.42"));
    assert!(out.contains("ctx 50.0%"));
    assert!(out.contains("5h 84%"));
    assert!(!out.contains("(512K)"));
    assert!(!out.contains('█'));
    assert!(visible_line_width(&out) <= 90);

    // Remaining mode collapses to "rem 50.0%".
    let mut rem_cfg = cfg.clone();
    rem_cfg.ctx_remaining = true;
    let out_rem = render_mono(&base_payload(90), &meta, &git, &rem_cfg);
    assert!(out_rem.contains("rem 50.0%"));
    assert!(visible_line_width(&out_rem) <= 90);
}

#[test]
fn stage3_ctx_pct_strips_limit_suffix_and_stage4_abbreviates_models() {
    let mut cfg = LayoutConfig::preset_minimal();
    cfg.theme = ThemeStyle::Mono;
    cfg.rows = vec![vec![
        WidgetId::Model,
        WidgetId::CtxPct,
        WidgetId::Speed,
        WidgetId::Cost,
    ]];
    let meta = SessionMetadata {
        speed: Some(75.0),
        max_context_tokens: Some(512_000),
        ..Default::default()
    };

    // Uncompacted: "Gemini 3.1 Pro Preview · high · ctx 50.0% (512K) · ⚡ 75 tok/s · ~$0.42" (74 cols)
    // Stage 3 strips (512K) -> 67 cols; Stage 4 abbreviates "Gemini " -> "G" & caps Model at 20 cols -> 55 cols.
    let out = render_mono(&base_payload(56), &meta, &GitInfo::default(), &cfg);
    assert!(out.contains("G3.1 Pro Preview"));
    assert!(out.contains("ctx 50.0%"));
    assert!(!out.contains("(512K)"));
    assert!(out.contains("75 tok/s"));
    assert!(out.contains("~$0.42"));
    assert!(visible_line_width(&out) <= 56);

    // Claude abbreviation check: "Claude Sonnet 4.6 (Thinking)" -> "CSonnet 4.6 (Thin…" (20 cols), total 54 cols.
    let mut claude_payload = base_payload(55);
    claude_payload.model = Some(ModelInfo {
        id: None,
        display_name: Some("Claude Sonnet 4.6 (Thinking)".to_string()),
        effort: None,
    });
    let out_claude = render_mono(&claude_payload, &meta, &GitInfo::default(), &cfg);
    assert!(out_claude.contains("CSonnet 4.6"));
    assert!(out_claude.contains("75 tok/s"));
    assert!(out_claude.contains("~$0.42"));
    assert!(visible_line_width(&out_claude) <= 55);
}

fn render_with_style(style: VisualStyle, width: usize) -> String {
    let mut cfg = LayoutConfig::preset_full();
    cfg.style = style;
    cfg.theme = ThemeStyle::Default;
    cfg.glyph_mode = GlyphMode::Unicode;
    let git = GitInfo {
        branch: Some("feat/extremely-long-branch-name".to_string()),
        worktree: Some("wt-long".to_string()),
        added: 88,
        deleted: 19,
    };
    let meta = SessionMetadata {
        speed: Some(64.0),
        max_context_tokens: Some(512_000),
        ..Default::default()
    };
    let mut out = Vec::new();
    write_statusline(&base_payload(width), &meta, &git, &cfg, &mut out).unwrap();
    String::from_utf8(out).unwrap()
}

#[test]
fn stage5_ultra_narrow_widths_never_exceed_limit() {
    assert!(visible_line_width(&render_with_style(VisualStyle::Plain, 28)) <= 28);
    assert!(visible_line_width(&render_with_style(VisualStyle::Pill, 28)) <= 28);
    assert!(visible_line_width(&render_with_style(VisualStyle::Powerline, 28)) <= 28);
    assert!(visible_line_width(&render_with_style(VisualStyle::Plain, 36)) <= 36);
    assert!(visible_line_width(&render_with_style(VisualStyle::Pill, 36)) <= 36);
    assert!(visible_line_width(&render_with_style(VisualStyle::Powerline, 36)) <= 36);
}

#[test]
fn tier2_binary_compacts_88col_row_and_keeps_core_widgets() {
    let dir = std::env::temp_dir().join(format!("agy_compact_tier2_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let xdg_cfg = dir.join(".config");
    let cfg_dir = xdg_cfg.join("agy-statusline");
    fs::create_dir_all(&cfg_dir).unwrap();

    let mut cfg = LayoutConfig::preset_minimal();
    cfg.theme = ThemeStyle::Mono;
    cfg.responsive = true;
    cfg.rows = vec![vec![
        WidgetId::State,
        WidgetId::Model,
        WidgetId::Vcs,
        WidgetId::CtxBar,
        WidgetId::Cost,
        WidgetId::Quota5h,
    ]];
    cfg.normalize_rows();
    fs::write(
        cfg_dir.join("config.json"),
        serde_json::to_string(&cfg).unwrap(),
    )
    .unwrap();

    let stdin_json = r#"{
        "agent_state": "working",
        "model": {"display_name": "Gemini 3.1 Pro Preview", "effort": "high"},
        "vcs": {"branch": "feat/ui", "dirty": true},
        "context_window": {
            "context_window_size": 512000,
            "current_usage": {"input_tokens": 56000, "cache_read_input_tokens": 200000}
        },
        "cost": {"total_usd": 0.42, "estimated": true},
        "quota": {"gemini-5h": {"remaining_fraction": 0.84, "reset_in_seconds": 11400}},
        "terminal_width": 78
    }"#;

    let mut child = Command::new(env!("CARGO_BIN_EXE_agy-statusline"))
        .env("HOME", &dir)
        .env("XDG_CONFIG_HOME", &xdg_cfg)
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
    assert!(stdout.contains("G3.1 Pro Preview"), "stdout: {stdout}");
    assert!(stdout.contains("feat/ui*"), "stdout: {stdout}");
    assert!(stdout.contains("ctx 50.0%"), "stdout: {stdout}");
    assert!(stdout.contains("~$0.42"), "stdout: {stdout}");
    assert!(stdout.contains("5h 84%"), "stdout: {stdout}");
    assert!(visible_line_width(&stdout) <= 78, "stdout: {stdout}");

    let _ = fs::remove_dir_all(&dir);
}
