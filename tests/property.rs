#![forbid(unsafe_code)]
//! Property-based (`proptest`) invariant tests for `agy-statusline`.

mod common;

use std::collections::BTreeMap;

use proptest::prelude::*;

use agy_statusline::db::SessionMetadata;
use agy_statusline::git::GitInfo;
use agy_statusline::layout::{GlyphMode, LayoutConfig, ThemeStyle, VisualStyle, WidgetId};
use agy_statusline::protobuf::{
    extract_gen_metadata_record, extract_metadata_speed, extract_step_created_at_secs,
    MAX_TIMESTAMP_SECS, MAX_TTFT_SECS, MAX_TURN_SPEED_TPS, MIN_TIMESTAMP_SECS,
};
use agy_statusline::render::{build_progress_bar, write_statusline};
use agy_statusline::statusline::{
    ContextWindow, CostInfo, JsonUsage, ModelInfo, QuotaBucket, StatuslinePayload, WorkspaceInfo,
};

use common::{build_gen_blob, build_step_meta_blob, visible_line_width, TestGenSpec};

proptest! {
    #[test]
    fn prop_structured_protobuf_roundtrip_and_bounds(
        step_idx in 0u64..10_000,
        model_enum in 0u64..10_000,
        input_tok in 0u64..5_000_000,
        cache_read_tok in 0u64..5_000_000,
        output_tok in 1u64..100_000,
        thinking_tok in 0u64..100_000,
        dur_secs in 11u64..3_600,
        ttft_ms in 1u64..599_999,
        step_secs in 1u64..=(MAX_TIMESTAMP_SECS as u64),
    ) {
        let gen_blob = build_gen_blob(TestGenSpec {
            step_idx,
            model_enum,
            resp_model: "model-prop",
            disp_name: "Model Prop",
            input_tok,
            cache_read_tok,
            output_tok,
            thinking_tok,
            dur_secs,
            ttft_ms,
            ..Default::default()
        });
        let rec = extract_gen_metadata_record(&gen_blob).unwrap();
        prop_assert_eq!(rec.first_step_index, Some(step_idx));
        prop_assert_eq!(rec.model_enum, model_enum);
        prop_assert_eq!(rec.input_tokens, input_tok);
        prop_assert_eq!(rec.cache_read_tokens, cache_read_tok);
        prop_assert_eq!(rec.output_tokens, output_tok);
        prop_assert_eq!(rec.thinking_output_tokens, thinking_tok);
        prop_assert_eq!(rec.response_model.as_deref(), Some("model-prop"));
        prop_assert_eq!(rec.model_display_name.as_deref(), Some("Model Prop"));

        let decode_secs = rec.decode_secs.unwrap();
        prop_assert!((decode_secs - dur_secs as f64).abs() < 1e-9);
        let ttft_secs = rec.ttft_secs.unwrap();
        prop_assert!(ttft_secs > 0.0 && ttft_secs < MAX_TTFT_SECS);
        prop_assert!((ttft_secs - (ttft_ms as f64) / 1_000.0).abs() < 1e-6);

        let expected_tps = (output_tok as f64) / (dur_secs as f64);
        let speed = extract_metadata_speed(&gen_blob).unwrap();
        prop_assert!(speed > 0.0 && speed < MAX_TURN_SPEED_TPS);
        prop_assert!((speed - expected_tps).abs() < 1e-9);
        prop_assert_eq!(rec.decode_speed_tps(), Some(speed));

        let step_blob = build_step_meta_blob(step_secs);
        prop_assert_eq!(
            extract_step_created_at_secs(&step_blob),
            Some(step_secs as i64)
        );
    }

    #[test]
    fn prop_extract_gen_metadata_record_never_panics_on_arbitrary_bytes(
        data in prop::collection::vec(any::<u8>(), 0..256),
    ) {
        let rec = extract_gen_metadata_record(&data);
        let valid = rec.as_ref().is_none_or(|r| {
            r.decode_secs.is_none_or(|s| s.is_finite() && s > 0.0)
                && r.ttft_secs.is_none_or(|t| t > 0.0 && t < MAX_TTFT_SECS)
                && r.model_cost_usd.is_none_or(|c| c.is_finite() && c > 0.0)
                && r.chat_start_created_at_secs
                    .is_none_or(|ts| (MIN_TIMESTAMP_SECS..=MAX_TIMESTAMP_SECS).contains(&ts))
        });
        prop_assert!(valid);
    }

    #[test]
    fn prop_progress_bar_clamps_and_matches_char_len(
        pct in any::<f64>(),
        bar_len in 0usize..100,
    ) {
        let (bar, clamped) = build_progress_bar(pct, bar_len);
        prop_assert!(clamped.is_finite() && (0.0..=100.0).contains(&clamped));
        prop_assert_eq!(bar.chars().count(), bar_len.clamp(4, 40));
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
