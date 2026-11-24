//! Multi-style, theme-aware statusline renderer (`Plain`, `Pill`, `Powerline`).

mod emit;
mod fit;
mod widgets;

use std::io::{self, Write as IoWrite};

use crate::agy::db::SessionMetadata;
use crate::config::layout::{LayoutConfig, SpeedMode, WidgetId, DEFAULT_SPEED_WARN_THRESHOLD};
use crate::statusline::git::GitInfo;
use crate::statusline::metrics::{
    current_context_tokens, resolve_quota_views, valid_cache_usage, QuotaView,
};
use crate::statusline::theme::{glyphs_for, palette_for, GlyphSet, Palette};
use crate::statusline::StatuslinePayload;
use crate::sys::time::unix_now_secs;

use emit::{compute_geometry, StyleGeometry};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompactStage {
    VcsDropDiff,
    VcsTruncate,
    SubagentsDropLabel,
    CostStripSuffix,
    QuotaStripReset,
    CtxDropLimit,
    CtxCollapseBar,
    ModelAbbreviate,
    ModelTruncate,
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
enum ResolvedCost {
    Payload {
        total_usd: f64,
        subagent_usd: Option<f64>,
        estimated: bool,
    },
    Estimated {
        total_usd: f64,
        subagent_usd: Option<f64>,
        unpriced_turns: u64,
    },
}

struct RenderContext<'a> {
    cache: Option<(f64, u64)>,
    used_pct: f64,
    cur_ctx: Option<u64>,
    eff_limit: Option<u64>,
    speed: Option<f64>,
    speed_warn_threshold: f64,
    ttft_secs: Option<f64>,
    thinking_tokens: Option<u64>,
    cost: Option<ResolvedCost>,
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
        unpriced_turns: u64,
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
            (Some(cur), Some(lim)) => (cur as f64) * 100.0 / (lim as f64),
            _ => payload
                .context_window
                .as_ref()
                .and_then(|c| c.used_percentage)
                .filter(|v| v.is_finite())
                .map_or(0.0, |v| v.max(0.0)),
        };
        let speed = match cfg.speed_mode {
            SpeedMode::Weighted => meta.speed,
            SpeedMode::Median => meta.median_speed.or(meta.speed),
            SpeedMode::Instant => meta.last_turn_speed.or(meta.speed),
            SpeedMode::E2e => meta.e2e_speed.or(meta.speed),
        }
        .filter(|s| s.is_finite() && *s >= 0.0);
        let speed_warn_threshold =
            if cfg.speed_warn_threshold.is_finite() && cfg.speed_warn_threshold >= 0.0 {
                cfg.speed_warn_threshold
            } else {
                DEFAULT_SPEED_WARN_THRESHOLD
            };
        let cost = if let Some(total_usd) = payload.valid_cost_usd() {
            let payload_cost = payload.cost.as_ref();
            Some(ResolvedCost::Payload {
                total_usd,
                subagent_usd: payload_cost
                    .and_then(|c| c.subagent_usd)
                    .filter(|&s| s.is_finite() && s >= 0.01),
                estimated: payload_cost.is_some_and(|c| c.estimated == Some(true)),
            })
        } else {
            meta.estimated_cost_usd
                .filter(|v| v.is_finite() && *v >= 0.0)
                .map(|total_usd| ResolvedCost::Estimated {
                    total_usd,
                    subagent_usd: meta
                        .estimated_subagent_cost_usd
                        .filter(|&s| s.is_finite() && s >= 0.01),
                    unpriced_turns,
                })
        };
        Self {
            cache,
            used_pct,
            cur_ctx,
            eff_limit,
            speed,
            speed_warn_threshold,
            ttft_secs: meta.ttft_secs.filter(|t| t.is_finite() && *t >= 0.0),
            thinking_tokens: meta.thinking_tokens.filter(|&t| t > 0),
            cost,
            quota_5h,
            quota_weekly,
            git,
        }
    }
}

