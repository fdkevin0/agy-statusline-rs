#![forbid(unsafe_code)]
//! Integration and Tier 2 CLI tests for statusline rendering, quota routing,
//! responsive compaction, SQLite median speed, thinking tokens, and AGY telemetry widgets.

mod common;

use std::collections::BTreeMap;
use std::fs;

use rusqlite::Connection;

use agy_statusline::db::SessionMetadata;
use agy_statusline::git::GitInfo;
use agy_statusline::layout::{
    save_layout_config, GlyphMode, LayoutConfig, SpeedMode, ThemeStyle, VisualStyle, WidgetId,
};
use agy_statusline::render::write_statusline;
use agy_statusline::statusline::{
    BattleInfo, ContextWindow, CostInfo, JsonUsage, ModelInfo, QuotaBucket, SandboxInfo,
    StatuslinePayload, VimInfo, WorkspaceInfo,
};

use common::{isolated_agy_cmd, run_statusline_stdin, speed_1s_blob, visible_line_width};

fn render_mono(
    payload: &StatuslinePayload,
    meta: &SessionMetadata,
    git: &GitInfo,
    cfg: &LayoutConfig,
) -> String {
    let mut out = Vec::new();
    write_statusline(payload, meta, git, cfg, &mut out).unwrap();
    String::from_utf8(out).unwrap()
}

fn render_single(
    widget: WidgetId,
    payload: &StatuslinePayload,
    meta: &SessionMetadata,
    glyph_mode: GlyphMode,
    theme: ThemeStyle,
    smart_hide: bool,
    no_color: bool,
) -> String {
    let mut cfg = LayoutConfig::preset_minimal();
    cfg.rows = vec![vec![widget]];
    cfg.glyph_mode = glyph_mode;
    cfg.theme = theme;
    cfg.smart_hide = smart_hide;
    cfg.no_color = no_color;
    cfg.responsive = false;

    let mut out = Vec::new();
    write_statusline(payload, meta, &GitInfo::default(), &cfg, &mut out).unwrap();
    String::from_utf8(out).unwrap()
}

