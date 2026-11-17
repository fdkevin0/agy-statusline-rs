//! Multi-style, theme-aware statusline renderer (`Plain`, `Pill`, `Powerline`).

use std::io::{self, Write as IoWrite};

use unicode_width::UnicodeWidthStr;

use crate::db::SessionMetadata;
use crate::format::format_tokens;
use crate::git::GitInfo;
use crate::layout::{LayoutConfig, SpeedMode, ThemeStyle, VisualStyle, WidgetId};
use crate::statusline::StatuslinePayload;

#[derive(Debug, Clone, Copy)]
struct Palette {
    ready: u8,
    thinking: u8,
    working: u8,
    tool: u8,
    model: u8,
    dir: u8,
    vcs_clean: u8,
    vcs_dirty: u8,
    ctx_ok: u8,
    ctx_warn: u8,
    ctx_crit: u8,
    cache: u8,
    speed_ok: u8,
    speed_slow: u8,
    cost: u8,
    muted: u8,
    sep: u8,
    pill_bg: u8,
    pl_bg_a: u8,
    pl_bg_b: u8,
    pl_accent_fg: u8,
}

const PALETTES: [Palette; 4] = [
    // Default
    Palette {
        ready: 78, thinking: 220, working: 81, tool: 177, model: 183, dir: 117,
        vcs_clean: 75, vcs_dirty: 203, ctx_ok: 78, ctx_warn: 220, ctx_crit: 203,
        cache: 111, speed_ok: 80, speed_slow: 214, cost: 150, muted: 245, sep: 240,
        pill_bg: 236, pl_bg_a: 237, pl_bg_b: 235, pl_accent_fg: 232,
    },
    // Nord
    Palette {
        ready: 108, thinking: 222, working: 110, tool: 139, model: 110, dir: 109,
        vcs_clean: 74, vcs_dirty: 174, ctx_ok: 108, ctx_warn: 222, ctx_crit: 174,
        cache: 110, speed_ok: 109, speed_slow: 215, cost: 151, muted: 244, sep: 239,
        pill_bg: 236, pl_bg_a: 238, pl_bg_b: 235, pl_accent_fg: 233,
    },
    // Catppuccin
    Palette {
        ready: 114, thinking: 223, working: 117, tool: 183, model: 183, dir: 117,
        vcs_clean: 111, vcs_dirty: 210, ctx_ok: 114, ctx_warn: 223, ctx_crit: 210,
        cache: 147, speed_ok: 116, speed_slow: 216, cost: 151, muted: 245, sep: 240,
        pill_bg: 237, pl_bg_a: 238, pl_bg_b: 236, pl_accent_fg: 232,
    },
    // Mono
    Palette {
        ready: 15, thinking: 15, working: 15, tool: 15, model: 15, dir: 15,
        vcs_clean: 15, vcs_dirty: 15, ctx_ok: 15, ctx_warn: 15, ctx_crit: 15,
        cache: 15, speed_ok: 15, speed_slow: 15, cost: 15, muted: 15, sep: 15,
        pill_bg: 0, pl_bg_a: 0, pl_bg_b: 0, pl_accent_fg: 0,
    },
];

impl Palette {
    const fn for_theme(theme: ThemeStyle) -> Self {
        match theme {
            ThemeStyle::Default => PALETTES[0],
            ThemeStyle::Nord => PALETTES[1],
            ThemeStyle::Catppuccin => PALETTES[2],
            ThemeStyle::Mono => PALETTES[3],
        }
    }
}

struct Segment {
    text: String,
    fg: u8,
    accent_bg: Option<u8>,
    bold: bool,
    is_spacer: bool,
}

impl Segment {
    fn plain(text: String, fg: u8, bold: bool) -> Self {
        Self { text, fg, accent_bg: None, bold, is_spacer: false }
    }

    fn spacer() -> Self {
        Self { text: String::new(), fg: 0, accent_bg: None, bold: false, is_spacer: true }
    }
}