/// Builds a `bar_len`-segment Unicode context progress bar (`█▓▒░·`).
pub fn build_progress_bar(used_percentage: f64, bar_len: usize) -> (String, f64) {
    let clamped = if used_percentage.is_finite() {
        used_percentage.clamp(0.0, 100.0)
    } else {
        0.0
    };
    (
        build_progress_bar_with_chars(clamped, bar_len, ['█', '▓', '▒', '░', '·']),
        clamped,
    )
}

fn build_progress_bar_with_chars(used_percentage: f64, bar_len: usize, chars: [char; 5]) -> String {
    let pct_int = used_percentage.clamp(0.0, 100.0).trunc() as usize;
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
    bar
}

/// Renders all non-empty configured rows in `cfg` for `payload`, `meta`, and `git`.
pub fn write_statusline<W: IoWrite>(
    payload: &StatuslinePayload,
    meta: &SessionMetadata,
    git: &GitInfo,
    cfg: &LayoutConfig,
    out: &mut W,
) -> io::Result<()> {
    write_statusline_at(payload, meta, git, 0, unix_now_secs(), cfg, out)
}

fn layout_statusline_rows<'a>(
    payload: &StatuslinePayload,
    meta: &SessionMetadata,
    git: &'a GitInfo,
    unpriced_turns: u64,
    now_unix_secs: u64,
    cfg: &'a LayoutConfig,
) -> (Renderer<'a>, Vec<Vec<Vec<Segment>>>) {
    let quotas = resolve_quota_views(payload, now_unix_secs);
    let renderer = Renderer::new(cfg);
    let ctx = RenderContext::build(payload, meta, git, quotas, unpriced_turns, cfg);
    let max_width = cfg.responsive.then_some(payload.terminal_width).flatten();

    let rows = cfg
        .rows
        .iter()
        .map(|row| {
            let segs: Vec<Segment> = row
                .iter()
                .filter_map(|&w| renderer.render_widget(w, payload, &ctx))
                .collect();
            let groups = fit::split_into_groups(segs);
            fit::fit_row_groups(groups, max_width, renderer.geom, renderer.glyphs.ellipsis)
        })
        .collect();
    (renderer, rows)
}

/// Renders all non-empty configured rows using an explicit unpriced-turn count and timestamp.
pub(crate) fn write_statusline_at<W: IoWrite>(
    payload: &StatuslinePayload,
    meta: &SessionMetadata,
    git: &GitInfo,
    unpriced_turns: u64,
    now_unix_secs: u64,
    cfg: &LayoutConfig,
    out: &mut W,
) -> io::Result<()> {
    let (renderer, rows) =
        layout_statusline_rows(payload, meta, git, unpriced_turns, now_unix_secs, cfg);
    for fitted in &rows {
        renderer.write_row_groups(fitted, payload.terminal_width, out)?;
    }
    Ok(())
}

