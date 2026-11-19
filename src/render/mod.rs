//! Multi-style, theme-aware statusline renderer (`Plain`, `Pill`, `Powerline`).

mod emit;
mod fit;
mod widgets;

use std::io::{self, Write as IoWrite};

use crate::db::SessionMetadata;
use crate::git::GitInfo;
use crate::layout::{LayoutConfig, SpeedMode, WidgetId};
use crate::metrics::{
    current_context_tokens, resolve_quota_views_with_reset_at, valid_cache_usage, QuotaView,
};
use crate::statusline::StatuslinePayload;
use crate::theme::{glyphs_for, palette_for, GlyphSet, Palette};
use crate::timeutil::unix_now_secs;

pub(crate) use fit::truncate_to_width;

use emit::{compute_geometry, StyleGeometry};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompactStage {
    VcsDropDiff,
    VcsTruncate,
    QuotaStripTrailers,
    CtxDropLimit,
    CtxCollapseBar,
    ModelAbbreviate,
    ModelTruncate,
    SubagentsDropLabel,
    AccountDropDomain,
    TitleTruncate,
}

#[derive(Debug, Clone)]
struct Segment {
    widget: WidgetId,
    text: String,
    fg: u8,
    accent_bg: Option<u8>,
    bold: bool,
    is_spacer: bool,
    fallbacks: Vec<(CompactStage, String)>,
}

impl Segment {
    fn plain(widget: WidgetId, text: String, fg: u8, bold: bool) -> Self {
        Self {
            widget,
            text,
            fg,
            accent_bg: None,
            bold,
            is_spacer: false,
            fallbacks: Vec::new(),
        }
    }

    fn accent(widget: WidgetId, text: String, fg: u8) -> Self {
        Self {
            widget,
            text,
            fg,
            accent_bg: Some(fg),
            bold: true,
            is_spacer: false,
            fallbacks: Vec::new(),
        }
    }

    fn spacer() -> Self {
        Self {
            widget: WidgetId::Spacer,
            text: String::new(),
            fg: 0,
            accent_bg: None,
            bold: false,
            is_spacer: true,
            fallbacks: Vec::new(),
        }
    }

    fn push_fallback(&mut self, stage: CompactStage, text: String) {
        self.fallbacks.push((stage, text));
    }

    fn apply_compaction(&mut self, stage: CompactStage) {
        if let Some((_, replacement)) = self.fallbacks.iter().find(|(s, _)| *s == stage) {
            self.text.clone_from(replacement);
        }
    }
}

struct Renderer<'a> {
    cfg: &'a LayoutConfig,
    pal: &'a Palette,
    glyphs: &'a GlyphSet,
    geom: StyleGeometry,
}