/// Builds a `bar_len`-segment Unicode context progress bar (`█▓▒░·`).
pub fn build_progress_bar(used_percentage: f64, bar_len: usize) -> (String, f64) {
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
            '█'
        } else if i == filled && remainder >= 75 {
            '▓'
        } else if i == filled && remainder >= 50 {
            '▒'
        } else if i == filled && remainder >= 25 {
            '░'
        } else {
            '·'
        };
        bar.push(ch);
    }
    (bar, clamped)
}

fn ctx_color(pct: f64, pal: &Palette) -> u8 {
    if pct >= 85.0 {
        pal.ctx_crit
    } else if pct >= 60.0 {
        pal.ctx_warn
    } else {
        pal.ctx_ok
    }
}

fn valid_cache_usage(payload: &StatuslinePayload) -> Option<(f64, u64)> {
    let u = payload.context_window.as_ref()?.current_usage?;
    let (Some(input), Some(read), Some(0)) = (
        u.input_tokens,
        u.cache_read_input_tokens,
        u.cache_creation_input_tokens,
    ) else {
        return None;
    };
    if input < 0 || read < 0 {
        return None;
    }
    let total = (input as f64) + (read as f64);
    (total > 0.0).then(|| (100.0 * (read as f64) / total, read as u64))
}

fn current_context_tokens(payload: &StatuslinePayload) -> Option<u64> {
    let u = payload.context_window.as_ref()?.current_usage?;
    if u.input_tokens.is_none()
        && u.cache_read_input_tokens.is_none()
        && u.cache_creation_input_tokens.is_none()
    {
        return None;
    }
    let inp = u.input_tokens.unwrap_or(0).max(0) as u64;
    let read = u.cache_read_input_tokens.unwrap_or(0).max(0) as u64;
    let create = u.cache_creation_input_tokens.unwrap_or(0).max(0) as u64;
    Some(inp + read + create)
}

fn format_tok_str(tokens: u64) -> String {
    let mut buf = String::new();
    format_tokens(tokens, &mut buf);
    buf
}

struct RenderContext<'a> {
    cache: Option<(f64, u64)>,
    used_pct: f64,
    cur_ctx: Option<u64>,
    eff_limit: Option<u64>,
    speed: Option<f64>,
    ttft_secs: Option<f64>,
    git: &'a GitInfo,
}

