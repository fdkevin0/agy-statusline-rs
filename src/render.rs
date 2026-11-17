//! Multi-style, theme-aware statusline renderer (`Plain`, `Pill`, `Powerline`).

use std::io::{self, Write as IoWrite};

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::db::SessionMetadata;
use crate::format::format_tokens;
use crate::git::GitInfo;
use crate::layout::{GlyphMode, LayoutConfig, SpeedMode, ThemeStyle, VisualStyle, WidgetId};
use crate::metrics::{
    current_context_tokens, resolve_quota_5h, resolve_quota_weekly, valid_cache_usage, QuotaView,
};
use crate::statusline::StatuslinePayload;
use crate::theme::{GlyphSet, Palette};

#[derive(Debug, Clone)]
struct Segment {
    widget: WidgetId,
    text: String,
    fg: u8,
    accent_bg: Option<u8>,
    bold: bool,
    is_spacer: bool,
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
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct StyleGeometry {
    pad_per_seg: usize,
    sep_w: usize,
    slice_trail_w: usize,
    mono: bool,
    sep_str: &'static str,
}

fn compute_geometry(cfg: &LayoutConfig, glyphs: &GlyphSet) -> StyleGeometry {
    let mono = cfg.theme == ThemeStyle::Mono || cfg.no_color;
    let sep_str = if cfg.glyph_mode == GlyphMode::Ascii {
        cfg.separator.as_ascii_str()
    } else {
        cfg.separator.as_str()
    };
    if mono {
        return StyleGeometry {
            pad_per_seg: 0,
            sep_w: sep_str.width(),
            slice_trail_w: 0,
            mono: true,
            sep_str,
        };
    }
    match cfg.style {
        VisualStyle::Plain => StyleGeometry {
            pad_per_seg: 0,
            sep_w: sep_str.width(),
            slice_trail_w: 0,
            mono: false,
            sep_str,
        },
        VisualStyle::Pill => {
            let pad = match glyphs.pill_caps {
                Some((l, r)) => l.width() + r.width(),
                None => 2,
            };
            StyleGeometry {
                pad_per_seg: pad,
                sep_w: 1,
                slice_trail_w: 0,
                mono: false,
                sep_str,
            }
        }
        VisualStyle::Powerline => {
            let arrow_w = glyphs.pl_right.width();
            StyleGeometry {
                pad_per_seg: 2,
                sep_w: arrow_w,
                slice_trail_w: arrow_w,
                mono: false,
                sep_str,
            }
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

fn ctx_color(pct: f64, pal: &Palette) -> u8 {
    if pct >= 85.0 {
        pal.ctx_crit
    } else if pct >= 60.0 {
        pal.ctx_warn
    } else {
        pal.ctx_ok
    }
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
    quota_5h: Option<QuotaView>,
    quota_weekly: Option<QuotaView>,
    git: &'a GitInfo,
}

fn render_quota(
    widget: WidgetId,
    view: &Option<QuotaView>,
    fallback_label: &str,
    cfg: &LayoutConfig,
    pal: &Palette,
    glyphs: &GlyphSet,
) -> Option<Segment> {
    let Some(q) = view else {
        return (!cfg.smart_hide).then(|| {
            Segment::plain(widget, format!("{fallback_label} --"), pal.muted, false)
        });
    };
    let mut text = format!(
        "{} {}",
        q.label,
        crate::metrics::format_quota_pct(q.remaining_pct)
    );
    if let Some(reset) = &q.reset_compact {
        text.push(' ');
        text.push_str(glyphs.reset);
        text.push_str(reset);
    }
    let (fg, bold) = if q.remaining_pct <= 10.0 {
        (pal.ctx_crit, true)
    } else if q.remaining_pct <= 30.0 || q.pace_behind {
        (pal.ctx_warn, false)
    } else {
        (pal.ctx_ok, false)
    };
    Some(Segment::plain(widget, text, fg, bold))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EffortLevel {
    Low,
    Medium,
    High,
    XHigh,
    Extra,
    Minimal,
    Max,
}

fn canonical_effort(raw: &str) -> Option<EffortLevel> {
    let mut norm = String::with_capacity(raw.len());
    let mut prev_space = true;
    for ch in raw.trim().chars() {
        if ch == '_' || ch == '-' || ch.is_whitespace() {
            if !prev_space {
                norm.push(' ');
                prev_space = true;
            }
        } else {
            norm.push(ch.to_ascii_lowercase());
            prev_space = false;
        }
    }
    match norm.trim_end() {
        "low" => Some(EffortLevel::Low),
        "medium" => Some(EffortLevel::Medium),
        "high" => Some(EffortLevel::High),
        "xhigh" | "x high" | "extra high" => Some(EffortLevel::XHigh),
        "extra" => Some(EffortLevel::Extra),
        "minimal" => Some(EffortLevel::Minimal),
        "max" => Some(EffortLevel::Max),
        _ => None,
    }
}

fn trailing_display_effort(display_name: &str) -> Option<EffortLevel> {
    let trimmed = display_name.trim();
    if let Some(inner_start) = trimmed.rfind(['(', '（']) {
        let after = &trimmed[inner_start..];
        let mut chars = after.chars();
        let open = chars.next()?;
        let close = if open == '(' { ')' } else { '）' };
        if let Some(inner) = chars.as_str().strip_suffix(close) {
            if let Some(level) = canonical_effort(inner) {
                return Some(level);
            }
        }
    }
    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    if tokens.len() >= 3 {
        let two_word = format!("{} {}", tokens[tokens.len() - 2], tokens[tokens.len() - 1]);
        if let Some(level) = canonical_effort(&two_word) {
            return Some(level);
        }
    }
    if tokens.len() >= 2 {
        let last = tokens[tokens.len() - 1].trim_start_matches(['-', ':', '·', '/']);
        if let Some(level) = canonical_effort(last) {
            return Some(level);
        }
    }
    None
}

fn display_includes_effort(display_name: &str, effort: &str) -> bool {
    match canonical_effort(effort) {
        Some(target) => trailing_display_effort(display_name) == Some(target),
        None => {
            let trimmed = display_name.trim().to_ascii_lowercase();
            let lower_eff = effort.trim().to_ascii_lowercase();
            trimmed.ends_with(&format!("({lower_eff})"))
                || trimmed.ends_with(&format!(" {lower_eff}"))
        }
    }
}

fn ctx_display_metrics(used_pct: f64, ctx_remaining: bool) -> (&'static str, f64, f64) {
    let raw_pct = if used_pct.is_finite() {
        used_pct.max(0.0)
    } else {
        0.0
    };
    if ctx_remaining {
        ("rem", (100.0 - raw_pct).max(0.0), raw_pct)
    } else {
        ("ctx", raw_pct, raw_pct)
    }
}

fn render_widget(
    widget: WidgetId,
    payload: &StatuslinePayload,
    ctx: &RenderContext<'_>,
    cfg: &LayoutConfig,
    pal: &Palette,
    glyphs: &GlyphSet,
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
                "idle" => (format!("{} READY", glyphs.ready), pal.ready),
                "thinking" => (format!("{} THINKING", glyphs.thinking), pal.thinking),
                "working" => (format!("{} WORKING", glyphs.working), pal.working),
                "tool_use" => (format!("{} TOOL", glyphs.tool), pal.tool),
                "reviewing" => (format!("{} REVIEW", glyphs.review), pal.review),
                "authenticating" => (format!("{} AUTH", glyphs.thinking), pal.thinking),
                "initializing" => (format!("{} INIT", glyphs.thinking), pal.thinking),
                "error" => (format!("{} ERROR", glyphs.error), pal.error),
                other => (
                    format!("{} {}", glyphs.other, other.to_ascii_uppercase()),
                    pal.muted,
                ),
            };
            Some(Segment {
                widget,
                text,
                fg,
                accent_bg: Some(fg),
                bold: true,
                is_spacer: false,
            })
        }
        WidgetId::Model => {
            let model = payload.model.as_ref()?;
            let name = model.display_name.as_deref()?.trim();
            if name.is_empty() {
                return None;
            }
            let joiner = if cfg.glyph_mode == GlyphMode::Ascii {
                " / "
            } else {
                " · "
            };
            let text = match model
                .effort
                .as_deref()
                .map(str::trim)
                .filter(|e| !e.is_empty())
            {
                Some(effort) if !display_includes_effort(name, effort) => {
                    format!("{name}{joiner}{effort}")
                }
                _ => name.to_string(),
            };
            Some(Segment::plain(widget, text, pal.model, true))
        }
        WidgetId::Directory => Some(Segment::plain(
            widget,
            format!("{} {}", glyphs.dir, payload.workspace_basename()?),
            pal.dir,
            false,
        )),
        WidgetId::Vcs => {
            let vcs = payload.vcs.as_ref();
            let branch = vcs
                .and_then(|v| v.branch.as_deref())
                .map(str::trim)
                .filter(|b| !b.is_empty())
                .or_else(|| ctx.git.branch.as_deref().filter(|b| !b.is_empty()))?;
            let dirty = vcs.and_then(|v| v.dirty).unwrap_or(false)
                || ctx.git.added > 0
                || ctx.git.deleted > 0;
            let mut text = if dirty {
                format!("{} {branch}*", glyphs.branch)
            } else {
                format!("{} {branch}", glyphs.branch)
            };
            if cfg.show_git_diff && (ctx.git.added > 0 || ctx.git.deleted > 0) {
                text.push_str(&format!(" +{} -{}", ctx.git.added, ctx.git.deleted));
            }
            Some(Segment::plain(
                widget,
                text,
                if dirty { pal.vcs_dirty } else { pal.vcs_clean },
                dirty,
            ))
        }
        WidgetId::Worktree => match &ctx.git.worktree {
            Some(wt) => {
                let text = if cfg.glyph_mode == GlyphMode::Ascii {
                    format!("wt:{wt}")
                } else {
                    format!("{} wt:{wt}", glyphs.branch)
                };
                Some(Segment::plain(widget, text, pal.dir, true))
            }
            None if cfg.smart_hide => None,
            None => Some(Segment::plain(
                widget,
                "wt:main".to_string(),
                pal.muted,
                false,
            )),
        },
        WidgetId::CtxBar => {
            let (bar, _) =
                build_progress_bar_with_chars(used_pct, cfg.bar_length, glyphs.bar_chars);
            let (tag, disp_pct, raw_pct) = ctx_display_metrics(used_pct, cfg.ctx_remaining);
            let text = match eff_limit {
                Some(lim) => format!("{tag} {bar} {disp_pct:.1}% ({})", format_tok_str(lim)),
                None => format!("{tag} {bar} {disp_pct:.1}%"),
            };
            Some(Segment::plain(
                widget,
                text,
                ctx_color(raw_pct, pal),
                raw_pct >= 85.0,
            ))
        }
        WidgetId::CtxPct => {
            let (tag, disp_pct, raw_pct) = ctx_display_metrics(used_pct, cfg.ctx_remaining);
            let text = match eff_limit {
                Some(lim) => format!("{tag} {disp_pct:.1}% ({})", format_tok_str(lim)),
                None => format!("{tag} {disp_pct:.1}%"),
            };
            Some(Segment::plain(widget, text, ctx_color(raw_pct, pal), false))
        }
        WidgetId::CtxTokens => {
            if let (Some(cur), Some(lim)) = (cur_ctx, eff_limit) {
                return Some(Segment::plain(
                    widget,
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
            Some(Segment::plain(
                widget,
                format!("{} tok", format_tok_str(total)),
                pal.muted,
                false,
            ))
        }
        WidgetId::CacheHit => {
            let text = if glyphs.cache == "cache" {
                cache.map_or_else(
                    || "cache --".to_string(),
                    |(pct, _)| format!("cache {pct:.1}%"),
                )
            } else {
                cache.map_or_else(
                    || format!("{} cache --", glyphs.cache),
                    |(pct, _)| format!("{} cache {pct:.1}%", glyphs.cache),
                )
            };
            Some(Segment::plain(
                widget,
                text,
                if cache.is_some() { pal.cache } else { pal.muted },
                false,
            ))
        }
        WidgetId::CacheRead => cache.map(|(_, read)| {
            let mut buf = format!("{} ", glyphs.reset);
            format_tokens(read, &mut buf);
            Segment::plain(widget, buf, pal.cache, false)
        }),
        WidgetId::Speed => {
            let slow = speed.is_some_and(|s| s < cfg.speed_warn_threshold);
            let text = speed.map_or_else(
                || format!("{} --", glyphs.speed),
                |s| format!("{} {s:.0} tok/s", glyphs.speed),
            );
            let fg = match speed {
                Some(_) if slow => pal.speed_slow,
                Some(_) => pal.speed_ok,
                None => pal.muted,
            };
            Some(Segment {
                widget,
                text,
                fg,
                accent_bg: slow.then_some(pal.speed_slow),
                bold: slow,
                is_spacer: false,
            })
        }
        WidgetId::Ttft => match ctx.ttft_secs {
            Some(t) if t < 1.0 => Some(Segment::plain(
                widget,
                format!("{} {:.0}ms", glyphs.ttft, t * 1000.0),
                pal.muted,
                false,
            )),
            Some(t) => Some(Segment::plain(
                widget,
                format!("{} {t:.1}s", glyphs.ttft),
                if t >= 5.0 { pal.speed_slow } else { pal.muted },
                false,
            )),
            None if cfg.smart_hide => None,
            None => Some(Segment::plain(
                widget,
                format!("{} --", glyphs.ttft),
                pal.muted,
                false,
            )),
        },
        WidgetId::Cost => {
            let cost = payload.cost.as_ref()?;
            let usd = cost.total_usd?;
            let prefix = if cost.estimated == Some(true) {
                "~$"
            } else {
                "$"
            };
            let mut text = format!("{prefix}{usd:.2}");
            if let Some(sub) = cost.subagent_usd.filter(|&s| s >= 0.01) {
                text.push_str(&format!(" (sub ${sub:.2})"));
            }
            Some(Segment::plain(widget, text, pal.cost, false))
        }
        WidgetId::Mode => {
            let mode = payload
                .cycle_mode
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())?;
            let mut text = format!("{} {}", glyphs.mode, mode.to_ascii_uppercase());
            if payload.exceeds_200k_tokens == Some(true) {
                text.push_str(" >200K");
            }
            let fg = match mode.to_ascii_lowercase().as_str() {
                "plan" => pal.thinking,
                "auto" => pal.ready,
                _ => pal.model,
            };
            Some(Segment::plain(widget, text, fg, true))
        }
        WidgetId::Quota5h => render_quota(widget, &ctx.quota_5h, "5h", cfg, pal, glyphs),
        WidgetId::QuotaWeekly => render_quota(widget, &ctx.quota_weekly, "7d", cfg, pal, glyphs),
        WidgetId::Artifacts => {
            let n = payload.artifact_count.unwrap_or(0);
            if cfg.smart_hide && n == 0 {
                return None;
            }
            Some(Segment::plain(widget, format!("art {n}"), pal.muted, false))
        }
        WidgetId::Subagents => {
            let n = payload.subagent_count();
            if cfg.smart_hide && n == 0 {
                return None;
            }
            Some(Segment::plain(widget, format!("sub {n}"), pal.muted, false))
        }
        WidgetId::Tasks => {
            let n = payload.task_count.unwrap_or(0);
            if cfg.smart_hide && n == 0 {
                return None;
            }
            Some(Segment::plain(
                widget,
                format!("tasks {n}"),
                pal.muted,
                false,
            ))
        }
        WidgetId::Sandbox => {
            let on = payload
                .sandbox
                .as_ref()
                .and_then(|s| s.enabled)
                .unwrap_or(false);
            if cfg.smart_hide && !on {
                return None;
            }
            let text = if on { "sandbox ON" } else { "sandbox off" }.to_string();
            Some(Segment::plain(
                widget,
                text,
                if on { pal.ready } else { pal.muted },
                on,
            ))
        }
    }
}

fn slice_visible_width(segments: &[Segment], geom: StyleGeometry) -> usize {
    if segments.is_empty() {
        return 0;
    }
    let body: usize = segments
        .iter()
        .enumerate()
        .map(|(i, s)| s.text.width() + geom.pad_per_seg + if i > 0 { geom.sep_w } else { 0 })
        .sum();
    body + geom.slice_trail_w
}

fn groups_visible_width(groups: &[Vec<Segment>], geom: StyleGeometry) -> usize {
    if groups.is_empty() {
        return 0;
    }
    let sum: usize = groups.iter().map(|g| slice_visible_width(g, geom)).sum();
    sum + groups.len().saturating_sub(1)
}

fn truncate_to_width(s: &str, max_cols: usize, ellipsis: &str) -> String {
    if s.width() <= max_cols {
        return s.to_string();
    }
    let ell_w = ellipsis.width();
    if max_cols == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0usize;
    let budget = if max_cols > ell_w {
        max_cols - ell_w
    } else {
        max_cols
    };
    for ch in s.chars() {
        let w = ch.width().unwrap_or(0);
        if used + w > budget {
            break;
        }
        out.push(ch);
        used += w;
    }
    if max_cols > ell_w {
        out.push_str(ellipsis);
    }
    out
}

fn split_into_groups(segs: Vec<Segment>) -> Vec<Vec<Segment>> {
    let mut groups: Vec<Vec<Segment>> = Vec::new();
    let mut cur = Vec::new();
    for seg in segs {
        if seg.is_spacer {
            if !cur.is_empty() {
                groups.push(cur);
                cur = Vec::new();
            }
        } else {
            cur.push(seg);
        }
    }
    if !cur.is_empty() {
        groups.push(cur);
    }
    while groups.len() > 3 {
        let tail = groups.remove(3);
        groups[2].extend(tail);
    }
    groups
}

fn fit_row_groups(
    mut groups: Vec<Vec<Segment>>,
    max_width: Option<usize>,
    geom: StyleGeometry,
    ellipsis: &str,
) -> Vec<Vec<Segment>> {
    let Some(limit) = max_width.filter(|&w| w > 0) else {
        return groups;
    };
    if groups_visible_width(&groups, geom) <= limit {
        return groups;
    }

    for g in &mut groups {
        for seg in g {
            if seg.widget == WidgetId::Vcs && seg.text.width() > 24 {
                seg.text = truncate_to_width(&seg.text, 24, ellipsis);
            }
        }
    }

    while groups_visible_width(&groups, geom) > limit {
        let total_segs: usize = groups.iter().map(Vec::len).sum();
        if total_segs <= 1 {
            break;
        }
        let mut drop_pos: Option<(usize, usize, u8)> = None;
        for (gi, g) in groups.iter().enumerate() {
            for (si, seg) in g.iter().enumerate() {
                let p = seg.widget.priority();
                let replace = match drop_pos {
                    None => true,
                    Some((_, _, best_p)) => p <= best_p,
                };
                if replace {
                    drop_pos = Some((gi, si, p));
                }
            }
        }
        if let Some((gi, si, _)) = drop_pos {
            groups[gi].remove(si);
            if groups[gi].is_empty() {
                groups.remove(gi);
            }
        } else {
            break;
        }
    }

    if groups_visible_width(&groups, geom) > limit && groups.len() == 1 && groups[0].len() == 1 {
        let overhead = geom.pad_per_seg + geom.slice_trail_w;
        let text_budget = limit.saturating_sub(overhead);
        if text_budget == 0 {
            groups.clear();
        } else {
            groups[0][0].text = truncate_to_width(&groups[0][0].text, text_budget, ellipsis);
        }
    }

    groups
}

fn write_slice<W: IoWrite>(
    segments: &[Segment],
    cfg: &LayoutConfig,
    geom: StyleGeometry,
    pal: &Palette,
    glyphs: &GlyphSet,
    out: &mut W,
) -> io::Result<()> {
    if geom.mono {
        for (i, seg) in segments.iter().enumerate() {
            if i > 0 {
                write!(out, "{}", geom.sep_str)?;
            }
            write!(out, "{}", seg.text)?;
        }
        return Ok(());
    }

    match cfg.style {
        VisualStyle::Plain => {
            for (i, seg) in segments.iter().enumerate() {
                if i > 0 {
                    write!(out, "\x1b[38;5;{}m{}\x1b[0m", pal.sep, geom.sep_str)?;
                }
                let bold = if seg.bold { "\x1b[1m" } else { "" };
                write!(out, "{bold}\x1b[38;5;{}m{}\x1b[0m", seg.fg, seg.text)?;
            }
            Ok(())
        }
        VisualStyle::Pill => {
            for (i, seg) in segments.iter().enumerate() {
                if i > 0 {
                    write!(out, " ")?;
                }
                let (bg, fg, bold) = match seg.accent_bg {
                    Some(abg) => (abg, pal.pl_accent_fg, "\x1b[1m"),
                    None => (pal.pill_bg, seg.fg, if seg.bold { "\x1b[1m" } else { "" }),
                };
                if let Some((cap_l, cap_r)) = glyphs.pill_caps {
                    write!(
                        out,
                        "\x1b[38;5;{bg}m{cap_l}\x1b[48;5;{bg}m\x1b[38;5;{fg}m{bold}{}\x1b[0m\x1b[38;5;{bg}m{cap_r}\x1b[0m",
                        seg.text
                    )?;
                } else {
                    write!(out, "\x1b[48;5;{bg}m\x1b[38;5;{fg}m{bold} {} \x1b[0m", seg.text)?;
                }
            }
            Ok(())
        }
        VisualStyle::Powerline => {
            let mut prev_bg: Option<u8> = None;
            for (i, seg) in segments.iter().enumerate() {
                let (bg, fg, bold) = match seg.accent_bg {
                    Some(accent) => (accent, pal.pl_accent_fg, true),
                    None => (
                        if i % 2 == 0 { pal.pl_bg_a } else { pal.pl_bg_b },
                        seg.fg,
                        seg.bold,
                    ),
                };
                match prev_bg {
                    Some(p_bg) => {
                        write!(out, "\x1b[48;5;{bg}m\x1b[38;5;{p_bg}m{}", glyphs.pl_right)?;
                    }
                    None => write!(out, "\x1b[48;5;{bg}m")?,
                }
                let bold_s = if bold { "\x1b[1m" } else { "\x1b[22m" };
                write!(out, "{bold_s}\x1b[38;5;{fg}m {} ", seg.text)?;
                prev_bg = Some(bg);
            }
            if let Some(last_bg) = prev_bg {
                write!(
                    out,
                    "\x1b[0m\x1b[38;5;{last_bg}m{}\x1b[0m",
                    glyphs.pl_right
                )?;
            }
            Ok(())
        }
    }
}

fn write_row_groups<W: IoWrite>(
    groups: &[Vec<Segment>],
    term_width: Option<usize>,
    geom: StyleGeometry,
    cfg: &LayoutConfig,
    pal: &Palette,
    glyphs: &GlyphSet,
    out: &mut W,
) -> io::Result<()> {
    match groups {
        [] => Ok(()),
        [only] => {
            write_slice(only, cfg, geom, pal, glyphs, out)?;
            writeln!(out)
        }
        [left, right] => {
            let w0 = slice_visible_width(left, geom);
            let w1 = slice_visible_width(right, geom);
            let gap = term_width.unwrap_or(0).saturating_sub(w0 + w1).max(1);
            write_slice(left, cfg, geom, pal, glyphs, out)?;
            write!(out, "{}", " ".repeat(gap))?;
            write_slice(right, cfg, geom, pal, glyphs, out)?;
            writeln!(out)
        }
        [left, center, right, ..] => {
            let w0 = slice_visible_width(left, geom);
            let w1 = slice_visible_width(center, geom);
            let w2 = slice_visible_width(right, geom);
            let rem = term_width.unwrap_or(0).saturating_sub(w0 + w1 + w2);
            let gap1 = (rem / 2).max(1);
            let gap2 = rem.saturating_sub(gap1).max(1);
            write_slice(left, cfg, geom, pal, glyphs, out)?;
            write!(out, "{}", " ".repeat(gap1))?;
            write_slice(center, cfg, geom, pal, glyphs, out)?;
            write!(out, "{}", " ".repeat(gap2))?;
            write_slice(right, cfg, geom, pal, glyphs, out)?;
            writeln!(out)
        }
    }
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
    let glyphs = GlyphSet::for_mode(cfg.glyph_mode);
    let geom = compute_geometry(cfg, &glyphs);
    let cache = valid_cache_usage(payload);
    let cur_ctx = current_context_tokens(payload)
        .or_else(|| meta.estimated_context_tokens.filter(|&v| v > 0));
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
    let max_width = cfg.responsive.then_some(payload.terminal_width).flatten();

    let ctx = RenderContext {
        cache,
        used_pct,
        cur_ctx,
        eff_limit,
        speed,
        ttft_secs: meta.ttft_secs,
        quota_5h: resolve_quota_5h(payload),
        quota_weekly: resolve_quota_weekly(payload),
        git,
    };

    for row in &cfg.rows {
        let segs: Vec<Segment> = row
            .iter()
            .filter_map(|&w| render_widget(w, payload, &ctx, cfg, &pal, &glyphs))
            .collect();
        let groups = split_into_groups(segs);
        let fitted = fit_row_groups(groups, max_width, geom, glyphs.ellipsis);
        write_row_groups(
            &fitted,
            payload.terminal_width,
            geom,
            cfg,
            &pal,
            &glyphs,
            out,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::statusline::{
        ContextWindow, CostInfo, JsonUsage, ModelInfo, QuotaBucket, VcsInfo, WorkspaceInfo,
    };
    use std::collections::BTreeMap;

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
        write_statusline(
            &payload,
            &SessionMetadata::default(),
            &GitInfo::default(),
            &cfg,
            &mut out,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "Gemini 4 Hydrogen (Medium)\n"
        );
    }

    #[test]
    fn renders_mode_quota_subagent_cost_and_ascii_no_color() {
        let mut quota = BTreeMap::new();
        quota.insert(
            "gemini-5h".to_string(),
            QuotaBucket {
                remaining_fraction: Some(0.84),
                reset_in_seconds: Some(11_400),
                reset_time: None,
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
            "@ REVIEW * mode: PLAN >200K * $0.45 (sub $0.12) * 5h 84% r:3h10m\n"
        );
    }

    #[test]
    fn model_canonical_effort_matching_avoids_false_positives_and_duplicates() {
        let mut cfg = LayoutConfig::preset_minimal();
        cfg.theme = ThemeStyle::Mono;
        cfg.rows = vec![vec![WidgetId::Model]];

        let gpt_xhigh = StatuslinePayload {
            model: Some(ModelInfo {
                id: None,
                display_name: Some("GPT-OSS 120B (Extra High)".to_string()),
                effort: Some("xhigh".to_string()),
            }),
            ..Default::default()
        };
        let mut out1 = Vec::new();
        write_statusline(
            &gpt_xhigh,
            &SessionMetadata::default(),
            &GitInfo::default(),
            &cfg,
            &mut out1,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(out1).unwrap(),
            "GPT-OSS 120B (Extra High)\n"
        );

        let minimax = StatuslinePayload {
            model: Some(ModelInfo {
                id: None,
                display_name: Some("MiniMax M2".to_string()),
                effort: Some("max".to_string()),
            }),
            ..Default::default()
        };
        let mut out2 = Vec::new();
        write_statusline(
            &minimax,
            &SessionMetadata::default(),
            &GitInfo::default(),
            &cfg,
            &mut out2,
        )
        .unwrap();
        assert_eq!(String::from_utf8(out2).unwrap(), "MiniMax M2 · max\n");

        let gpt_high_vs_xhigh = StatuslinePayload {
            model: Some(ModelInfo {
                id: None,
                display_name: Some("GPT-OSS 120B (Extra High)".to_string()),
                effort: Some("high".to_string()),
            }),
            ..Default::default()
        };
        let mut out3 = Vec::new();
        write_statusline(
            &gpt_high_vs_xhigh,
            &SessionMetadata::default(),
            &GitInfo::default(),
            &cfg,
            &mut out3,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(out3).unwrap(),
            "GPT-OSS 120B (Extra High) · high\n"
        );

        let claude_min = StatuslinePayload {
            model: Some(ModelInfo {
                id: None,
                display_name: Some("Claude Sonnet Minimal".to_string()),
                effort: Some("minimal".to_string()),
            }),
            ..Default::default()
        };
        let mut out4 = Vec::new();
        write_statusline(
            &claude_min,
            &SessionMetadata::default(),
            &GitInfo::default(),
            &cfg,
            &mut out4,
        )
        .unwrap();
        assert_eq!(String::from_utf8(out4).unwrap(), "Claude Sonnet Minimal\n");
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
            "ctx ████████ 104.7% (512K) · ctx 104.7% (512K)\n"
        );

        let mut cfg_rem = LayoutConfig::preset_minimal();
        cfg_rem.theme = ThemeStyle::Default;
        cfg_rem.ctx_remaining = true;
        cfg_rem.bar_length = 8;
        cfg_rem.rows = vec![vec![WidgetId::CtxBar, WidgetId::CtxPct]];
        let mut out_rem = Vec::new();
        write_statusline(&payload, &meta, &GitInfo::default(), &cfg_rem, &mut out_rem).unwrap();
        let rem_text = String::from_utf8(out_rem).unwrap();
        let crit_code = format!("\x1b[38;5;{}m", Palette::for_theme(ThemeStyle::Default).ctx_crit);
        assert!(rem_text.contains(&crit_code));
        assert!(rem_text.contains("rem ████████ 0.0% (512K)"));
        assert!(rem_text.contains("rem 0.0% (512K)"));
    }
}