fn base_compaction_payload(width: usize) -> StatuslinePayload {
    let mut quota = BTreeMap::new();
    quota.insert(
        "gemini-5h".to_string(),
        QuotaBucket {
            remaining_fraction: Some(0.84),
            reset_in_seconds: Some(11_400),
            ..Default::default()
        },
    );
    quota.insert(
        "gemini-weekly".to_string(),
        QuotaBucket {
            remaining_fraction: Some(0.92),
            reset_in_seconds: Some(432_000),
            ..Default::default()
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
    render_mono(&base_compaction_payload(width), &meta, &git, &cfg)
}

// ---------------------------------------------------------------------------
// Token K-formatting, canonical effort matching, and >100% context overflow
// ---------------------------------------------------------------------------

#[test]
fn cli_renders_minimax_effort_and_context_overflow_with_tenth_k() {
    let dir = tempfile::tempdir().unwrap();
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

    let stdout = run_statusline_stdin(dir.path(), &["--preset", "minimal"], &payload);
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
    let dir = tempfile::tempdir().unwrap();
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
    save_layout_config(&dir.path().join("config/agy-statusline/config.json"), &cfg).unwrap();

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

    let stdout = run_statusline_stdin(dir.path(), &[], &payload);
    assert_eq!(
        stdout,
        "GPT-OSS 120B (Extra High) * ctx ###### 105.0% (10K) * 11K/10K * r: 2K\n"
    );
}

// ---------------------------------------------------------------------------
// Protobuf estimated context & SQLite median speed
// ---------------------------------------------------------------------------

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

    let rendered_est = render_mono(&payload_no_usage, &meta_est, &GitInfo::default(), &cfg);
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
    let rendered_actual = render_mono(&payload_with_usage, &meta_est, &GitInfo::default(), &cfg);
    assert!(rendered_actual.contains("50.0%"));
    assert!(rendered_actual.contains("256K/512K"));

    // Zero estimated_context_tokens is ignored.
    let meta_zero = SessionMetadata {
        estimated_context_tokens: Some(0),
        max_context_tokens: Some(512_000),
        ..Default::default()
    };
    let rendered_zero = render_mono(&payload_no_usage, &meta_zero, &GitInfo::default(), &cfg);
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
    assert_eq!(
        render_mono(&payload, &meta_median, &GitInfo::default(), &cfg),
        "⚡ 73 tok/s\n"
    );

    let meta_fallback = SessionMetadata {
        speed: Some(50.0),
        median_speed: None,
        ..Default::default()
    };
    assert_eq!(
        render_mono(&payload, &meta_fallback, &GitInfo::default(), &cfg),
        "⚡ 50 tok/s\n"
    );
}

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

    let stdin_json =
        format!(r#"{{"conversation_id":"{uuid}","context_window":{{}},"terminal_width":120}}"#);
    let stdout = run_statusline_stdin(dir.path(), &[], &stdin_json);
    assert!(stdout.contains("25.0% (512K)"), "stdout was: {stdout}");
    assert!(stdout.contains("128K/512K"), "stdout was: {stdout}");
    assert!(stdout.contains("55 tok/s"), "stdout was: {stdout}");
}

// ---------------------------------------------------------------------------
// Quota precision, reset_time fallback, and pool routing
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
    assert!(stdout.contains("3p 5h 0.4%"), "unexpected stdout: {stdout}");
    assert!(!stdout.contains("3p 5h 0%"), "unexpected stdout: {stdout}");
    assert!(
        !stdout.contains('⟳'),
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
// Responsive compaction stages
// ---------------------------------------------------------------------------

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
    let s1 = render_mono(&base_compaction_payload(28), &meta, &git_short, &cfg);
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
    let s2 = render_mono(&base_compaction_payload(35), &meta, &git_long, &cfg);
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
    let out = render_mono(&base_compaction_payload(48), &meta, &git, &cfg);
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
    let out = render_mono(&base_compaction_payload(90), &meta, &git, &cfg);
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
    let out_rem = render_mono(&base_compaction_payload(90), &meta, &git, &rem_cfg);
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
    let out = render_mono(
        &base_compaction_payload(56),
        &meta,
        &GitInfo::default(),
        &cfg,
    );
    assert!(out.contains("G3.1 Pro Preview"));
    assert!(out.contains("ctx 50.0%"));
    assert!(!out.contains("(512K)"));
    assert!(out.contains("75 tok/s"));
    assert!(out.contains("~$0.42"));
    assert!(visible_line_width(&out) <= 56);

    // Claude abbreviation check: "Claude Sonnet 4.6 (Thinking)" -> "CSonnet 4.6 (Thin…" (20 cols), total 54 cols.
    let mut claude_payload = base_compaction_payload(55);
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
    let dir = tempfile::tempdir().unwrap();
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
    save_layout_config(&dir.path().join("config/agy-statusline/config.json"), &cfg).unwrap();

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

    let stdout = run_statusline_stdin(dir.path(), &[], stdin_json);
    assert!(stdout.contains("G3.1 Pro Preview"), "stdout: {stdout}");
    assert!(stdout.contains("feat/ui*"), "stdout: {stdout}");
    assert!(stdout.contains("ctx 50.0%"), "stdout: {stdout}");
    assert!(stdout.contains("~$0.42"), "stdout: {stdout}");
    assert!(stdout.contains("5h 84%"), "stdout: {stdout}");
    assert!(visible_line_width(&stdout) <= 78, "stdout: {stdout}");
}

// ---------------------------------------------------------------------------
// Thinking tokens widget
// ---------------------------------------------------------------------------

fn render_thinking(
    thinking_tokens: Option<u64>,
    glyph_mode: GlyphMode,
    theme: ThemeStyle,
    smart_hide: bool,
    no_color: bool,
) -> String {
    let meta = SessionMetadata {
        thinking_tokens,
        ..Default::default()
    };
    render_single(
        WidgetId::Thinking,
        &StatuslinePayload::default(),
        &meta,
        glyph_mode,
        theme,
        smart_hide,
        no_color,
    )
}

#[test]
fn thinking_widget_renders_glyph_modes_palette_and_smart_hide() {
    assert_eq!(
        render_thinking(Some(4_200), GlyphMode::Unicode, ThemeStyle::Mono, false, true),
        "◆ think 4.2K\n"
    );
    assert_eq!(
        render_thinking(Some(4_200), GlyphMode::Nerd, ThemeStyle::Mono, false, true),
        "\u{f0eb} think 4.2K\n"
    );
    assert_eq!(
        render_thinking(Some(4_200), GlyphMode::Ascii, ThemeStyle::Mono, false, true),
        "~ think 4.2K\n"
    );
    assert_eq!(
        render_thinking(
            Some(4_200),
            GlyphMode::Unicode,
            ThemeStyle::Default,
            false,
            false
        ),
        "\x1b[38;5;220m◆ think 4.2K\x1b[0m\n"
    );
    assert_eq!(
        render_thinking(None, GlyphMode::Unicode, ThemeStyle::Mono, true, true),
        ""
    );
    assert_eq!(
        render_thinking(Some(0), GlyphMode::Unicode, ThemeStyle::Mono, true, true),
        ""
    );
    assert_eq!(
        render_thinking(None, GlyphMode::Unicode, ThemeStyle::Default, false, false),
        "\x1b[38;5;245m◆ think --\x1b[0m\n"
    );
    assert_eq!(
        render_thinking(Some(0), GlyphMode::Nerd, ThemeStyle::Mono, false, true),
        "\u{f0eb} think --\n"
    );
    assert_eq!(
        render_thinking(None, GlyphMode::Ascii, ThemeStyle::Mono, false, true),
        "~ think --\n"
    );
}

#[test]
fn thinking_widget_drops_at_priority_1_when_narrow() {
    let mut cfg = LayoutConfig::preset_minimal();
    cfg.theme = ThemeStyle::Mono;
    cfg.no_color = true;
    cfg.responsive = true;
    cfg.rows = vec![vec![WidgetId::State, WidgetId::Model, WidgetId::Thinking]];

    let payload = StatuslinePayload {
        agent_state: Some("idle".to_string()),
        model: Some(ModelInfo {
            display_name: Some("GPT-4".to_string()),
            ..Default::default()
        }),
        terminal_width: Some(25),
        ..Default::default()
    };
    let meta = SessionMetadata {
        thinking_tokens: Some(4_200),
        ..Default::default()
    };
    let rendered = render_mono(&payload, &meta, &GitInfo::default(), &cfg);
    assert!(rendered.contains("READY"), "rendered: {rendered}");
    assert!(rendered.contains("GPT-4"), "rendered: {rendered}");
    assert!(!rendered.contains("think"), "rendered: {rendered}");
}

#[test]
fn tier2_cli_reads_real_sqlite_gen_metadata_thinking_tokens() {
    let dir = tempfile::tempdir().unwrap();
    let conv_dir = dir.path().join(".gemini/antigravity-cli/conversations");
    fs::create_dir_all(&conv_dir).unwrap();

    let mut cfg = LayoutConfig::preset_minimal();
    cfg.rows = vec![vec![WidgetId::State, WidgetId::Thinking]];
    cfg.theme = ThemeStyle::Default;
    cfg.glyph_mode = GlyphMode::Unicode;
    cfg.smart_hide = true;
    cfg.responsive = false;
    save_layout_config(&dir.path().join("config/agy-statusline/config.json"), &cfg).unwrap();

    let uuid = "11223344-5566-7788-99aa-bbccddeeff00";
    let conn = Connection::open(conv_dir.join(format!("{uuid}.db"))).unwrap();
    conn.execute(
        "CREATE TABLE gen_metadata (idx INTEGER PRIMARY KEY, data BLOB)",
        [],
    )
    .unwrap();
    // chat(1) -> { usage(4) -> { output(3)=100, thinking(9)=4200 }, dur(12) -> { sec(1)=2 } }
    let blob: [u8; 13] = [
        0x0a, 0x0b, 0x22, 0x05, 0x18, 0x64, 0x48, 0xe8, 0x20, 0x62, 0x02, 0x08, 0x02,
    ];
    conn.execute("INSERT INTO gen_metadata (idx, data) VALUES (1, ?1)", [blob])
        .unwrap();
    drop(conn);

    let stdin_json =
        format!(r#"{{"conversation_id":"{uuid}","agent_state":"idle","terminal_width":120}}"#);
    let stdout = run_statusline_stdin(dir.path(), &[], &stdin_json);
    assert!(stdout.contains("◆ think 4.2K"), "stdout was: {stdout}");

    let doctor_out = isolated_agy_cmd(dir.path())
        .arg("doctor")
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    assert!(doctor_out.status.success());
    let doctor_stdout = String::from_utf8(doctor_out.stdout).unwrap();
    assert!(
        doctor_stdout.contains("◆ think 4.2K"),
        "doctor stdout was: {doctor_stdout}"
    );
}

// ---------------------------------------------------------------------------
// Telemetry & AGY payload widgets
// ---------------------------------------------------------------------------

#[test]
fn state_widget_renders_confirm_alert_on_tool_confirmation_pending() {
    let empty_meta = SessionMetadata::default();
    let pending = StatuslinePayload {
        agent_state: Some("idle".to_string()),
        tool_confirmation_pending: Some(true),
        ..Default::default()
    };
    assert_eq!(
        render_single(
            WidgetId::State,
            &pending,
            &empty_meta,
            GlyphMode::Unicode,
            ThemeStyle::Default,
            true,
            false
        ),
        "\x1b[1m\x1b[38;5;220m◎ CONFIRM\x1b[0m\n"
    );
    assert_eq!(
        render_single(
            WidgetId::State,
            &pending,
            &empty_meta,
            GlyphMode::Ascii,
            ThemeStyle::Mono,
            true,
            true
        ),
        "@ CONFIRM\n"
    );

    let not_pending = StatuslinePayload {
        agent_state: Some("idle".to_string()),
        tool_confirmation_pending: Some(false),
        ..Default::default()
    };
    assert_eq!(
        render_single(
            WidgetId::State,
            &not_pending,
            &empty_meta,
            GlyphMode::Unicode,
            ThemeStyle::Mono,
            true,
            true
        ),
        "● READY\n"
    );
}

#[test]
fn vim_widget_renders_modes_colors_and_smart_hide() {
    let empty_meta = SessionMetadata::default();
    let render_vim = |mode: Option<&str>, smart_hide: bool| {
        let payload = StatuslinePayload {
            vim: mode.map(|m| VimInfo {
                mode: Some(m.to_string()),
            }),
            ..Default::default()
        };
        render_single(
            WidgetId::Vim,
            &payload,
            &empty_meta,
            GlyphMode::Unicode,
            ThemeStyle::Default,
            smart_hide,
            false,
        )
    };

    assert_eq!(render_vim(Some("insert"), true), "\x1b[1m\x1b[38;5;78mvim INSERT\x1b[0m\n");
    assert_eq!(render_vim(Some("visual"), true), "\x1b[1m\x1b[38;5;183mvim VISUAL\x1b[0m\n");
    assert_eq!(render_vim(Some("v-line"), true), "\x1b[1m\x1b[38;5;183mvim V-LINE\x1b[0m\n");
    assert_eq!(render_vim(Some("v-block"), true), "\x1b[1m\x1b[38;5;183mvim V-BLOCK\x1b[0m\n");
    assert_eq!(render_vim(Some("command"), true), "\x1b[1m\x1b[38;5;220mvim COMMAND\x1b[0m\n");
    assert_eq!(render_vim(Some("replace"), true), "\x1b[1m\x1b[38;5;220mvim REPLACE\x1b[0m\n");
    assert_eq!(render_vim(Some(" normal "), true), "\x1b[1m\x1b[38;5;117mvim NORMAL\x1b[0m\n");
    assert_eq!(render_vim(Some("   "), true), "");
    assert_eq!(render_vim(None, false), "\x1b[38;5;245mvim off\x1b[0m\n");
}

#[test]
fn queue_widget_renders_count_and_smart_hide() {
    let empty_meta = SessionMetadata::default();
    let active = StatuslinePayload {
        pending_input_count: Some(3),
        ..Default::default()
    };
    assert_eq!(
        render_single(
            WidgetId::Queue,
            &active,
            &empty_meta,
            GlyphMode::Unicode,
            ThemeStyle::Default,
            true,
            false
        ),
        "\x1b[1m\x1b[38;5;220mqueue 3\x1b[0m\n"
    );

    let zero = StatuslinePayload {
        pending_input_count: Some(0),
        ..Default::default()
    };
    assert_eq!(
        render_single(
            WidgetId::Queue,
            &zero,
            &empty_meta,
            GlyphMode::Unicode,
            ThemeStyle::Default,
            true,
            false
        ),
        ""
    );
    assert_eq!(
        render_single(
            WidgetId::Queue,
            &StatuslinePayload::default(),
            &empty_meta,
            GlyphMode::Unicode,
            ThemeStyle::Default,
            false,
            false
        ),
        "\x1b[38;5;245mqueue 0\x1b[0m\n"
    );
}

#[test]
fn battle_widget_renders_status_arm_and_smart_hide() {
    let empty_meta = SessionMetadata::default();
    let with_arm = StatuslinePayload {
        battle: Some(BattleInfo {
            status: Some("active".to_string()),
            focused_arm: Some(2),
        }),
        ..Default::default()
    };
    assert_eq!(
        render_single(
            WidgetId::Battle,
            &with_arm,
            &empty_meta,
            GlyphMode::Unicode,
            ThemeStyle::Default,
            true,
            false
        ),
        "\x1b[1m\x1b[38;5;141mbattle ACTIVE #2\x1b[0m\n"
    );

    let no_arm = StatuslinePayload {
        battle: Some(BattleInfo {
            status: Some("tie".to_string()),
            focused_arm: None,
        }),
        ..Default::default()
    };
    assert_eq!(
        render_single(
            WidgetId::Battle,
            &no_arm,
            &empty_meta,
            GlyphMode::Unicode,
            ThemeStyle::Default,
            true,
            false
        ),
        "\x1b[1m\x1b[38;5;141mbattle TIE\x1b[0m\n"
    );
    assert_eq!(
        render_single(
            WidgetId::Battle,
            &StatuslinePayload::default(),
            &empty_meta,
            GlyphMode::Unicode,
            ThemeStyle::Default,
            true,
            false
        ),
        ""
    );
    assert_eq!(
        render_single(
            WidgetId::Battle,
            &StatuslinePayload::default(),
            &empty_meta,
            GlyphMode::Unicode,
            ThemeStyle::Default,
            false,
            false
        ),
        "\x1b[38;5;245mbattle off\x1b[0m\n"
    );
}

#[test]
fn sandbox_widget_renders_allow_network_states() {
    let empty_meta = SessionMetadata::default();
    let on_net = StatuslinePayload {
        sandbox: Some(SandboxInfo {
            enabled: Some(true),
            allow_network: Some(true),
        }),
        ..Default::default()
    };
    assert_eq!(
        render_single(
            WidgetId::Sandbox,
            &on_net,
            &empty_meta,
            GlyphMode::Unicode,
            ThemeStyle::Mono,
            true,
            true
        ),
        "sandbox ON+net\n"
    );

    let on_no_net = StatuslinePayload {
        sandbox: Some(SandboxInfo {
            enabled: Some(true),
            allow_network: Some(false),
        }),
        ..Default::default()
    };
    assert_eq!(
        render_single(
            WidgetId::Sandbox,
            &on_no_net,
            &empty_meta,
            GlyphMode::Unicode,
            ThemeStyle::Mono,
            true,
            true
        ),
        "sandbox ON\n"
    );

    let on_none_net = StatuslinePayload {
        sandbox: Some(SandboxInfo {
            enabled: Some(true),
            allow_network: None,
        }),
        ..Default::default()
    };
    assert_eq!(
        render_single(
            WidgetId::Sandbox,
            &on_none_net,
            &empty_meta,
            GlyphMode::Unicode,
            ThemeStyle::Mono,
            true,
            true
        ),
        "sandbox ON\n"
    );

    let off_with_net = StatuslinePayload {
        sandbox: Some(SandboxInfo {
            enabled: Some(false),
            allow_network: Some(true),
        }),
        ..Default::default()
    };
    assert_eq!(
        render_single(
            WidgetId::Sandbox,
            &off_with_net,
            &empty_meta,
            GlyphMode::Unicode,
            ThemeStyle::Mono,
            true,
            true
        ),
        ""
    );
    assert_eq!(
        render_single(
            WidgetId::Sandbox,
            &off_with_net,
            &empty_meta,
            GlyphMode::Unicode,
            ThemeStyle::Mono,
            false,
            true
        ),
        "sandbox off\n"
    );
}

#[test]
fn tier2_cli_renders_agy_1510_telemetry_widgets_and_confirm_override() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = LayoutConfig::preset_minimal();
    cfg.rows = vec![vec![
        WidgetId::State,
        WidgetId::Vim,
        WidgetId::Queue,
        WidgetId::Battle,
        WidgetId::Sandbox,
    ]];
    cfg.theme = ThemeStyle::Default;
    cfg.glyph_mode = GlyphMode::Unicode;
    cfg.smart_hide = true;
    cfg.responsive = false;
    save_layout_config(&dir.path().join("config/agy-statusline/config.json"), &cfg).unwrap();

    let payload = serde_json::json!({
        "agent_state": "idle",
        "tool_confirmation_pending": true,
        "pending_input_count": 2,
        "vim": { "mode": "insert" },
        "battle": { "status": "active", "focused_arm": 1 },
        "sandbox": { "enabled": true, "allow_network": true },
        "terminal_width": 120
    })
    .to_string();

    let stdout = run_statusline_stdin(dir.path(), &[], &payload);
    assert_eq!(
        stdout,
        "◎ CONFIRM · vim INSERT · queue 2 · battle ACTIVE #1 · sandbox ON+net\n"
    );

    let preset_stdout = run_statusline_stdin(dir.path(), &["--preset", "minimal"], &payload);
    assert!(
        preset_stdout.contains("◎ CONFIRM"),
        "preset stdout: {preset_stdout}"
    );

    let doctor_out = isolated_agy_cmd(dir.path())
        .arg("doctor")
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    assert!(doctor_out.status.success());
    let doctor_stdout = String::from_utf8(doctor_out.stdout).unwrap();
    assert!(
        doctor_stdout.contains("vim NORMAL · queue 1"),
        "doctor stdout: {doctor_stdout}"
    );
}

#[test]
fn version_widget_renders_payload_version_and_handles_missing_or_prefixed() {
    let empty_meta = SessionMetadata::default();
    let payload: StatuslinePayload =
        serde_json::from_str(r#"{"agent_state":"idle","version":"1.5.11"}"#).unwrap();
    assert_eq!(
        render_single(
            WidgetId::Version,
            &payload,
            &empty_meta,
            GlyphMode::Unicode,
            ThemeStyle::Default,
            true,
            true
        )
        .trim(),
        "v1.5.11"
    );

    let payload_prefixed: StatuslinePayload =
        serde_json::from_str(r#"{"agent_state":"idle","version":" v1.5.11 "}"#).unwrap();
    assert_eq!(
        render_single(
            WidgetId::Version,
            &payload_prefixed,
            &empty_meta,
            GlyphMode::Unicode,
            ThemeStyle::Default,
            true,
            true
        )
        .trim(),
        "v1.5.11"
    );

    let payload_empty: StatuslinePayload =
        serde_json::from_str(r#"{"agent_state":"idle","version":"  "}"#).unwrap();
    assert_eq!(
        render_single(
            WidgetId::Version,
            &payload_empty,
            &empty_meta,
            GlyphMode::Unicode,
            ThemeStyle::Default,
            false,
            true
        )
        .trim(),
        ""
    );
}

#[test]
fn agy_payload_widgets_render_and_omit_when_absent() {
    let empty_meta = SessionMetadata::default();
    let payload: StatuslinePayload = serde_json::from_str(
        r#"{
            "session_id": "8d0371e1-5a05-423b-a8bc-ed75995f9a8b",
            "conversation_title": "Refactor\n  statusline\tpayload   widgets for long title test",
            "transcript_path": "/home/user/.gemini/logs/transcript.jsonl",
            "product": "Antigravity",
            "plan_tier": "Ultra",
            "email": "alice@example.com",
            "agent": {"name": "researcher"},
            "vcs": {"type": "citc", "client": "my-citc-ws", "dirty": true},
            "subagents": [
                {"name": "sub-a", "role": "Codebase Researcher", "status": "running"},
                {"name": "sub-b", "role": "Done Worker", "status": "completed"}
            ]
        }"#,
    )
    .unwrap();

    let r = |w, p, sh| {
        render_single(w, p, &empty_meta, GlyphMode::Ascii, ThemeStyle::Default, sh, true)
            .trim()
            .to_string()
    };
    assert_eq!(r(WidgetId::Plan, &payload, true), "Ultra");
    assert_eq!(r(WidgetId::Account, &payload, true), "alice@example.com");
    assert_eq!(
        r(WidgetId::Title, &payload, true),
        "Refactor statusline payload wi.."
    );
    assert_eq!(r(WidgetId::Agent, &payload, true), "agent researcher");
    assert_eq!(r(WidgetId::Product, &payload, true), "Antigravity");
    assert_eq!(r(WidgetId::Session, &payload, true), "sid:8d0371e1");
    assert_eq!(
        r(WidgetId::Transcript, &payload, true),
        "log:transcript.jsonl"
    );
    assert_eq!(r(WidgetId::Vcs, &payload, true), "citc: my-citc-ws*");
    assert_eq!(
        r(WidgetId::Subagents, &payload, true),
        "sub 1 (Codebase Researcher)"
    );

    let blank = StatuslinePayload::default();
    assert_eq!(r(WidgetId::Plan, &blank, false), "");
    assert_eq!(r(WidgetId::Account, &blank, false), "");
    assert_eq!(r(WidgetId::Title, &blank, false), "");
    assert_eq!(r(WidgetId::Agent, &blank, false), "");
    assert_eq!(r(WidgetId::Product, &blank, false), "");
    assert_eq!(r(WidgetId::Session, &blank, false), "");
    assert_eq!(r(WidgetId::Transcript, &blank, false), "");
}

#[test]
fn responsive_compaction_shortens_subagents_account_and_title() {
    let payload: StatuslinePayload = serde_json::from_str(
        r#"{
            "agent_state": "idle",
            "conversation_title": "Implement full AGY statusline payload widgets",
            "email": "alice@example.com",
            "subagents": [{"role": "Reviewer (Rust)", "status": "running"}],
            "terminal_width": 52
        }"#,
    )
    .unwrap();

    let mut cfg = LayoutConfig::preset_minimal();
    cfg.rows = vec![vec![
        WidgetId::State,
        WidgetId::Title,
        WidgetId::Account,
        WidgetId::Subagents,
    ]];
    cfg.glyph_mode = GlyphMode::Ascii;
    cfg.no_color = true;
    cfg.responsive = true;

    let rendered = render_mono(&payload, &SessionMetadata::default(), &GitInfo::default(), &cfg);
    assert!(rendered.contains("READY"), "rendered: {rendered}");
    assert!(rendered.contains("Implement full.."), "rendered: {rendered}");
    assert!(rendered.contains("alice"), "rendered: {rendered}");
    assert!(!rendered.contains("@example.com"), "rendered: {rendered}");
    assert!(rendered.contains("sub 1"), "rendered: {rendered}");
    assert!(!rendered.contains("Reviewer"), "rendered: {rendered}");
    assert!(!rendered.contains('('), "unclosed paren in: {rendered}");
}