fn render_widget(
    widget: WidgetId,
    payload: &StatuslinePayload,
    ctx: &RenderContext<'_>,
    cfg: &LayoutConfig,
    pal: &Palette,
) -> Option<Segment> {
    let cache = ctx.cache;
    let used_pct = ctx.used_pct;
    let cur_ctx = ctx.cur_ctx;
    let eff_limit = ctx.eff_limit;
    let speed = ctx.speed;

    match widget {
        WidgetId::Spacer => Some(Segment::spacer()),
        WidgetId::State => {
            let state = payload.agent_state.as_deref().unwrap_or("idle");
            let (text, fg) = match state {
                "idle" => ("● READY".to_string(), pal.ready),
                "thinking" => ("◆ THINKING".to_string(), pal.thinking),
                "working" => ("⚙ WORKING".to_string(), pal.working),
                "tool_use" => ("🔧 TOOL".to_string(), pal.tool),
                other => (format!("⏳ {}", other.to_ascii_uppercase()), pal.muted),
            };
            Some(Segment { text, fg, accent_bg: Some(fg), bold: true, is_spacer: false })
        }
        WidgetId::Model => {
            let model = payload.model.as_ref()?;
            let name = model.display_name.as_deref()?.trim();
            if name.is_empty() {
                return None;
            }
            let text = match model.effort.as_deref().map(str::trim).filter(|e| !e.is_empty()) {
                Some(effort)
                    if !name
                        .to_ascii_lowercase()
                        .contains(&effort.to_ascii_lowercase()) =>
                {
                    format!("{name} · {effort}")
                }
                _ => name.to_string(),
            };
            Some(Segment::plain(text, pal.model, true))
        }
        WidgetId::Directory => Some(Segment::plain(
            format!("⌂ {}", payload.workspace_basename()?),
            pal.dir,
            false,
        )),
        WidgetId::Vcs => {
            let vcs = payload.vcs.as_ref()?;
            let branch = vcs.branch.as_deref()?.trim();
            if branch.is_empty() {
                return None;
            }
            let dirty = vcs.dirty.unwrap_or(false);
            let mut text = if dirty { format!("⎇ {branch}*") } else { format!("⎇ {branch}") };
            if cfg.show_git_diff && (ctx.git.added > 0 || ctx.git.deleted > 0) {
                text.push_str(&format!(" +{} -{}", ctx.git.added, ctx.git.deleted));
            }
            Some(Segment::plain(text, if dirty { pal.vcs_dirty } else { pal.vcs_clean }, dirty))
        }
        WidgetId::Worktree => match &ctx.git.worktree {
            Some(wt) => Some(Segment::plain(format!("⎇ wt:{wt}"), pal.dir, true)),
            None if cfg.smart_hide => None,
            None => Some(Segment::plain("wt:main".to_string(), pal.muted, false)),
        },
        WidgetId::CtxBar => {
            let (bar, pct) = build_progress_bar(used_pct, cfg.bar_length);
            let (tag, disp_pct) = if cfg.ctx_remaining {
                ("rem", (100.0 - pct).max(0.0))
            } else {
                ("ctx", pct)
            };
            let text = match eff_limit {
                Some(lim) => format!("{tag} {bar} {disp_pct:.1}% ({})", format_tok_str(lim)),
                None => format!("{tag} {bar} {disp_pct:.1}%"),
            };
            Some(Segment::plain(text, ctx_color(pct, pal), pct >= 85.0))
        }
        WidgetId::CtxPct => {
            let pct = if used_pct.is_finite() { used_pct.clamp(0.0, 100.0) } else { 0.0 };
            let (tag, disp_pct) = if cfg.ctx_remaining {
                ("rem", (100.0 - pct).max(0.0))
            } else {
                ("ctx", pct)
            };
            let text = match eff_limit {
                Some(lim) => format!("{tag} {disp_pct:.1}% ({})", format_tok_str(lim)),
                None => format!("{tag} {disp_pct:.1}%"),
            };
            Some(Segment::plain(text, ctx_color(pct, pal), false))
        }
        WidgetId::CtxTokens => {
            if let (Some(cur), Some(lim)) = (cur_ctx, eff_limit) {
                return Some(Segment::plain(
                    format!("{}/{}", format_tok_str(cur), format_tok_str(lim)),
                    pal.muted,
                    false,
                ));
            }
            let cw = payload.context_window.as_ref()?;
            let total = cw.total_input_tokens.unwrap_or(0) + cw.total_output_tokens.unwrap_or(0);
            if total == 0 {
                return None;
            }
            Some(Segment::plain(format!("{} tok", format_tok_str(total)), pal.muted, false))
        }
        WidgetId::CacheHit => Some(Segment::plain(
            cache.map_or_else(|| "◈ cache --".to_string(), |(pct, _)| format!("◈ cache {pct:.1}%")),
            if cache.is_some() { pal.cache } else { pal.muted },
            false,
        )),
        WidgetId::CacheRead => cache.map(|(_, read)| {
            let mut buf = String::from("↻ ");
            format_tokens(read, &mut buf);
            Segment::plain(buf, pal.cache, false)
        }),
        WidgetId::Speed => {
            let slow = speed.is_some_and(|s| s < cfg.speed_warn_threshold);
            let text = speed.map_or_else(|| "⚡ --".to_string(), |s| format!("⚡ {s:.0} tok/s"));
            let fg = match speed {
                Some(_) if slow => pal.speed_slow,
                Some(_) => pal.speed_ok,
                None => pal.muted,
            };
            Some(Segment { text, fg, accent_bg: slow.then_some(pal.speed_slow), bold: slow, is_spacer: false })
        }
        WidgetId::Ttft => match ctx.ttft_secs {
            Some(t) if t < 1.0 => Some(Segment::plain(format!("⏱ {:.0}ms", t * 1000.0), pal.muted, false)),
            Some(t) => Some(Segment::plain(
                format!("⏱ {t:.1}s"),
                if t >= 5.0 { pal.speed_slow } else { pal.muted },
                false,
            )),
            None if cfg.smart_hide => None,
            None => Some(Segment::plain("⏱ --".to_string(), pal.muted, false)),
        },
        WidgetId::Cost => {
            let cost = payload.cost.as_ref()?;
            let usd = cost.total_usd?;
            let prefix = if cost.estimated == Some(true) { "~$" } else { "$" };
            Some(Segment::plain(format!("{prefix}{usd:.2}"), pal.cost, false))
        }
        WidgetId::Artifacts => {
            let n = payload.artifact_count.unwrap_or(0);
            if cfg.smart_hide && n == 0 {
                return None;
            }
            Some(Segment::plain(format!("art {n}"), pal.muted, false))
        }
        WidgetId::Subagents => {
            let n = payload.subagent_count();
            if cfg.smart_hide && n == 0 {
                return None;
            }
            Some(Segment::plain(format!("sub {n}"), pal.muted, false))
        }
        WidgetId::Tasks => {
            let n = payload.task_count.unwrap_or(0);
            if cfg.smart_hide && n == 0 {
                return None;
            }
            Some(Segment::plain(format!("tasks {n}"), pal.muted, false))
        }
        WidgetId::Sandbox => {
            let on = payload.sandbox.as_ref().and_then(|s| s.enabled).unwrap_or(false);
            if cfg.smart_hide && !on {
                return None;
            }
            let text = if on { "sandbox ON" } else { "sandbox off" }.to_string();
            Some(Segment::plain(text, if on { pal.ready } else { pal.muted }, on))
        }
    }
}

