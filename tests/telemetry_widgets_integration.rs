#![forbid(unsafe_code)]

use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

use agy_statusline::db::SessionMetadata;
use agy_statusline::editor::TuiEditorState;
use agy_statusline::git::GitInfo;
use agy_statusline::layout::{GlyphMode, LayoutConfig, ThemeStyle, WidgetId};
use agy_statusline::render::write_statusline;
use agy_statusline::statusline::{BattleInfo, SandboxInfo, StatuslinePayload, VimInfo};

fn render_single(
    widget: WidgetId,
    payload: &StatuslinePayload,
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
    write_statusline(
        payload,
        &SessionMetadata::default(),
        &GitInfo::default(),
        &cfg,
        &mut out,
    )
    .unwrap();
    String::from_utf8(out).unwrap()
}

#[test]
fn telemetry_widgets_catalog_metadata_and_presets_unchanged() {
    assert_eq!(WidgetId::ALL.len(), 33);
    assert_eq!(WidgetId::ALL[WidgetId::ALL.len() - 1], WidgetId::Spacer);

    assert_eq!(WidgetId::Vim.label(), "vim");
    assert_eq!(WidgetId::Vim.display_name(), "Vim Mode");
    assert_eq!(WidgetId::Vim.priority(), 2);

    assert_eq!(WidgetId::Queue.label(), "queue");
    assert_eq!(WidgetId::Queue.display_name(), "Queued Inputs");
    assert_eq!(WidgetId::Queue.priority(), 2);

    assert_eq!(WidgetId::Battle.label(), "battle");
    assert_eq!(WidgetId::Battle.display_name(), "Battle Mode");
    assert_eq!(WidgetId::Battle.priority(), 1);

    let vim_json = serde_json::to_string(&WidgetId::Vim).unwrap();
    assert_eq!(vim_json, "\"vim\"");
    assert_eq!(
        serde_json::from_str::<WidgetId>(&vim_json).unwrap(),
        WidgetId::Vim
    );

    let queue_json = serde_json::to_string(&WidgetId::Queue).unwrap();
    assert_eq!(queue_json, "\"queue\"");
    assert_eq!(
        serde_json::from_str::<WidgetId>(&queue_json).unwrap(),
        WidgetId::Queue
    );

    let battle_json = serde_json::to_string(&WidgetId::Battle).unwrap();
    assert_eq!(battle_json, "\"battle\"");
    assert_eq!(
        serde_json::from_str::<WidgetId>(&battle_json).unwrap(),
        WidgetId::Battle
    );

    assert_eq!(
        LayoutConfig::preset_full().matching_preset(),
        Some(agy_statusline::layout::Preset::Full)
    );
    assert_eq!(
        LayoutConfig::preset_powerline().matching_preset(),
        Some(agy_statusline::layout::Preset::Powerline)
    );
    assert_eq!(
        LayoutConfig::preset_minimal().matching_preset(),
        Some(agy_statusline::layout::Preset::Minimal)
    );
    assert_eq!(
        LayoutConfig::preset_addon().matching_preset(),
        Some(agy_statusline::layout::Preset::Addon)
    );

    let vim_idx = WidgetId::ALL.iter().position(|&w| w == WidgetId::Vim).unwrap();
    let queue_idx = WidgetId::ALL
        .iter()
        .position(|&w| w == WidgetId::Queue)
        .unwrap();
    let battle_idx = WidgetId::ALL
        .iter()
        .position(|&w| w == WidgetId::Battle)
        .unwrap();

    let mut editor = TuiEditorState::new(LayoutConfig::preset_minimal());
    editor.click_catalog(vim_idx);
    editor.click_catalog(queue_idx);
    editor.click_catalog(battle_idx);
    assert!(editor.config.rows[0].contains(&WidgetId::Vim));
    assert!(editor.config.rows[0].contains(&WidgetId::Queue));
    assert!(editor.config.rows[0].contains(&WidgetId::Battle));
    editor.click_catalog(battle_idx);
    assert!(!editor.config.rows[0].contains(&WidgetId::Battle));
}