impl<'a> Renderer<'a> {
    fn new(cfg: &'a LayoutConfig) -> Self {
        let pal = palette_for(cfg.theme);
        let glyphs = glyphs_for(cfg.glyph_mode);
        let geom = compute_geometry(cfg, glyphs);
        Self {
            cfg,
            pal,
            glyphs,
            geom,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum CostCoverage {
    Session,
    Partial(u64),
    LatestTurn,
}

struct RenderContext<'a> {
    cache: Option<(f64, u64)>,
    used_pct: f64,
    cur_ctx: Option<u64>,
    eff_limit: Option<u64>,
    speed: Option<f64>,
    ttft_secs: Option<f64>,
    thinking_tokens: Option<u64>,
    estimated_cost_usd: Option<f64>,
    estimated_subagent_cost_usd: Option<f64>,
    cost_coverage: CostCoverage,
    quota_5h: Option<QuotaView>,
    quota_weekly: Option<QuotaView>,
    git: &'a GitInfo,
}

impl<'a> RenderContext<'a> {
    fn build(
        payload: &StatuslinePayload,
        meta: &SessionMetadata,
        git: &'a GitInfo,
        (quota_5h, quota_weekly): (Option<QuotaView>, Option<QuotaView>),
        cost_coverage: CostCoverage,
        cfg: &LayoutConfig,
    ) -> Self {
        let cache = valid_cache_usage(payload);
        let cur_ctx = current_context_tokens(payload)
            .or_else(|| meta.estimated_context_tokens.filter(|&v| v > 0));
        let eff_limit = meta.max_context_tokens.filter(|&v| v > 0).or_else(|| {
            payload
                .context_window
                .as_ref()
                .and_then(|c| c.context_window_size)
                .filter(|&v| v > 0)
        });
        let used_pct = match (cur_ctx, eff_limit) {
            (Some(cur), Some(lim)) if lim > 0 => ((cur as f64) * 100.0 / (lim as f64)).max(0.0),
            _ => payload
                .context_window
                .as_ref()
                .and_then(|c| c.used_percentage)
                .unwrap_or(0.0),
        };
        let speed = match cfg.speed_mode {
            SpeedMode::Weighted => meta.speed,
            SpeedMode::Median => meta.median_speed.or(meta.speed),
            SpeedMode::Instant => meta.last_turn_speed.or(meta.speed),
            SpeedMode::E2e => meta.e2e_speed.or(meta.speed),
        };
        Self {
            cache,
            used_pct,
            cur_ctx,
            eff_limit,
            speed,
            ttft_secs: meta.ttft_secs,
            thinking_tokens: meta.thinking_tokens,
            estimated_cost_usd: meta.estimated_cost_usd,
            estimated_subagent_cost_usd: meta.estimated_subagent_cost_usd,
            cost_coverage,
            quota_5h,
            quota_weekly,
            git,
        }
    }
}

/// Builds a `bar_len`-segment Unicode context progress bar (`█▓▒░·`).
pub fn build_progress_bar(used_percentage: f64, bar_len: usize) -> (String, f64) {
    build_progress_bar_with_chars(used_percentage, bar_len, ['█', '▓', '▒', '░', '·'])
}

fn build_progress_bar_with_chars(
    used_percentage: f64,
    bar_len: usize,
    chars: [char; 5],
) -> (String, f64) {
    let clamped = if used_percentage.is_finite() {
        used_percentage.clamp(0.0, 100.0)
    } else {
        0.0
    };
    let pct_int = clamped.trunc() as usize;
    let len = bar_len.clamp(4, 40);
    let filled = (pct_int * len) / 100;
    let remainder = (pct_int * len) % 100;

    let mut bar = String::with_capacity(len * 3);
    for i in 0..len {
        let ch = if i < filled {
            chars[0]
        } else if i == filled && remainder >= 75 {
            chars[1]
        } else if i == filled && remainder >= 50 {
            chars[2]
        } else if i == filled && remainder >= 25 {
            chars[3]
        } else {
            chars[4]
        };
        bar.push(ch);
    }
    (bar, clamped)
}

/// Renders all non-empty configured rows in `cfg` for `payload`, `meta`, and `git`.
pub fn write_statusline<W: IoWrite>(
    payload: &StatuslinePayload,
    meta: &SessionMetadata,
    git: &GitInfo,
    cfg: &LayoutConfig,
    out: &mut W,
) -> io::Result<()> {
    let (quota_5h, quota_weekly) = resolve_quota_views_with_reset_at(payload, unix_now_secs());
    write_statusline_with_quotas(
        payload,
        meta,
        git,
        (quota_5h.map(|(v, _)| v), quota_weekly.map(|(v, _)| v)),
        CostCoverage::Session,
        cfg,
        out,
    )
}

/// Renders all non-empty configured rows using pre-resolved (e.g. calibrated) quota views.
pub(crate) fn write_statusline_with_quotas<W: IoWrite>(
    payload: &StatuslinePayload,
    meta: &SessionMetadata,
    git: &GitInfo,
    quotas: (Option<QuotaView>, Option<QuotaView>),
    cost_coverage: CostCoverage,
    cfg: &LayoutConfig,
    out: &mut W,
) -> io::Result<()> {
    let renderer = Renderer::new(cfg);
    let ctx = RenderContext::build(payload, meta, git, quotas, cost_coverage, cfg);
    let max_width = cfg.responsive.then_some(payload.terminal_width).flatten();

    for row in &cfg.rows {
        let segs: Vec<Segment> = row
            .iter()
            .filter_map(|&w| renderer.render_widget(w, payload, &ctx))
            .collect();
        let groups = fit::split_into_groups(segs);
        let fitted = fit::fit_row_groups(groups, max_width, renderer.geom, renderer.glyphs.ellipsis);
        renderer.write_row_groups(&fitted, payload.terminal_width, out)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use unicode_width::UnicodeWidthStr;

    use super::*;
    use crate::format::format_tokens_string as format_tok_str;
    use crate::layout::{GlyphMode, ThemeStyle, VisualStyle};
    use crate::statusline::{
        AgentInfo, BattleInfo, ContextWindow, CostInfo, JsonUsage, ModelInfo, QuotaBucket, VcsInfo,
        VimInfo, WorkspaceInfo,
    };

    #[test]
    fn renders_all_three_styles_and_drops_overflow_when_responsive() {
        let payload = StatuslinePayload {
            agent_state: Some("thinking".to_string()),
            model: Some(ModelInfo {
                id: None,
                display_name: Some("Gemini 4".to_string()),
                effort: Some("high".to_string()),
            }),
            workspace: Some(WorkspaceInfo {
                current_dir: Some("/tmp/agy-statusline".to_string()),
                project_dir: None,
            }),
            vcs: Some(VcsInfo {
                vcs_type: None,
                branch: Some("main".to_string()),
                client: None,
                dirty: Some(true),
            }),
            context_window: Some(ContextWindow {
                used_percentage: Some(42.5),
                current_usage: Some(JsonUsage {
                    input_tokens: Some(200),
                    output_tokens: Some(50),
                    cache_read_input_tokens: Some(800),
                    cache_creation_input_tokens: Some(0),
                }),
                ..Default::default()
            }),
            cost: Some(CostInfo {
                total_usd: Some(0.14),
                subagent_usd: None,
                estimated: Some(true),
            }),
            terminal_width: Some(40),
            ..Default::default()
        };

        let mut powerline_cfg = LayoutConfig::preset_powerline();
        powerline_cfg.responsive = false;
        let meta = SessionMetadata {
            speed: Some(64.0),
            last_turn_speed: Some(64.0),
            e2e_speed: Some(45.0),
            ttft_secs: Some(1.4),
            max_context_tokens: Some(512_000),
            ..Default::default()
        };
        let git = GitInfo {
            branch: None,
            worktree: Some("feat-wt".to_string()),
            added: 12,
            deleted: 3,
        };
        let mut pl_out = Vec::new();
        write_statusline(&payload, &meta, &git, &powerline_cfg, &mut pl_out).unwrap();
        let pl_text = String::from_utf8(pl_out).unwrap();
        assert!(pl_text.contains("\u{e0b0}"));
        assert!(pl_text.contains("Gemini 4 · high"));
        assert!(pl_text.contains("~$0.14"));
        assert!(pl_text.contains("(1K/512K)"));

        let mut pill_cfg = LayoutConfig::preset_full();
        pill_cfg.style = VisualStyle::Pill;
        pill_cfg.glyph_mode = GlyphMode::Nerd;
        pill_cfg.responsive = false;
        let mut pill_out = Vec::new();
        write_statusline(&payload, &meta, &git, &pill_cfg, &mut pill_out).unwrap();
        let pill_text = String::from_utf8(pill_out).unwrap();
        assert!(pill_text.contains("\u{e0b6}"));
        assert!(pill_text.contains("\u{e0b4}"));
        assert!(pill_text.contains("\x1b[48;5;236m"));
        assert!(pill_text.contains("THINKING"));
        assert!(pill_text.contains("~$0.14"));
        assert!(!pill_text.contains("\u{e0b0}"));

        let mut narrow_cfg = LayoutConfig::preset_full();
        narrow_cfg.theme = ThemeStyle::Mono;
        let mut narrow_out = Vec::new();
        write_statusline(&payload, &meta, &git, &narrow_cfg, &mut narrow_out).unwrap();
        let narrow_text = String::from_utf8(narrow_out).unwrap();
        assert!(narrow_text.contains("THINKING"));
        assert!(!narrow_text.contains("sandbox"));
        assert!(!narrow_text.contains("\u{e0b0}"));
        assert!(!narrow_text.contains("\u{e0b6}"));
    }

    #[test]
    fn recomputes_used_pct_from_sniffed_compaction_threshold() {
        let payload = StatuslinePayload {
            context_window: Some(ContextWindow {
                context_window_size: Some(2_000_000),
                used_percentage: Some(10.9),
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
        let meta = SessionMetadata {
            speed: Some(80.0),
            max_context_tokens: Some(512_000),
            ..Default::default()
        };
        let mut cfg = LayoutConfig::preset_minimal();
        cfg.theme = ThemeStyle::Mono;
        let mut out = Vec::new();
        write_statusline(&payload, &meta, &GitInfo::default(), &cfg, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("ctx 50.0% (512K)"));
    }

    #[test]
    fn spacer_aligns_left_center_right_and_smart_hide_hides_zeros() {
        let payload = StatuslinePayload {
            agent_state: Some("idle".to_string()),
            terminal_width: Some(40),
            ..Default::default()
        };
        let mut cfg = LayoutConfig::preset_minimal();
        cfg.theme = ThemeStyle::Mono;
        cfg.rows = vec![vec![
            WidgetId::State,
            WidgetId::Spacer,
            WidgetId::Tasks,
            WidgetId::Speed,
        ]];
        let mut out = Vec::new();
        write_statusline(
            &payload,
            &SessionMetadata::default(),
            &GitInfo::default(),
            &cfg,
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains("tasks 0"));
        assert!(text.starts_with("● READY"));
        assert!(text.ends_with("⚡ --\n"));
        assert!(text.contains("        "));

        // Three-group centering: Left, Center, Right.
        let mut cfg3 = LayoutConfig::preset_minimal();
        cfg3.theme = ThemeStyle::Mono;
        cfg3.rows = vec![vec![
            WidgetId::State,
            WidgetId::Spacer,
            WidgetId::Speed,
            WidgetId::Spacer,
            WidgetId::CacheHit,
        ]];
        let mut out3 = Vec::new();
        write_statusline(
            &payload,
            &SessionMetadata::default(),
            &GitInfo::default(),
            &cfg3,
            &mut out3,
        )
        .unwrap();
        let line3 = String::from_utf8(out3).unwrap();
        let trimmed = line3.trim_end_matches('\n');
        assert_eq!(trimmed.width(), 40);
        assert!(trimmed.starts_with("● READY"));
        assert!(trimmed.contains("⚡ --"));
        assert!(trimmed.ends_with("◈ cache --"));
    }

    #[test]
    fn renders_mode_quota_subagent_cost_completed_state_and_credit_bucket() {
        let mut quota = BTreeMap::new();
        quota.insert(
            "acme-5h".to_string(),
            QuotaBucket {
                remaining_fraction: Some(0.84),
                reset_in_seconds: Some(11_400),
                ..Default::default()
            },
        );
        quota.insert(
            "acme-weekly".to_string(),
            QuotaBucket {
                remaining_fraction: None,
                remaining_amount: Some(450),
                reset_in_seconds: Some(172_800),
                ..Default::default()
            },
        );
        let payload = StatuslinePayload {
            agent_state: Some("reviewing".to_string()),
            cycle_mode: Some("plan".to_string()),
            exceeds_200k_tokens: Some(true),
            cost: Some(CostInfo {
                total_usd: Some(0.45),
                subagent_usd: Some(0.12),
                estimated: Some(false),
            }),
            quota: Some(quota),
            ..Default::default()
        };
        let mut cfg = LayoutConfig::preset_minimal();
        cfg.glyph_mode = GlyphMode::Ascii;
        cfg.no_color = true;
        cfg.rows = vec![vec![
            WidgetId::State,
            WidgetId::Mode,
            WidgetId::Cost,
            WidgetId::Quota5h,
            WidgetId::QuotaWeekly,
        ]];
        let mut out = Vec::new();
        write_statusline(
            &payload,
            &SessionMetadata::default(),
            &GitInfo::default(),
            &cfg,
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains('\x1b'));
        assert_eq!(
            text,
            "@ REVIEW * mode: PLAN >200K * $0.45 (sub $0.12) * 5h 84% r:3h10m * 7d 450 r:2d\n"
        );

        let completed = StatuslinePayload {
            agent_state: Some("completed".to_string()),
            ..Default::default()
        };
        cfg.rows = vec![vec![WidgetId::State]];
        let mut out_done = Vec::new();
        write_statusline(
            &completed,
            &SessionMetadata::default(),
            &GitInfo::default(),
            &cfg,
            &mut out_done,
        )
        .unwrap();
        assert_eq!(String::from_utf8(out_done).unwrap(), "* DONE\n");
    }

    #[test]
    fn model_structural_effort_matching_works_for_arbitrary_tiers() {
        let mut cfg = LayoutConfig::preset_minimal();
        cfg.theme = ThemeStyle::Mono;
        cfg.rows = vec![vec![WidgetId::Model]];

        let model_xhigh = StatuslinePayload {
            model: Some(ModelInfo {
                id: None,
                display_name: Some("Model-X 120B (Extra High)".to_string()),
                effort: Some("xhigh".to_string()),
            }),
            ..Default::default()
        };
        let mut out1 = Vec::new();
        write_statusline(
            &model_xhigh,
            &SessionMetadata::default(),
            &GitInfo::default(),
            &cfg,
            &mut out1,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(out1).unwrap(),
            "Model-X 120B (Extra High)\n"
        );

        let model_m2 = StatuslinePayload {
            model: Some(ModelInfo {
                id: None,
                display_name: Some("VendorM M2".to_string()),
                effort: Some("max".to_string()),
            }),
            ..Default::default()
        };
        let mut out2 = Vec::new();
        write_statusline(
            &model_m2,
            &SessionMetadata::default(),
            &GitInfo::default(),
            &cfg,
            &mut out2,
        )
        .unwrap();
        assert_eq!(String::from_utf8(out2).unwrap(), "VendorM M2 · max\n");

        let model_high_vs_xhigh = StatuslinePayload {
            model: Some(ModelInfo {
                id: None,
                display_name: Some("Model-X 120B (Extra High)".to_string()),
                effort: Some("high".to_string()),
            }),
            ..Default::default()
        };
        let mut out3 = Vec::new();
        write_statusline(
            &model_high_vs_xhigh,
            &SessionMetadata::default(),
            &GitInfo::default(),
            &cfg,
            &mut out3,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(out3).unwrap(),
            "Model-X 120B (Extra High) · high\n"
        );

        let model_min = StatuslinePayload {
            model: Some(ModelInfo {
                id: None,
                display_name: Some("Acme Model Minimal".to_string()),
                effort: Some("minimal".to_string()),
            }),
            ..Default::default()
        };
        let mut out4 = Vec::new();
        write_statusline(
            &model_min,
            &SessionMetadata::default(),
            &GitInfo::default(),
            &cfg,
            &mut out4,
        )
        .unwrap();
        assert_eq!(String::from_utf8(out4).unwrap(), "Acme Model Minimal\n");

        let model_turbo = StatuslinePayload {
            model: Some(ModelInfo {
                id: None,
                display_name: Some("DeepSeek R2 (Turbo)".to_string()),
                effort: Some("turbo".to_string()),
            }),
            ..Default::default()
        };
        let mut out5 = Vec::new();
        write_statusline(
            &model_turbo,
            &SessionMetadata::default(),
            &GitInfo::default(),
            &cfg,
            &mut out5,
        )
        .unwrap();
        assert_eq!(String::from_utf8(out5).unwrap(), "DeepSeek R2 (Turbo)\n");

        let model_ultra = StatuslinePayload {
            model: Some(ModelInfo {
                id: None,
                display_name: Some("Model-Z - Ultra".to_string()),
                effort: Some("ultra".to_string()),
            }),
            ..Default::default()
        };
        let mut out6 = Vec::new();
        write_statusline(
            &model_ultra,
            &SessionMetadata::default(),
            &GitInfo::default(),
            &cfg,
            &mut out6,
        )
        .unwrap();
        assert_eq!(String::from_utf8(out6).unwrap(), "Model-Z - Ultra\n");
    }

    #[test]
    fn context_overflow_above_100_pct_renders_true_pct_and_crit_remaining_zero() {
        let payload = StatuslinePayload {
            context_window: Some(ContextWindow {
                context_window_size: Some(2_000_000),
                used_percentage: Some(26.8),
                current_usage: Some(JsonUsage {
                    input_tokens: Some(36_000),
                    output_tokens: None,
                    cache_read_input_tokens: Some(500_000),
                    cache_creation_input_tokens: Some(0),
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let meta = SessionMetadata {
            max_context_tokens: Some(512_000),
            ..Default::default()
        };

        let mut cfg_used = LayoutConfig::preset_minimal();
        cfg_used.theme = ThemeStyle::Mono;
        cfg_used.bar_length = 8;
        cfg_used.rows = vec![vec![WidgetId::CtxBar, WidgetId::CtxPct]];
        let mut out_used = Vec::new();
        write_statusline(&payload, &meta, &GitInfo::default(), &cfg_used, &mut out_used).unwrap();
        assert_eq!(
            String::from_utf8(out_used).unwrap(),
            "ctx ████████ 104.7% (536K/512K) · ctx 104.7% (512K)\n"
        );

        let mut cfg_rem = LayoutConfig::preset_minimal();
        cfg_rem.theme = ThemeStyle::Default;
        cfg_rem.ctx_remaining = true;
        cfg_rem.bar_length = 8;
        cfg_rem.rows = vec![vec![WidgetId::CtxBar, WidgetId::CtxPct]];
        let mut out_rem = Vec::new();
        write_statusline(&payload, &meta, &GitInfo::default(), &cfg_rem, &mut out_rem).unwrap();
        let rem_text = String::from_utf8(out_rem).unwrap();
        let crit_code = format!("\x1b[38;5;{}m", palette_for(ThemeStyle::Default).ctx_crit);
        assert!(rem_text.contains(&crit_code));
        assert!(rem_text.contains("rem ████████ 0.0% (0/512K)"));
        assert!(rem_text.contains("rem 0.0% (512K)"));
    }

    #[test]
    fn nerd_font_credit_bucket_strips_reset_timer_in_stage2_compaction() {
        let mut cfg = LayoutConfig::preset_minimal();
        cfg.theme = ThemeStyle::Mono;
        cfg.glyph_mode = GlyphMode::Nerd;
        cfg.responsive = true;
        cfg.rows = vec![vec![WidgetId::Vcs, WidgetId::Quota5h]];

        let mut quota = BTreeMap::new();
        quota.insert(
            "acme-1h".to_string(),
            QuotaBucket {
                remaining_amount: Some(450),
                reset_in_seconds: Some(1_200),
                ..Default::default()
            },
        );
        // Full row: "<nf_branch> main · 1h 450 <nf_reset>20m" (19 cols).
        // After Stage 2: "<nf_branch> main · 1h 450" (15 cols).
        let payload = StatuslinePayload {
            quota: Some(quota),
            terminal_width: Some(16),
            ..Default::default()
        };
        let git = GitInfo {
            branch: Some("main".to_string()),
            ..Default::default()
        };

        let mut out = Vec::new();
        write_statusline(&payload, &SessionMetadata::default(), &git, &cfg, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("1h 450"), "text was: {text:?}");
        assert!(!text.contains('\u{f021}'), "reset glyph remained: {text:?}");
        assert!(!text.contains("20m"), "reset timer remained: {text:?}");
    }

    #[test]
    fn ctx_tokens_total_saturates_on_u64_max() {
        let mut cfg = LayoutConfig::preset_minimal();
        cfg.theme = ThemeStyle::Mono;
        cfg.rows = vec![vec![WidgetId::CtxTokens]];

        let payload = StatuslinePayload {
            context_window: Some(ContextWindow {
                total_input_tokens: Some(u64::MAX),
                total_output_tokens: Some(1),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut out = Vec::new();
        write_statusline(
            &payload,
            &SessionMetadata::default(),
            &GitInfo::default(),
            &cfg,
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text, format!("{} tok\n", format_tok_str(u64::MAX)));
    }

    #[test]
    fn cost_widget_falls_back_to_meta_estimated_cost_when_payload_cost_missing() {
        let mut cfg = LayoutConfig::preset_minimal();
        cfg.theme = ThemeStyle::Mono;
        cfg.rows = vec![vec![WidgetId::Cost]];

        let payload = StatuslinePayload::default();
        let meta = SessionMetadata {
            estimated_cost_usd: Some(1.2345),
            ..Default::default()
        };
        let mut out = Vec::new();
        write_statusline(&payload, &meta, &GitInfo::default(), &cfg, &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "~$1.23\n");

        let meta_sub = SessionMetadata {
            estimated_cost_usd: Some(1.2345),
            estimated_subagent_cost_usd: Some(0.25),
            ..Default::default()
        };
        let mut out_sub = Vec::new();
        write_statusline(&payload, &meta_sub, &GitInfo::default(), &cfg, &mut out_sub).unwrap();
        assert_eq!(String::from_utf8(out_sub).unwrap(), "~$1.23 (sub $0.25)\n");

        let meta_tiny_sub = SessionMetadata {
            estimated_cost_usd: Some(1.2345),
            estimated_subagent_cost_usd: Some(0.004),
            ..Default::default()
        };
        let mut out_tiny = Vec::new();
        write_statusline(
            &payload,
            &meta_tiny_sub,
            &GitInfo::default(),
            &cfg,
            &mut out_tiny,
        )
        .unwrap();
        assert_eq!(String::from_utf8(out_tiny).unwrap(), "~$1.23\n");

        let mut out_partial_sub = Vec::new();
        write_statusline_with_quotas(
            &payload,
            &meta_sub,
            &GitInfo::default(),
            (None, None),
            CostCoverage::Partial(2),
            &cfg,
            &mut out_partial_sub,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(out_partial_sub).unwrap(),
            "~$1.23 (sub $0.25, 2 unpriced)\n"
        );

        let mut out_partial_only = Vec::new();
        write_statusline_with_quotas(
            &payload,
            &meta,
            &GitInfo::default(),
            (None, None),
            CostCoverage::Partial(2),
            &cfg,
            &mut out_partial_only,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(out_partial_only).unwrap(),
            "~$1.23 (2 unpriced)\n"
        );
    }

    #[test]
    fn render_widgets_strip_escape_sequences_and_newlines_from_untrusted_fields() {
        let payload = StatuslinePayload {
            agent_state: Some("\x1b[31mcustom\nstate\x1b[0m".to_string()),
            model: Some(ModelInfo {
                id: None,
                display_name: Some("Gemini\n4\x1b[31m".to_string()),
                effort: Some("\x1b[1mhigh\tmode\x1b[0m".to_string()),
            }),
            workspace: Some(WorkspaceInfo {
                current_dir: Some("/tmp/my\x1b[32mdir\nname".to_string()),
                project_dir: None,
            }),
            vcs: Some(VcsInfo {
                vcs_type: Some("git\x1b[0m".to_string()),
                branch: Some("feat/\x1b[31mbar\nbaz".to_string()),
                client: None,
                dirty: Some(false),
            }),
            cycle_mode: Some("plan\x1b[33m\n".to_string()),
            vim: Some(VimInfo {
                mode: Some("\x1b[31minsert\n".to_string()),
            }),
            battle: Some(BattleInfo {
                status: Some("active\x1b[0m\tnow".to_string()),
                focused_arm: Some(1),
            }),
            conversation_title: Some("\x1b]0;evil\x07Hello\nWorld".to_string()),
            agent: Some(AgentInfo {
                name: Some("rev\x1b[2Kiewer\n".to_string()),
            }),
            ..Default::default()
        };
        let git = GitInfo {
            worktree: Some("wt\x1b[31m\n1".to_string()),
            ..Default::default()
        };
        let mut cfg = LayoutConfig::preset_minimal();
        cfg.theme = ThemeStyle::Mono;
        cfg.glyph_mode = GlyphMode::Ascii;
        cfg.responsive = false;
        cfg.rows = vec![vec![
            WidgetId::State,
            WidgetId::Model,
            WidgetId::Directory,
            WidgetId::Vcs,
            WidgetId::Worktree,
            WidgetId::Mode,
            WidgetId::Vim,
            WidgetId::Battle,
            WidgetId::Title,
            WidgetId::Agent,
        ]];

        let mut out = Vec::new();
        write_statusline(&payload, &SessionMetadata::default(), &git, &cfg, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains('\x1b'), "ESC leaked: {text:?}");
        assert_eq!(text.lines().count(), 1, "newline leaked: {text:?}");
        assert!(text.contains("CUSTOM STATE"), "text: {text}");
        assert!(text.contains("Gemini 4 / high mode"), "text: {text}");
        assert!(text.contains("mydir name"), "text: {text}");
        assert!(text.contains("git: feat/bar baz"), "text: {text}");
        assert!(text.contains("wt:wt 1"), "text: {text}");
        assert!(text.contains("mode: PLAN"), "text: {text}");
        assert!(text.contains("vim INSERT"), "text: {text}");
        assert!(text.contains("battle ACTIVE NOW #1"), "text: {text}");
        assert!(text.contains("Hello World"), "text: {text}");
        assert!(text.contains("agent reviewer"), "text: {text}");
    }

    #[test]
    fn non_finite_and_negative_floats_fail_closed_in_cost_speed_and_ttft() {
        let mut cfg = LayoutConfig::preset_minimal();
        cfg.theme = ThemeStyle::Mono;
        cfg.glyph_mode = GlyphMode::Ascii;
        cfg.smart_hide = false;
        cfg.rows = vec![vec![WidgetId::Speed, WidgetId::Ttft, WidgetId::Cost]];

        let bad_payload = StatuslinePayload {
            cost: Some(CostInfo {
                total_usd: Some(f64::INFINITY),
                subagent_usd: Some(f64::NAN),
                estimated: Some(false),
            }),
            ..Default::default()
        };
        let bad_meta = SessionMetadata {
            speed: Some(f64::NAN),
            ttft_secs: Some(-0.5),
            estimated_cost_usd: Some(-2.0),
            ..Default::default()
        };
        let mut out = Vec::new();
        write_statusline(&bad_payload, &bad_meta, &GitInfo::default(), &cfg, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text, "spd -- * ttft --\n");

        let partial_payload = StatuslinePayload {
            cost: Some(CostInfo {
                total_usd: Some(1.50),
                subagent_usd: Some(f64::INFINITY),
                estimated: Some(false),
            }),
            ..Default::default()
        };
        let partial_meta = SessionMetadata {
            speed: Some(-10.0),
            ttft_secs: Some(f64::INFINITY),
            ..Default::default()
        };
        let mut out2 = Vec::new();
        write_statusline(
            &partial_payload,
            &partial_meta,
            &GitInfo::default(),
            &cfg,
            &mut out2,
        )
        .unwrap();
        let text2 = String::from_utf8(out2).unwrap();
        assert_eq!(text2, "spd -- * ttft -- * $1.50\n");
    }
}