fn fit_segments(segments: Vec<Segment>, max_width: Option<usize>, pad_per_seg: usize, sep_w: usize) -> Vec<Segment> {
    let Some(limit) = max_width.filter(|&w| w > 0) else {
        return segments;
    };
    let mut kept = Vec::with_capacity(segments.len());
    let mut used = 0usize;
    for (idx, seg) in segments.into_iter().enumerate() {
        let seg_w = seg.text.width() + pad_per_seg + if idx > 0 { sep_w } else { 0 };
        if idx > 0 && used + seg_w > limit {
            break;
        }
        used += seg_w;
        kept.push(seg);
    }
    kept
}

fn row_visible_width(segments: &[Segment], pad_per_seg: usize, sep_w: usize) -> usize {
    segments
        .iter()
        .enumerate()
        .map(|(i, s)| s.text.width() + pad_per_seg + if i > 0 { sep_w } else { 0 })
        .sum()
}

fn write_slice<W: IoWrite>(segments: &[Segment], cfg: &LayoutConfig, pal: &Palette, out: &mut W) -> io::Result<()> {
    if cfg.theme == ThemeStyle::Mono {
        let sep = cfg.separator.as_str();
        for (i, seg) in segments.iter().enumerate() {
            if i > 0 { write!(out, "{sep}")?; }
            write!(out, "{}", seg.text)?;
        }
        return Ok(());
    }

    match cfg.style {
        VisualStyle::Plain => {
            let sep = cfg.separator.as_str();
            for (i, seg) in segments.iter().enumerate() {
                if i > 0 { write!(out, "\x1b[38;5;{}m{sep}\x1b[0m", pal.sep)?; }
                let bold = if seg.bold { "\x1b[1m" } else { "" };
                write!(out, "{bold}\x1b[38;5;{}m{}\x1b[0m", seg.fg, seg.text)?;
            }
            Ok(())
        }
        VisualStyle::Pill => {
            for (i, seg) in segments.iter().enumerate() {
                if i > 0 { write!(out, " ")?; }
                let (bg, fg, bold) = match seg.accent_bg {
                    Some(abg) => (abg, pal.pl_accent_fg, "\x1b[1m"),
                    None => (pal.pill_bg, seg.fg, if seg.bold { "\x1b[1m" } else { "" }),
                };
                write!(out, "\x1b[48;5;{bg}m\x1b[38;5;{fg}m{bold} {} \x1b[0m", seg.text)?;
            }
            Ok(())
        }
        VisualStyle::Powerline => {
            let mut prev_bg: Option<u8> = None;
            for (i, seg) in segments.iter().enumerate() {
                let (bg, fg, bold) = match seg.accent_bg {
                    Some(accent) => (accent, pal.pl_accent_fg, true),
                    None => (if i % 2 == 0 { pal.pl_bg_a } else { pal.pl_bg_b }, seg.fg, seg.bold),
                };
                match prev_bg {
                    Some(p_bg) => write!(out, "\x1b[48;5;{bg}m\x1b[38;5;{p_bg}m\u{e0b0}")?,
                    None => write!(out, "\x1b[48;5;{bg}m")?,
                }
                let bold_s = if bold { "\x1b[1m" } else { "\x1b[22m" };
                write!(out, "{bold_s}\x1b[38;5;{fg}m {} ", seg.text)?;
                prev_bg = Some(bg);
            }
            if let Some(last_bg) = prev_bg {
                write!(out, "\x1b[0m\x1b[38;5;{last_bg}m\u{e0b0}\x1b[0m")?;
            }
            Ok(())
        }
    }
}

