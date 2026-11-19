#![forbid(unsafe_code)]

use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

use rusqlite::Connection;

use agy_statusline::db::SessionMetadata;
use agy_statusline::editor::TuiEditorState;
use agy_statusline::git::GitInfo;
use agy_statusline::layout::{GlyphMode, LayoutConfig, ThemeStyle, WidgetId};
use agy_statusline::render::write_statusline;
use agy_statusline::statusline::{ModelInfo, StatuslinePayload};

fn render_thinking(
    thinking_tokens: Option<u64>,
    glyph_mode: GlyphMode,
    theme: ThemeStyle,
    smart_hide: bool,
    no_color: bool,
) -> String {
    let mut cfg = LayoutConfig::preset_minimal();
    cfg.rows = vec![vec![WidgetId::Thinking]];
    cfg.glyph_mode = glyph_mode;
    cfg.theme = theme;
    cfg.smart_hide = smart_hide;
    cfg.no_color = no_color;
    cfg.responsive = false;

    let meta = SessionMetadata {
        thinking_tokens,
        ..Default::default()
    };
    let mut out = Vec::new();
    write_statusline(
        &StatuslinePayload::default(),
        &meta,
        &GitInfo::default(),
        &cfg,
        &mut out,
    )
    .unwrap();
    String::from_utf8(out).unwrap()
}

#[test]
fn thinking_widget_catalog_metadata_and_presets_unchanged() {
    assert_eq!(WidgetId::ALL.len(), 33);
    assert_eq!(WidgetId::Thinking.label(), "thinking");
    assert_eq!(WidgetId::Thinking.display_name(), "Thinking Tokens");
    assert_eq!(WidgetId::Thinking.priority(), 1);
    assert_eq!(WidgetId::ALL[WidgetId::ALL.len() - 1], WidgetId::Spacer);

    let json = serde_json::to_string(&WidgetId::Thinking).unwrap();
    assert_eq!(json, "\"thinking\"");
    let parsed: WidgetId = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, WidgetId::Thinking);

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

    let thinking_idx = WidgetId::ALL
        .iter()
        .position(|&w| w == WidgetId::Thinking)
        .unwrap();
    let mut editor = TuiEditorState::new(LayoutConfig::preset_minimal());
    editor.click_catalog(thinking_idx);
    assert!(editor.config.rows[0].contains(&WidgetId::Thinking));
    editor.click_catalog(thinking_idx);
    assert!(!editor.config.rows[0].contains(&WidgetId::Thinking));
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
    let mut out = Vec::new();
    write_statusline(&payload, &meta, &GitInfo::default(), &cfg, &mut out).unwrap();
    let rendered = String::from_utf8(out).unwrap();
    assert!(rendered.contains("READY"), "rendered: {rendered}");
    assert!(rendered.contains("GPT-4"), "rendered: {rendered}");
    assert!(!rendered.contains("think"), "rendered: {rendered}");
}

#[test]
fn tier2_cli_reads_real_sqlite_gen_metadata_thinking_tokens() {
    let dir = std::env::temp_dir().join(format!("agy_thinking_tier2_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let xdg_cfg = dir.join("cfg");
    let cfg_dir = xdg_cfg.join("agy-statusline");
    let conv_dir = dir.join(".gemini/antigravity-cli/conversations");
    fs::create_dir_all(&cfg_dir).unwrap();
    fs::create_dir_all(&conv_dir).unwrap();

    let mut cfg = LayoutConfig::preset_minimal();
    cfg.rows = vec![vec![WidgetId::State, WidgetId::Thinking]];
    cfg.theme = ThemeStyle::Default;
    cfg.glyph_mode = GlyphMode::Unicode;
    cfg.smart_hide = true;
    cfg.responsive = false;
    fs::write(
        cfg_dir.join("config.json"),
        serde_json::to_string(&cfg).unwrap(),
    )
    .unwrap();

    let uuid = "11223344-5566-7788-99aa-bbccddeeff00";
    let db_path = conv_dir.join(format!("{uuid}.db"));
    let conn = Connection::open(&db_path).unwrap();
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
        .write_all(stdin_json.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert!(output.status.success());
    assert!(stdout.contains("◆ think 4.2K"), "stdout was: {stdout}");

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

    assert!(doctor_out.status.success());
    assert!(
        doctor_stdout.contains("◆ think 4.2K"),
        "doctor stdout was: {doctor_stdout}"
    );
}