/// Renders all non-empty configured rows directly into styled [`ratatui::text::Line`] values.
pub(crate) fn render_statusline_lines(
    payload: &StatuslinePayload,
    meta: &SessionMetadata,
    git: &GitInfo,
    cfg: &LayoutConfig,
) -> Vec<ratatui::text::Line<'static>> {
    let (renderer, rows) = layout_statusline_rows(payload, meta, git, 0, unix_now_secs(), cfg);
    rows.iter()
        .filter_map(|fitted| renderer.render_row_groups_line(fitted, payload.terminal_width))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use unicode_width::UnicodeWidthStr;

    use super::*;
    use crate::config::layout::{GlyphMode, ThemeStyle, VisualStyle};
    use crate::statusline::{
        AgentInfo, BattleInfo, ContextWindow, CostInfo, JsonUsage, ModelInfo, QuotaBucket,
        SandboxInfo, SubagentInfo, VcsInfo, VimInfo, WorkspaceInfo,
    };

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
        smart_hide: bool,
    ) -> String {
        let mut cfg = LayoutConfig::preset_minimal();
        cfg.rows = vec![vec![widget]];
        cfg.glyph_mode = glyph_mode;
        cfg.theme = ThemeStyle::Mono;
        cfg.smart_hide = smart_hide;
        cfg.no_color = true;
        cfg.responsive = false;
        render_mono(payload, meta, &GitInfo::default(), &cfg)
    }

    fn visible_line_width(rendered: &str) -> usize {
        rendered
            .lines()
            .next()
            .map_or(0, |line| strip_ansi_escapes::strip_str(line).width())
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
        let mut narrow_lines = narrow_text.lines();
        let narrow_row1 = narrow_lines.next().unwrap();
        let narrow_row2 = narrow_lines.next().unwrap();
        assert!(narrow_lines.next().is_none());
        assert!(narrow_row1.width() <= 40, "row1: {narrow_row1:?}");
        assert!(narrow_row2.width() <= 40, "row2: {narrow_row2:?}");
        assert!(narrow_row1.contains("THINKING"));
        assert!(!narrow_row1.contains("wt:feat-wt"));
        assert!(!narrow_row1.contains("agy-statusline"));
        assert!(narrow_row2.contains("ctx 0.2%"));
        assert!(!narrow_row2.contains("~$0.14"));
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
        write_statusline(
            &payload,
            &meta,
            &GitInfo::default(),
            &cfg_used,
            &mut out_used,
        )
        .unwrap();
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

        // Non-overflow remaining mode renders (remaining/limit) in ctx_bar.
        let payload_half = StatuslinePayload {
            context_window: Some(ContextWindow {
                context_window_size: Some(200_000),
                current_usage: Some(JsonUsage {
                    input_tokens: Some(15_000),
                    output_tokens: None,
                    cache_read_input_tokens: Some(70_000),
                    cache_creation_input_tokens: Some(0),
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut out_rem_half = Vec::new();
        write_statusline(
            &payload_half,
            &SessionMetadata::default(),
            &GitInfo::default(),
            &cfg_rem,
            &mut out_rem_half,
        )
        .unwrap();
        let rem_half = String::from_utf8(out_rem_half).unwrap();
        assert!(rem_half.contains("57.5% (115K/200K)"));
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
        assert_eq!(text, "18,446,744,073B tok\n");
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
        write_statusline_at(
            &payload,
            &meta_sub,
            &GitInfo::default(),
            2,
            0,
            &cfg,
            &mut out_partial_sub,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(out_partial_sub).unwrap(),
            "~$1.23 (sub $0.25, 2 unpriced)\n"
        );

        let mut out_partial_only = Vec::new();
        write_statusline_at(
            &payload,
            &meta,
            &GitInfo::default(),
            2,
            0,
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
        assert!(rendered_est.contains("25.0% (128K/512K)"));
        assert!(rendered_est.contains("· 128K/512K"));

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
        let rendered_actual =
            render_mono(&payload_with_usage, &meta_est, &GitInfo::default(), &cfg);
        assert!(rendered_actual.contains("50.0% (256K/512K)"));

        // Zero estimated_context_tokens falls back to showing only the limit in ctx_bar.
        let meta_zero = SessionMetadata {
            estimated_context_tokens: Some(0),
            max_context_tokens: Some(512_000),
            ..Default::default()
        };
        let rendered_zero = render_mono(&payload_no_usage, &meta_zero, &GitInfo::default(), &cfg);
        assert!(rendered_zero.contains("0.0% (512K)"));
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

        // At 90 cols, Stages 1-4 compact Vcs/Quota/CtxBar/Model so core widgets survive.
        let out = render_mono(&base_compaction_payload(90), &meta, &git, &cfg);
        assert!(out.contains("feat/ui*"));
        assert!(out.contains("82 tok/s"));
        assert!(out.contains("~$0.42"));
        assert!(out.contains("ctx 50.0%"));
        assert!(out.contains("5h 84%"));
        assert!(!out.contains("512K"));
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

        // Stage 3 strips (512K); Stage 4 abbreviates "Gemini " -> "G" & caps Model at 20 cols.
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

        // Claude abbreviation check: "Claude Sonnet 4.6 (Thinking)" -> "CSonnet 4.6 (Thin…"
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
    fn thinking_widget_renders_glyph_modes_and_smart_hide() {
        let render_thinking = |thinking_tokens: Option<u64>, glyph_mode, smart_hide| {
            let meta = SessionMetadata {
                thinking_tokens,
                ..Default::default()
            };
            render_single(
                WidgetId::Thinking,
                &StatuslinePayload::default(),
                &meta,
                glyph_mode,
                smart_hide,
            )
        };

        assert_eq!(
            render_thinking(Some(4_200), GlyphMode::Unicode, false),
            "◆ think 4.2K\n"
        );
        assert_eq!(
            render_thinking(Some(4_200), GlyphMode::Nerd, false),
            "\u{f0eb} think 4.2K\n"
        );
        assert_eq!(
            render_thinking(Some(4_200), GlyphMode::Ascii, false),
            "~ think 4.2K\n"
        );
        assert_eq!(render_thinking(None, GlyphMode::Unicode, true), "");
        assert_eq!(render_thinking(Some(0), GlyphMode::Unicode, true), "");
        assert_eq!(
            render_thinking(None, GlyphMode::Unicode, false),
            "◆ think --\n"
        );
        assert_eq!(
            render_thinking(Some(0), GlyphMode::Nerd, false),
            "\u{f0eb} think --\n"
        );
        assert_eq!(
            render_thinking(None, GlyphMode::Ascii, false),
            "~ think --\n"
        );
    }

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
                true
            ),
            "◎ CONFIRM\n"
        );
        assert_eq!(
            render_single(
                WidgetId::State,
                &pending,
                &empty_meta,
                GlyphMode::Ascii,
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
                true
            ),
            "● READY\n"
        );
    }

    #[test]
    fn vim_widget_renders_modes_trimming_and_smart_hide() {
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
                smart_hide,
            )
        };

        assert_eq!(render_vim(Some("insert"), true), "vim INSERT\n");
        assert_eq!(render_vim(Some("visual"), true), "vim VISUAL\n");
        assert_eq!(render_vim(Some("v-line"), true), "vim V-LINE\n");
        assert_eq!(render_vim(Some("v-block"), true), "vim V-BLOCK\n");
        assert_eq!(render_vim(Some("command"), true), "vim COMMAND\n");
        assert_eq!(render_vim(Some("replace"), true), "vim REPLACE\n");
        assert_eq!(render_vim(Some(" normal "), true), "vim NORMAL\n");
        assert_eq!(render_vim(Some("   "), true), "");
        assert_eq!(render_vim(None, false), "vim off\n");
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
                true
            ),
            "queue 3\n"
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
                true
            ),
            ""
        );
        assert_eq!(
            render_single(
                WidgetId::Queue,
                &StatuslinePayload::default(),
                &empty_meta,
                GlyphMode::Unicode,
                false
            ),
            "queue 0\n"
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
                true
            ),
            "battle ACTIVE #2\n"
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
                true
            ),
            "battle TIE\n"
        );
        assert_eq!(
            render_single(
                WidgetId::Battle,
                &StatuslinePayload::default(),
                &empty_meta,
                GlyphMode::Unicode,
                true
            ),
            ""
        );
        assert_eq!(
            render_single(
                WidgetId::Battle,
                &StatuslinePayload::default(),
                &empty_meta,
                GlyphMode::Unicode,
                false
            ),
            "battle off\n"
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
                false
            ),
            "sandbox off\n"
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
                false
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
                "workspace": {"project_dir": "/work/agy-statusline"},
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
            render_single(w, p, &empty_meta, GlyphMode::Ascii, sh)
                .trim()
                .to_string()
        };
        assert_eq!(
            r(WidgetId::Title, &payload, true),
            "Refactor statusline payload wi.."
        );
        assert_eq!(
            r(WidgetId::Directory, &payload, true),
            "dir: agy-statusline"
        );
        assert_eq!(r(WidgetId::Agent, &payload, true), "agent researcher");
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
        assert_eq!(r(WidgetId::Title, &blank, false), "");
        assert_eq!(r(WidgetId::Directory, &blank, false), "");
        assert_eq!(r(WidgetId::Agent, &blank, false), "");
        assert_eq!(r(WidgetId::Session, &blank, false), "");
        assert_eq!(r(WidgetId::Transcript, &blank, false), "");
        assert_eq!(r(WidgetId::Vcs, &blank, false), "");
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

        let rendered = render_mono(
            &payload,
            &SessionMetadata::default(),
            &GitInfo::default(),
            &cfg,
        );
        assert!(rendered.contains("READY"), "rendered: {rendered}");
        assert!(
            rendered.contains("Implement full.."),
            "rendered: {rendered}"
        );
        assert!(rendered.contains("alice"), "rendered: {rendered}");
        assert!(!rendered.contains("@example.com"), "rendered: {rendered}");
        assert!(rendered.contains("sub 1"), "rendered: {rendered}");
        assert!(!rendered.contains("Reviewer"), "rendered: {rendered}");
        assert!(!rendered.contains('('), "unclosed paren in: {rendered}");
    }

    #[test]
    fn cost_strip_suffix_runs_before_ctx_bar_compaction() {
        let mut cfg = LayoutConfig::preset_minimal();
        cfg.theme = ThemeStyle::Mono;
        cfg.bar_length = 8;
        cfg.rows = vec![vec![WidgetId::CtxBar, WidgetId::Cost]];

        let mut payload = base_compaction_payload(40);
        payload.cost = Some(CostInfo {
            total_usd: Some(0.42),
            subagent_usd: Some(0.12),
            estimated: Some(true),
        });
        let meta = SessionMetadata {
            max_context_tokens: Some(512_000),
            ..Default::default()
        };

        // Uncompacted: "ctx ████···· 50.0% (256K/512K) · ~$0.42 (sub $0.12)" (51 cols)
        // After CostStripSuffix: "ctx ████···· 50.0% (256K/512K) · ~$0.42" (39 cols)
        let out = render_mono(&payload, &meta, &GitInfo::default(), &cfg);
        assert!(
            out.contains("ctx ████···· 50.0% (256K/512K)"),
            "out: {out:?}"
        );
        assert!(out.contains("~$0.42"), "out: {out:?}");
        assert!(!out.contains("sub $0.12"), "out: {out:?}");
        assert!(visible_line_width(&out) <= 40, "out: {out:?}");
    }

    #[test]
    fn subagents_label_drops_before_quota_reset_timer() {
        let mut cfg = LayoutConfig::preset_minimal();
        cfg.theme = ThemeStyle::Mono;
        cfg.rows = vec![vec![WidgetId::Quota5h, WidgetId::Subagents]];

        let quota = BTreeMap::from([(
            "gemini-5h".to_string(),
            QuotaBucket {
                remaining_fraction: Some(0.84),
                reset_in_seconds: Some(9_900),
                ..Default::default()
            },
        )]);
        let render_at = |width: usize| {
            let payload = StatuslinePayload {
                quota: Some(quota.clone()),
                subagents: Some(vec![SubagentInfo {
                    name: None,
                    role: Some("Codebase Researcher".to_string()),
                    status: Some("running".to_string()),
                }]),
                terminal_width: Some(width),
                ..Default::default()
            };
            let mut out = Vec::new();
            write_statusline_at(
                &payload,
                &SessionMetadata::default(),
                &GitInfo::default(),
                0,
                0,
                &cfg,
                &mut out,
            )
            .unwrap();
            String::from_utf8(out).unwrap()
        };

        // Full (42 cols): "5h 84% ↻2h45m · sub 1 (Codebase Researcher)"
        // SubagentsDropLabel runs before QuotaStripReset (21 cols):
        assert_eq!(render_at(22), "5h 84% ↻2h45m · sub 1\n");
        // QuotaStripReset drops ↻2h45m when narrower (14 cols):
        assert_eq!(render_at(15), "5h 84% · sub 1\n");
    }

    #[test]
    fn cache_hit_outlives_directory_on_narrow_row() {
        let mut cfg = LayoutConfig::preset_minimal();
        cfg.theme = ThemeStyle::Mono;
        cfg.rows = vec![vec![
            WidgetId::State,
            WidgetId::Directory,
            WidgetId::CacheHit,
        ]];

        let payload = StatuslinePayload {
            agent_state: Some("idle".to_string()),
            workspace: Some(WorkspaceInfo {
                current_dir: Some("/work/agy-statusline".to_string()),
                project_dir: None,
            }),
            context_window: Some(ContextWindow {
                current_usage: Some(JsonUsage {
                    input_tokens: Some(200),
                    output_tokens: None,
                    cache_read_input_tokens: Some(800),
                    cache_creation_input_tokens: Some(0),
                }),
                ..Default::default()
            }),
            terminal_width: Some(25),
            ..Default::default()
        };

        // Full: "● READY · ⌂ agy-statusline · ◈ cache 80.0%" (42 cols).
        // Even though CacheHit is rightmost, Directory (p2) drops before CacheHit (p3).
        let out = render_mono(
            &payload,
            &SessionMetadata::default(),
            &GitInfo::default(),
            &cfg,
        );
        assert_eq!(out, "● READY · ◈ cache 80.0%\n");
    }

    #[test]
    fn preset_full_fits_realistic_session_uncompacted_at_120_cols() {
        let cfg = LayoutConfig::preset_full();
        let mut payload = base_compaction_payload(120);
        payload.version = Some("1.5.16".to_string());
        let meta = SessionMetadata {
            speed: Some(82.0),
            max_context_tokens: Some(512_000),
            ..Default::default()
        };
        let git = GitInfo {
            branch: Some("feat/ui".to_string()),
            added: 12,
            deleted: 3,
            ..Default::default()
        };

        let mut out = Vec::new();
        write_statusline(&payload, &meta, &git, &cfg, &mut out).unwrap();
        let plain = strip_ansi_escapes::strip_str(String::from_utf8(out).unwrap());
        let mut lines = plain.lines();
        let row1 = lines.next().unwrap();
        let row2 = lines.next().unwrap();
        assert!(lines.next().is_none(), "expected 2 rows: {plain:?}");

        assert!(
            row1.width() <= 120,
            "row1 width {} > 120: {row1:?}",
            row1.width()
        );
        assert!(row1.contains("THINKING"), "row1: {row1:?}");
        assert!(row1.contains("PLAN"), "row1: {row1:?}");
        assert!(
            row1.contains("Gemini 3.1 Pro Preview · high"),
            "row1: {row1:?}"
        );
        assert!(row1.contains("agy-statusline"), "row1: {row1:?}");
        assert!(row1.contains("feat/ui* +12 -3"), "row1: {row1:?}");
        assert!(row1.contains("v1.5.16"), "row1: {row1:?}");

        assert!(
            row2.width() <= 120,
            "row2 width {} > 120: {row2:?}",
            row2.width()
        );
        assert!(
            row2.contains("ctx ████···· 50.0% (256K/512K)"),
            "row2: {row2:?}"
        );
        assert!(row2.contains("cache 78.1%"), "row2: {row2:?}");
        assert!(row2.contains("82 tok/s"), "row2: {row2:?}");
        assert!(row2.contains("~$0.42"), "row2: {row2:?}");
        assert!(row2.contains("5h 84% ↻3h10m"), "row2: {row2:?}");
        assert!(row2.contains("7d 92% ↻5d"), "row2: {row2:?}");
    }
}