fn write_row<W: IoWrite>(
    segments: &[Segment],
    term_width: Option<usize>,
    pad_per_seg: usize,
    sep_w: usize,
    cfg: &LayoutConfig,
    pal: &Palette,
    out: &mut W,
) -> io::Result<()> {
    if let Some(pos) = segments.iter().position(|s| s.is_spacer) {
        let left = &segments[..pos];
        let right: Vec<&Segment> = segments[pos + 1..].iter().filter(|s| !s.is_spacer).collect();
        let right_owned: Vec<Segment> = right
            .into_iter()
            .map(|s| Segment { text: s.text.clone(), fg: s.fg, accent_bg: s.accent_bg, bold: s.bold, is_spacer: false })
            .collect();
        let left_w = row_visible_width(left, pad_per_seg, sep_w);
        let right_w = row_visible_width(&right_owned, pad_per_seg, sep_w);
        let gap = term_width
            .unwrap_or(0)
            .saturating_sub(left_w + right_w)
            .max(1);
        write_slice(left, cfg, pal, out)?;
        if !right_owned.is_empty() {
            write!(out, "{}", " ".repeat(gap))?;
            write_slice(&right_owned, cfg, pal, out)?;
        }
        return writeln!(out);
    }
    write_slice(segments, cfg, pal, out)?;
    writeln!(out)
}