#[test]
fn state_widget_renders_confirm_alert_on_tool_confirmation_pending() {
    let pending = StatuslinePayload {
        agent_state: Some("idle".to_string()),
        tool_confirmation_pending: Some(true),
        ..Default::default()
    };
    assert_eq!(
        render_single(
            WidgetId::State,
            &pending,
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
    let insert = StatuslinePayload {
        vim: Some(VimInfo {
            mode: Some("insert".to_string()),
        }),
        ..Default::default()
    };
    assert_eq!(
        render_single(
            WidgetId::Vim,
            &insert,
            GlyphMode::Unicode,
            ThemeStyle::Default,
            true,
            false
        ),
        "\x1b[1m\x1b[38;5;78mvim INSERT\x1b[0m\n"
    );

    let visual = StatuslinePayload {
        vim: Some(VimInfo {
            mode: Some("visual".to_string()),
        }),
        ..Default::default()
    };
    assert_eq!(
        render_single(
            WidgetId::Vim,
            &visual,
            GlyphMode::Unicode,
            ThemeStyle::Default,
            true,
            false
        ),
        "\x1b[1m\x1b[38;5;183mvim VISUAL\x1b[0m\n"
    );

    let vline = StatuslinePayload {
        vim: Some(VimInfo {
            mode: Some("v-line".to_string()),
        }),
        ..Default::default()
    };
    assert_eq!(
        render_single(
            WidgetId::Vim,
            &vline,
            GlyphMode::Unicode,
            ThemeStyle::Default,
            true,
            false
        ),
        "\x1b[1m\x1b[38;5;183mvim V-LINE\x1b[0m\n"
    );

    let vblock = StatuslinePayload {
        vim: Some(VimInfo {
            mode: Some("v-block".to_string()),
        }),
        ..Default::default()
    };
    assert_eq!(
        render_single(
            WidgetId::Vim,
            &vblock,
            GlyphMode::Unicode,
            ThemeStyle::Default,
            true,
            false
        ),
        "\x1b[1m\x1b[38;5;183mvim V-BLOCK\x1b[0m\n"
    );

    let command = StatuslinePayload {
        vim: Some(VimInfo {
            mode: Some("command".to_string()),
        }),
        ..Default::default()
    };
    assert_eq!(
        render_single(
            WidgetId::Vim,
            &command,
            GlyphMode::Unicode,
            ThemeStyle::Default,
            true,
            false
        ),
        "\x1b[1m\x1b[38;5;220mvim COMMAND\x1b[0m\n"
    );

    let replace = StatuslinePayload {
        vim: Some(VimInfo {
            mode: Some("replace".to_string()),
        }),
        ..Default::default()
    };
    assert_eq!(
        render_single(
            WidgetId::Vim,
            &replace,
            GlyphMode::Unicode,
            ThemeStyle::Default,
            true,
            false
        ),
        "\x1b[1m\x1b[38;5;220mvim REPLACE\x1b[0m\n"
    );

    let normal = StatuslinePayload {
        vim: Some(VimInfo {
            mode: Some(" normal ".to_string()),
        }),
        ..Default::default()
    };
    assert_eq!(
        render_single(
            WidgetId::Vim,
            &normal,
            GlyphMode::Unicode,
            ThemeStyle::Default,
            true,
            false
        ),
        "\x1b[1m\x1b[38;5;117mvim NORMAL\x1b[0m\n"
    );

    let empty = StatuslinePayload {
        vim: Some(VimInfo {
            mode: Some("   ".to_string()),
        }),
        ..Default::default()
    };
    assert_eq!(
        render_single(
            WidgetId::Vim,
            &empty,
            GlyphMode::Unicode,
            ThemeStyle::Default,
            true,
            false
        ),
        ""
    );
    assert_eq!(
        render_single(
            WidgetId::Vim,
            &StatuslinePayload::default(),
            GlyphMode::Unicode,
            ThemeStyle::Default,
            false,
            false
        ),
        "\x1b[38;5;245mvim off\x1b[0m\n"
    );
}

#[test]
fn queue_widget_renders_count_and_smart_hide() {
    let active = StatuslinePayload {
        pending_input_count: Some(3),
        ..Default::default()
    };
    assert_eq!(
        render_single(
            WidgetId::Queue,
            &active,
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
    let dir = std::env::temp_dir().join(format!("agy_telemetry_tier2_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let xdg_cfg = dir.join("cfg");
    let cfg_dir = xdg_cfg.join("agy-statusline");
    fs::create_dir_all(&cfg_dir).unwrap();

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
    fs::write(
        cfg_dir.join("config.json"),
        serde_json::to_string(&cfg).unwrap(),
    )
    .unwrap();

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
        .write_all(payload.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert!(output.status.success());
    assert_eq!(
        stdout,
        "◎ CONFIRM · vim INSERT · queue 2 · battle ACTIVE #1 · sandbox ON+net\n"
    );

    let mut preset_child = Command::new(env!("CARGO_BIN_EXE_agy-statusline"))
        .arg("--preset")
        .arg("minimal")
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
    preset_child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    let preset_output = preset_child.wait_with_output().unwrap();
    let preset_stdout = String::from_utf8(preset_output.stdout).unwrap();

    let doctor_out = Command::new(env!("CARGO_BIN_EXE_agy-statusline"))
        .arg("doctor")
        .env("HOME", &dir)
        .env("USERPROFILE", &dir)
        .env("XDG_CONFIG_HOME", &xdg_cfg)
        .env("XDG_CACHE_HOME", dir.join("cache"))
        .env_remove("ANTIGRAVITY_CLI_HOME")
        .env_remove("AGY_HOME")
        .env_remove("AGY_STATUSLINE_FULL")
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    let doctor_stdout = String::from_utf8(doctor_out.stdout).unwrap();

    let _ = fs::remove_dir_all(&dir);

    assert!(preset_output.status.success());
    assert!(
        preset_stdout.contains("◎ CONFIRM"),
        "preset stdout: {preset_stdout}"
    );
    assert!(doctor_out.status.success());
    assert!(
        doctor_stdout.contains("vim NORMAL · queue 1"),
        "doctor stdout: {doctor_stdout}"
    );
}

#[test]
fn version_widget_renders_payload_version_and_handles_missing_or_prefixed() {
    let payload: StatuslinePayload =
        serde_json::from_str(r#"{"agent_state":"idle","version":"1.5.11"}"#).unwrap();
    let plain = render_single(
        WidgetId::Version,
        &payload,
        GlyphMode::Unicode,
        ThemeStyle::Default,
        true,
        true,
    );
    assert_eq!(plain.trim(), "v1.5.11");

    let payload_prefixed: StatuslinePayload =
        serde_json::from_str(r#"{"agent_state":"idle","version":" v1.5.11 "}"#).unwrap();
    let prefixed = render_single(
        WidgetId::Version,
        &payload_prefixed,
        GlyphMode::Unicode,
        ThemeStyle::Default,
        true,
        true,
    );
    assert_eq!(prefixed.trim(), "v1.5.11");

    let payload_empty: StatuslinePayload =
        serde_json::from_str(r#"{"agent_state":"idle","version":"  "}"#).unwrap();
    let empty = render_single(
        WidgetId::Version,
        &payload_empty,
        GlyphMode::Unicode,
        ThemeStyle::Default,
        false,
        true,
    );
    assert_eq!(empty.trim(), "");
}

#[test]
fn agy_payload_widgets_render_and_omit_when_absent() {
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

    assert_eq!(
        render_single(WidgetId::Plan, &payload, GlyphMode::Ascii, ThemeStyle::Default, true, true).trim(),
        "Ultra"
    );
    assert_eq!(
        render_single(WidgetId::Account, &payload, GlyphMode::Ascii, ThemeStyle::Default, true, true).trim(),
        "alice@example.com"
    );
    let title = render_single(WidgetId::Title, &payload, GlyphMode::Ascii, ThemeStyle::Default, true, true);
    assert_eq!(title.trim(), "Refactor statusline payload wi..");
    assert_eq!(
        render_single(WidgetId::Agent, &payload, GlyphMode::Ascii, ThemeStyle::Default, true, true).trim(),
        "agent researcher"
    );
    assert_eq!(
        render_single(WidgetId::Product, &payload, GlyphMode::Ascii, ThemeStyle::Default, true, true).trim(),
        "Antigravity"
    );
    assert_eq!(
        render_single(WidgetId::Session, &payload, GlyphMode::Ascii, ThemeStyle::Default, true, true).trim(),
        "sid:8d0371e1"
    );
    assert_eq!(
        render_single(WidgetId::Transcript, &payload, GlyphMode::Ascii, ThemeStyle::Default, true, true).trim(),
        "log:transcript.jsonl"
    );
    assert_eq!(
        render_single(WidgetId::Vcs, &payload, GlyphMode::Ascii, ThemeStyle::Default, true, true).trim(),
        "citc: my-citc-ws*"
    );
    assert_eq!(
        render_single(WidgetId::Subagents, &payload, GlyphMode::Ascii, ThemeStyle::Default, true, true).trim(),
        "sub 1 (Codebase Researcher)"
    );

    let blank = StatuslinePayload::default();
    assert_eq!(render_single(WidgetId::Plan, &blank, GlyphMode::Ascii, ThemeStyle::Default, false, true).trim(), "");
    assert_eq!(render_single(WidgetId::Account, &blank, GlyphMode::Ascii, ThemeStyle::Default, false, true).trim(), "");
    assert_eq!(render_single(WidgetId::Title, &blank, GlyphMode::Ascii, ThemeStyle::Default, false, true).trim(), "");
    assert_eq!(render_single(WidgetId::Agent, &blank, GlyphMode::Ascii, ThemeStyle::Default, false, true).trim(), "");
    assert_eq!(render_single(WidgetId::Product, &blank, GlyphMode::Ascii, ThemeStyle::Default, false, true).trim(), "");
    assert_eq!(render_single(WidgetId::Session, &blank, GlyphMode::Ascii, ThemeStyle::Default, false, true).trim(), "");
    assert_eq!(render_single(WidgetId::Transcript, &blank, GlyphMode::Ascii, ThemeStyle::Default, false, true).trim(), "");
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

    let mut out = Vec::new();
    write_statusline(
        &payload,
        &SessionMetadata::default(),
        &GitInfo::default(),
        &cfg,
        &mut out,
    )
    .unwrap();
    let rendered = String::from_utf8(out).unwrap();
    assert!(rendered.contains("READY"), "rendered: {rendered}");
    assert!(rendered.contains("Implement full.."), "rendered: {rendered}");
    assert!(rendered.contains("alice"), "rendered: {rendered}");
    assert!(!rendered.contains("@example.com"), "rendered: {rendered}");
    assert!(rendered.contains("sub 1"), "rendered: {rendered}");
    assert!(!rendered.contains("Reviewer"), "rendered: {rendered}");
    assert!(!rendered.contains('('), "unclosed paren in: {rendered}");
}