/// Renders all non-empty configured rows in `cfg` for `payload`, `meta`, and `git`.
pub fn write_statusline<W: IoWrite>(
    payload: &StatuslinePayload,
    meta: &SessionMetadata,
    git: &GitInfo,
    cfg: &LayoutConfig,
    out: &mut W,
) -> io::Result<()> {
    let pal = Palette::for_theme(cfg.theme);
    let cache = valid_cache_usage(payload);
    let cur_ctx = current_context_tokens(payload);
    let eff_limit = meta
        .max_context_tokens
        .filter(|&v| v > 0)
        .or_else(|| {
            payload
                .context_window
                .as_ref()
                .and_then(|c| c.context_window_size)
                .filter(|&v| v > 0)
        });
    let used_pct = match (cur_ctx, eff_limit) {
        (Some(cur), Some(lim)) if lim > 0 => ((cur as f64) * 100.0 / (lim as f64)).clamp(0.0, 100.0),
        _ => payload
            .context_window
            .as_ref()
            .and_then(|c| c.used_percentage)
            .unwrap_or(0.0),
    };
    let speed = match cfg.speed_mode {
        SpeedMode::Weighted => meta.speed,
        SpeedMode::Instant => meta.last_turn_speed.or(meta.speed),
        SpeedMode::E2e => meta.e2e_speed.or(meta.speed),
    };
    let max_width = cfg.responsive.then_some(payload.terminal_width).flatten();
    let (pad_per_seg, sep_w) = match cfg.style {
        VisualStyle::Plain => (0, cfg.separator.as_str().width()),
        VisualStyle::Pill | VisualStyle::Powerline => (2, 1),
    };

    let ctx = RenderContext {
        cache,
        used_pct,
        cur_ctx,
        eff_limit,
        speed,
        ttft_secs: meta.ttft_secs,
        git,
    };

    for row in &cfg.rows {
        let segs: Vec<Segment> = row
            .iter()
            .filter_map(|&w| render_widget(w, payload, &ctx, cfg, &pal))
            .collect();
        if !segs.is_empty() {
            let fitted = fit_segments(segs, max_width, pad_per_seg, sep_w);
            write_row(&fitted, payload.terminal_width, pad_per_seg, sep_w, cfg, &pal, out)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::statusline::{ContextWindow, CostInfo, JsonUsage, ModelInfo, VcsInfo, WorkspaceInfo};

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
                branch: Some("main".to_string()),
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
        };
        let git = GitInfo {
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
        assert!(pl_text.contains("(512K)"));

        let mut narrow_cfg = LayoutConfig::preset_full();
        narrow_cfg.theme = ThemeStyle::Mono;
        let mut narrow_out = Vec::new();
        write_statusline(&payload, &meta, &git, &narrow_cfg, &mut narrow_out).unwrap();
        let narrow_text = String::from_utf8(narrow_out).unwrap();
        assert!(narrow_text.contains("THINKING"));
        assert!(!narrow_text.contains("sandbox"));
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
    fn spacer_aligns_left_and_right_and_smart_hide_hides_zeros() {
        let payload = StatuslinePayload {
            agent_state: Some("idle".to_string()),
            terminal_width: Some(40),
            ..Default::default()
        };
        let mut cfg = LayoutConfig::preset_minimal();
        cfg.theme = ThemeStyle::Mono;
        cfg.rows = vec![vec![WidgetId::State, WidgetId::Spacer, WidgetId::Tasks, WidgetId::Speed]];
        let mut out = Vec::new();
        write_statusline(&payload, &SessionMetadata::default(), &GitInfo::default(), &cfg, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains("tasks 0"));
        assert!(text.starts_with("● READY"));
        assert!(text.ends_with("⚡ --\n"));
        assert!(text.contains("        "));
    }

    #[test]
    fn model_omits_effort_when_display_name_already_includes_it() {
        let payload = StatuslinePayload {
            model: Some(ModelInfo {
                id: None,
                display_name: Some("Gemini 4 Hydrogen (Medium)".to_string()),
                effort: Some("medium".to_string()),
            }),
            ..Default::default()
        };
        let mut cfg = LayoutConfig::preset_minimal();
        cfg.theme = ThemeStyle::Mono;
        cfg.rows = vec![vec![WidgetId::Model]];
        let mut out = Vec::new();
        write_statusline(&payload, &SessionMetadata::default(), &GitInfo::default(), &cfg, &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "Gemini 4 Hydrogen (Medium)\n");
    }
}
