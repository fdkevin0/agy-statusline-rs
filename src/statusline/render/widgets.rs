//! Domain-grouped widget renderers and inline text sanitization.

use std::path::Path;

use unicode_width::UnicodeWidthStr;

use crate::agy::db::RetryAnomalySummary;
use crate::agy::protobuf::RetryStatusClass;
use crate::config::layout::{GlyphMode, WidgetId};
use crate::statusline::metrics::{classify_credit_severity, CreditSeverity, CreditView, QuotaView};
use crate::statusline::theme::Palette;
use crate::statusline::{
    single_active_subagent_label, subagent_count, workspace_basename, StatuslinePayload,
};
use crate::sys::fmt::{
    format_duration_compact, format_tokens, format_tokens_string as format_tok_str,
    sanitize_inline_text, truncate_to_width,
};

const CACHE_HIT_MIN_CONTEXT_TOKENS: u64 = 10_000;

use super::{
    build_progress_bar_with_chars, CompactStage, RenderContext, Renderer, ResolvedCost, Segment,
};

impl Renderer<'_> {
    pub(super) fn render_widget(
        &self,
        widget: WidgetId,
        payload: &StatuslinePayload,
        ctx: &RenderContext<'_>,
    ) -> Option<Segment> {
        match widget {
            WidgetId::Spacer => Some(Segment::spacer()),
            WidgetId::State
            | WidgetId::Model
            | WidgetId::Mode
            | WidgetId::Version
            | WidgetId::Plan
            | WidgetId::Account
            | WidgetId::Title
            | WidgetId::Agent
            | WidgetId::Product
            | WidgetId::Session
            | WidgetId::Transcript => self.render_session_widget(widget, payload, ctx),
            WidgetId::Directory | WidgetId::Vcs | WidgetId::Worktree => {
                self.render_vcs_widget(widget, payload, ctx)
            }
            WidgetId::CtxBar
            | WidgetId::CtxPct
            | WidgetId::CtxTokens
            | WidgetId::CacheHit
            | WidgetId::CacheRead
            | WidgetId::Speed
            | WidgetId::Ttft
            | WidgetId::Thinking
            | WidgetId::Cost => self.render_usage_widget(widget, payload, ctx),
            WidgetId::Quota5h => self.render_quota_widget(widget, &ctx.quota_5h, "5h", ctx),
            WidgetId::QuotaWeekly => self.render_quota_widget(widget, &ctx.quota_weekly, "7d", ctx),
            WidgetId::Artifacts
            | WidgetId::Subagents
            | WidgetId::Tasks
            | WidgetId::Queue
            | WidgetId::Vim
            | WidgetId::Battle
            | WidgetId::Sandbox => self.render_telemetry_widget(widget, payload),
        }
    }

    fn render_session_widget(
        &self,
        widget: WidgetId,
        payload: &StatuslinePayload,
        ctx: &RenderContext<'_>,
    ) -> Option<Segment> {
        let pal = self.pal;
        let glyphs = self.glyphs;

        match widget {
            WidgetId::State => {
                if payload.tool_confirmation_pending == Some(true) {
                    return Some(Segment::accent(
                        widget,
                        format!("{} CONFIRM", glyphs.review),
                        pal.ctx_warn,
                    ));
                }
                let state_owned = payload
                    .agent_state
                    .as_deref()
                    .map(sanitize_inline_text)
                    .filter(|s| !s.is_empty());
                let state = state_owned.as_deref().unwrap_or("idle");
                let (text, fg) = match state {
                    "idle" => (format!("{} READY", glyphs.ready), pal.ready),
                    "thinking" => (format!("{} THINKING", glyphs.thinking), pal.thinking),
                    "working" => (format!("{} WORKING", glyphs.working), pal.working),
                    "tool_use" => (format!("{} TOOL", glyphs.tool), pal.tool),
                    "reviewing" => (format!("{} REVIEW", glyphs.review), pal.review),
                    "authenticating" => (format!("{} AUTH", glyphs.thinking), pal.thinking),
                    "initializing" => (format!("{} INIT", glyphs.thinking), pal.thinking),
                    "completed" => (format!("{} DONE", glyphs.ready), pal.ready),
                    "error" => (format!("{} ERROR", glyphs.error), pal.error),
                    other => (
                        format!("{} {}", glyphs.other, other.to_ascii_uppercase()),
                        pal.muted,
                    ),
                };
                Some(Segment::accent(widget, text, fg))
            }
            WidgetId::Model => {
                let model = payload.model.as_ref()?;
                let name = sanitize_inline_text(model.display_name.as_deref()?);
                if name.is_empty() {
                    return None;
                }
                let joiner = if self.cfg.glyph_mode == GlyphMode::Ascii {
                    " / "
                } else {
                    " · "
                };
                let full = match model
                    .effort
                    .as_deref()
                    .map(sanitize_inline_text)
                    .filter(|e| !e.is_empty())
                {
                    Some(effort) if !display_includes_effort(&name, &effort) => {
                        format!("{name}{joiner}{effort}")
                    }
                    _ => name,
                };
                let abbrev = abbreviate_leading_word(&full);
                let mut seg = Segment::plain(widget, full.clone(), pal.model, true);
                if abbrev != full {
                    seg.push_fallback(CompactStage::ModelAbbreviate, abbrev.clone());
                }
                if abbrev.width() > 20 {
                    seg.push_fallback(
                        CompactStage::ModelTruncate,
                        truncate_to_width(&abbrev, 20, glyphs.ellipsis),
                    );
                }
                Some(seg)
            }
            WidgetId::Mode => {
                let mode = payload
                    .cycle_mode
                    .as_deref()
                    .map(sanitize_inline_text)
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
            WidgetId::Version => {
                let ver = payload
                    .version
                    .as_deref()
                    .map(sanitize_inline_text)
                    .filter(|s| !s.is_empty())?;
                let text = if ver.starts_with('v') || ver.starts_with('V') {
                    ver
                } else {
                    format!("v{ver}")
                };
                Some(Segment::plain(widget, text, pal.muted, false))
            }
            WidgetId::Plan => {
                let tier = ctx
                    .plan_tier_override
                    .or(payload.plan_tier.as_deref())
                    .map(sanitize_inline_text)
                    .filter(|s| !s.is_empty())?;
                Some(Segment::plain(widget, tier, pal.model, true))
            }
            WidgetId::Account => {
                let email = payload
                    .email
                    .as_deref()
                    .map(sanitize_inline_text)
                    .filter(|s| !s.is_empty())?;
                let mut seg = Segment::plain(widget, email.clone(), pal.muted, false);
                if let Some((local, _)) = email.split_once('@') {
                    if !local.is_empty() {
                        seg.push_fallback(CompactStage::AccountDropDomain, local.to_string());
                    }
                }
                Some(seg)
            }
            WidgetId::Title => {
                let raw = payload
                    .conversation_title
                    .as_deref()
                    .map(sanitize_inline_text)
                    .filter(|s| !s.is_empty())?;
                let text = truncate_to_width(&raw, 32, glyphs.ellipsis);
                let mut seg = Segment::plain(widget, text.clone(), pal.dir, false);
                if text.width() > 16 {
                    seg.push_fallback(
                        CompactStage::TitleTruncate,
                        truncate_to_width(&text, 16, glyphs.ellipsis),
                    );
                }
                Some(seg)
            }
            WidgetId::Agent => {
                let name = payload
                    .agent
                    .as_ref()
                    .and_then(|a| a.name.as_deref())
                    .map(sanitize_inline_text)
                    .filter(|s| !s.is_empty())?;
                Some(Segment::plain(
                    widget,
                    format!("agent {name}"),
                    pal.tool,
                    true,
                ))
            }
            WidgetId::Product => {
                let prod = payload
                    .product
                    .as_deref()
                    .map(sanitize_inline_text)
                    .filter(|s| !s.is_empty())?;
                Some(Segment::plain(widget, prod, pal.muted, false))
            }
            WidgetId::Session => {
                let sid = payload
                    .session_id
                    .as_deref()
                    .map(sanitize_inline_text)
                    .filter(|s| !s.is_empty())?;
                let short: String = sid.chars().take(8).collect();
                Some(Segment::plain(
                    widget,
                    format!("sid:{short}"),
                    pal.muted,
                    false,
                ))
            }
            WidgetId::Transcript => {
                let raw = payload
                    .transcript_path
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())?;
                let name = sanitize_inline_text(
                    Path::new(raw)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .filter(|s| !s.is_empty())
                        .unwrap_or(raw),
                );
                if name.is_empty() {
                    return None;
                }
                Some(Segment::plain(
                    widget,
                    format!("log:{name}"),
                    pal.muted,
                    false,
                ))
            }
            _ => None,
        }
    }

    fn render_vcs_widget(
        &self,
        widget: WidgetId,
        payload: &StatuslinePayload,
        ctx: &RenderContext<'_>,
    ) -> Option<Segment> {
        let pal = self.pal;
        let glyphs = self.glyphs;

        match widget {
            WidgetId::Directory => {
                let dir = sanitize_inline_text(workspace_basename(payload)?);
                if dir.is_empty() {
                    return None;
                }
                Some(Segment::plain(
                    widget,
                    format!("{} {dir}", glyphs.dir),
                    pal.dir,
                    false,
                ))
            }
            WidgetId::Vcs => {
                let vcs = payload.vcs.as_ref();
                let branch = vcs
                    .and_then(|v| {
                        v.branch
                            .as_deref()
                            .map(sanitize_inline_text)
                            .filter(|b| !b.is_empty())
                            .or_else(|| {
                                v.client
                                    .as_deref()
                                    .map(sanitize_inline_text)
                                    .filter(|c| !c.is_empty())
                            })
                    })
                    .or_else(|| {
                        ctx.git
                            .branch
                            .as_deref()
                            .map(sanitize_inline_text)
                            .filter(|b| !b.is_empty())
                    })?;
                let dirty = vcs.and_then(|v| v.dirty).unwrap_or(false)
                    || ctx.git.added > 0
                    || ctx.git.deleted > 0;
                let prefix = if self.cfg.glyph_mode == GlyphMode::Ascii {
                    vcs.and_then(|v| v.vcs_type.as_deref())
                        .map(sanitize_inline_text)
                        .filter(|t| !t.is_empty())
                        .map(|t| format!("{t}:"))
                        .unwrap_or_else(|| glyphs.branch.to_string())
                } else {
                    glyphs.branch.to_string()
                };
                let base = if dirty {
                    format!("{prefix} {branch}*")
                } else {
                    format!("{prefix} {branch}")
                };
                let has_diff = self.cfg.show_git_diff && (ctx.git.added > 0 || ctx.git.deleted > 0);
                let full = if has_diff {
                    format!("{base} +{} -{}", ctx.git.added, ctx.git.deleted)
                } else {
                    base.clone()
                };
                let mut seg = Segment::plain(
                    widget,
                    full,
                    if dirty { pal.vcs_dirty } else { pal.vcs_clean },
                    dirty,
                );
                if has_diff {
                    seg.push_fallback(CompactStage::VcsDropDiff, base.clone());
                }
                if base.width() > 20 {
                    seg.push_fallback(
                        CompactStage::VcsTruncate,
                        truncate_to_width(&base, 20, glyphs.ellipsis),
                    );
                }
                Some(seg)
            }
            WidgetId::Worktree => match ctx
                .git
                .worktree
                .as_deref()
                .map(sanitize_inline_text)
                .filter(|s| !s.is_empty())
            {
                Some(wt) => {
                    let text = if self.cfg.glyph_mode == GlyphMode::Ascii {
                        format!("wt:{wt}")
                    } else {
                        format!("{} wt:{wt}", glyphs.branch)
                    };
                    Some(Segment::plain(widget, text, pal.dir, true))
                }
                None if self.cfg.smart_hide => None,
                None => Some(Segment::plain(
                    widget,
                    "wt:main".to_string(),
                    pal.muted,
                    false,
                )),
            },
            _ => None,
        }
    }

    fn render_usage_widget(
        &self,
        widget: WidgetId,
        payload: &StatuslinePayload,
        ctx: &RenderContext<'_>,
    ) -> Option<Segment> {
        let pal = self.pal;
        let glyphs = self.glyphs;

        match widget {
            WidgetId::CtxBar => {
                let bar = build_progress_bar_with_chars(
                    ctx.used_pct,
                    self.cfg.bar_length,
                    glyphs.bar_chars,
                );
                let (tag, disp_pct) = ctx_display_metrics(ctx.used_pct, self.cfg.ctx_remaining);
                let compact_pct = format!("{tag} {disp_pct:.1}%");
                let without_limit = format!("{tag} {bar} {disp_pct:.1}%");
                let full = match (ctx.cur_ctx, ctx.eff_limit) {
                    (Some(cur), Some(lim)) => {
                        let shown = if self.cfg.ctx_remaining {
                            lim.saturating_sub(cur)
                        } else {
                            cur
                        };
                        format!(
                            "{without_limit} ({}/{})",
                            format_tok_str(shown),
                            format_tok_str(lim)
                        )
                    }
                    (None, Some(lim)) => format!("{without_limit} ({})", format_tok_str(lim)),
                    (_, None) => without_limit.clone(),
                };
                let mut seg = Segment::plain(
                    widget,
                    full,
                    ctx_color(ctx.used_pct, pal),
                    ctx.used_pct >= 85.0,
                );
                if ctx.eff_limit.is_some() {
                    seg.push_fallback(CompactStage::CtxDropLimit, without_limit);
                }
                seg.push_fallback(CompactStage::CtxCollapseBar, compact_pct);
                Some(seg)
            }
            WidgetId::CtxPct => {
                let (tag, disp_pct) = ctx_display_metrics(ctx.used_pct, self.cfg.ctx_remaining);
                let without_limit = format!("{tag} {disp_pct:.1}%");
                let full = match ctx.eff_limit {
                    Some(lim) => format!("{without_limit} ({})", format_tok_str(lim)),
                    None => without_limit.clone(),
                };
                let mut seg = Segment::plain(widget, full, ctx_color(ctx.used_pct, pal), false);
                if ctx.eff_limit.is_some() {
                    seg.push_fallback(CompactStage::CtxDropLimit, without_limit);
                }
                Some(seg)
            }
            WidgetId::CtxTokens => {
                if let (Some(cur), Some(lim)) = (ctx.cur_ctx, ctx.eff_limit) {
                    return Some(Segment::plain(
                        widget,
                        format!("{}/{}", format_tok_str(cur), format_tok_str(lim)),
                        pal.muted,
                        false,
                    ));
                }
                let cw = payload.context_window.as_ref()?;
                let total = cw
                    .total_input_tokens
                    .unwrap_or(0)
                    .saturating_add(cw.total_output_tokens.unwrap_or(0));
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
                let text = ctx.cache.map_or_else(
                    || format!("{} --", glyphs.cache_label),
                    |(pct, _)| format!("{} {pct:.1}%", glyphs.cache_label),
                );
                let non_trivial_ctx = ctx
                    .cur_ctx
                    .is_some_and(|tok| tok >= CACHE_HIT_MIN_CONTEXT_TOKENS);
                let (fg, bold) = match ctx.cache {
                    Some((pct, _)) if non_trivial_ctx && pct < 20.0 => (pal.ctx_crit, true),
                    Some((pct, _)) if non_trivial_ctx && pct < 50.0 => (pal.ctx_warn, false),
                    Some(_) => (pal.cache, false),
                    None => (pal.muted, false),
                };
                Some(Segment::plain(widget, text, fg, bold))
            }
            WidgetId::CacheRead => ctx.cache.map(|(_, read)| {
                let mut buf = format!("{} ", glyphs.reset);
                format_tokens(read, &mut buf);
                Segment::plain(widget, buf, pal.cache, false)
            }),
            WidgetId::Speed => {
                let slow = ctx.speed.is_some_and(|s| s < self.cfg.speed_warn_threshold);
                let base = ctx.speed.map_or_else(
                    || format!("{} --", glyphs.speed),
                    |s| format!("{} {s:.0} tok/s", glyphs.speed),
                );
                let ascii = self.cfg.glyph_mode == GlyphMode::Ascii;
                let (text, fg, bold, accent_bg, has_retry_suffix) =
                    if let Some(anom) = ctx.retry_anomaly.as_ref() {
                        let tag = format_retry_anomaly_tag(anom, ascii);
                        let full = if ctx.speed.is_some() {
                            format!("{base} {tag}")
                        } else {
                            format!("{} {tag}", glyphs.speed)
                        };
                        let retry_fg = if anom.retries_429 > 0 {
                            pal.ctx_crit
                        } else {
                            pal.ctx_warn
                        };
                        (full, retry_fg, true, Some(retry_fg), true)
                    } else {
                        let fg = match ctx.speed {
                            Some(_) if slow => pal.speed_slow,
                            Some(_) => pal.speed_ok,
                            None => pal.muted,
                        };
                        (
                            base.clone(),
                            fg,
                            slow,
                            slow.then_some(pal.speed_slow),
                            false,
                        )
                    };
                let mut seg = Segment {
                    widget,
                    text,
                    fg,
                    accent_bg,
                    bold,
                    is_spacer: false,
                    fallbacks: Vec::new(),
                };
                if has_retry_suffix {
                    seg.push_fallback(CompactStage::SpeedStripRetry, base);
                }
                Some(seg)
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
                None if self.cfg.smart_hide => None,
                None => Some(Segment::plain(
                    widget,
                    format!("{} --", glyphs.ttft),
                    pal.muted,
                    false,
                )),
            },
            WidgetId::Thinking => match ctx.thinking_tokens {
                Some(tok) => Some(Segment::plain(
                    widget,
                    format!("{} think {}", glyphs.thinking, format_tok_str(tok)),
                    pal.thinking,
                    false,
                )),
                None if self.cfg.smart_hide => None,
                None => Some(Segment::plain(
                    widget,
                    format!("{} think --", glyphs.thinking),
                    pal.muted,
                    false,
                )),
            },
            WidgetId::Cost => {
                let (base, full) = match ctx.cost? {
                    ResolvedCost::Payload {
                        total_usd,
                        subagent_usd,
                        estimated,
                    } => {
                        let prefix = if estimated { "~$" } else { "$" };
                        let base = format!("{prefix}{total_usd:.2}");
                        let full = match subagent_usd {
                            Some(sub) => format!("{base} (sub ${sub:.2})"),
                            None => base.clone(),
                        };
                        (base, full)
                    }
                    ResolvedCost::Estimated {
                        total_usd,
                        subagent_usd,
                        unpriced_turns,
                    } => {
                        let base = format!("~${total_usd:.2}");
                        let full = match (unpriced_turns, subagent_usd) {
                            (0, None) => base.clone(),
                            (0, Some(sub)) => format!("{base} (sub ${sub:.2})"),
                            (n, None) => format!("{base} ({n} unpriced)"),
                            (n, Some(sub)) => format!("{base} (sub ${sub:.2}, {n} unpriced)"),
                        };
                        (base, full)
                    }
                };
                let has_suffix = full != base;
                let mut seg = Segment::plain(widget, full, pal.cost, false);
                if has_suffix {
                    seg.push_fallback(CompactStage::CostStripSuffix, base);
                }
                Some(seg)
            }
            _ => None,
        }
    }

    fn render_credit_segment(&self, widget: WidgetId, credit: &CreditView) -> Segment {
        let pal = self.pal;
        let glyphs = self.glyphs;
        let short_label = compact_credit_label(&credit.label);
        let core = format!("{short_label} {}", credit.remaining_amount);
        let (fg, bold) = credit_severity_style(credit.severity, pal);
        let mut full = core.clone();
        if let Some(reset) = &credit.reset_compact {
            full.push(' ');
            full.push_str(glyphs.reset);
            full.push_str(reset);
        }
        let has_reset = full != core;
        let mut seg = Segment::plain(widget, full, fg, bold);
        if has_reset {
            seg.push_fallback(CompactStage::QuotaStripReset, core);
        }
        seg
    }

    fn render_quota_widget(
        &self,
        widget: WidgetId,
        view: &Option<QuotaView>,
        fallback_label: &str,
        ctx: &RenderContext<'_>,
    ) -> Option<Segment> {
        let pal = self.pal;
        let glyphs = self.glyphs;

        let Some(q) = view else {
            let allow_credit_fallback = match widget {
                WidgetId::Quota5h => true,
                WidgetId::QuotaWeekly => !ctx.layout_has_quota_5h || ctx.quota_5h.is_some(),
                _ => false,
            };
            if allow_credit_fallback {
                if let Some(credit) = ctx.primary_credit.as_ref() {
                    return Some(self.render_credit_segment(widget, credit));
                }
            }
            return (!self.cfg.smart_hide)
                .then(|| Segment::plain(widget, format!("{fallback_label} --"), pal.muted, false));
        };
        let label = &q.label;
        let (core, mut fg, mut bold) = if let Some(amt) = q.remaining_amount {
            let (fg, bold) = credit_severity_style(classify_credit_severity(amt), pal);
            (format!("{label} {amt}"), fg, bold)
        } else {
            let core = format!(
                "{label} {}",
                crate::statusline::metrics::format_quota_pct(q.remaining_pct)
            );
            let (fg, bold) = if q.remaining_pct <= 10.0 {
                (pal.ctx_crit, true)
            } else if q.will_exhaust_before_reset {
                (pal.ctx_warn, true)
            } else if q.remaining_pct <= 30.0 || q.pace_behind {
                (pal.ctx_warn, false)
            } else {
                (pal.ctx_ok, false)
            };
            (core, fg, bold)
        };
        let mut full = core.clone();
        if let Some(reset) = &q.reset_compact {
            full.push(' ');
            full.push_str(glyphs.reset);
            full.push_str(reset);
        }
        if q.remaining_amount.is_none() && q.will_exhaust_before_reset && q.remaining_pct > 0.0 {
            if let Some(eta) = q
                .eta_secs
                .filter(|&e| e > 0 && q.burn_rate_pct_per_hr.is_some())
            {
                full.push_str(" !");
                full.push_str(&format_duration_compact(eta as u64));
            } else {
                full.push_str(" !");
            }
        }
        // Surface low/critical AI credits alongside Quota5h when QuotaWeekly will not display them.
        if widget == WidgetId::Quota5h
            && (!ctx.layout_has_quota_weekly || ctx.quota_weekly.is_some())
        {
            if let Some(credit) = ctx
                .primary_credit
                .as_ref()
                .filter(|c| c.severity != CreditSeverity::Ok)
            {
                full.push_str(&format!(" cr:{}", credit.remaining_amount));
                if credit.severity == CreditSeverity::Critical {
                    fg = pal.ctx_crit;
                    bold = true;
                } else if fg == pal.ctx_ok {
                    fg = pal.ctx_warn;
                }
            }
        }
        let has_suffix = full != core;
        let mut seg = Segment::plain(widget, full, fg, bold);
        if has_suffix {
            seg.push_fallback(CompactStage::QuotaStripReset, core);
        }
        Some(seg)
    }

    fn render_telemetry_widget(
        &self,
        widget: WidgetId,
        payload: &StatuslinePayload,
    ) -> Option<Segment> {
        let pal = self.pal;
        let glyphs = self.glyphs;

        match widget {
            WidgetId::Artifacts => {
                let n = payload.artifact_count.unwrap_or(0);
                if self.cfg.smart_hide && n == 0 {
                    return None;
                }
                Some(Segment::plain(widget, format!("art {n}"), pal.muted, false))
            }
            WidgetId::Subagents => {
                let n = subagent_count(payload);
                if self.cfg.smart_hide && n == 0 {
                    return None;
                }
                let base = format!("sub {n}");
                match single_active_subagent_label(payload)
                    .map(sanitize_inline_text)
                    .filter(|s| !s.is_empty())
                {
                    Some(clean) => {
                        let short = truncate_to_width(&clean, 20, glyphs.ellipsis);
                        let mut seg =
                            Segment::plain(widget, format!("{base} ({short})"), pal.muted, false);
                        seg.push_fallback(CompactStage::SubagentsDropLabel, base);
                        Some(seg)
                    }
                    None => Some(Segment::plain(widget, base, pal.muted, false)),
                }
            }
            WidgetId::Tasks => {
                let n = payload.task_count.unwrap_or(0);
                if self.cfg.smart_hide && n == 0 {
                    return None;
                }
                Some(Segment::plain(
                    widget,
                    format!("tasks {n}"),
                    pal.muted,
                    false,
                ))
            }
            WidgetId::Queue => {
                let n = payload.pending_input_count.unwrap_or(0);
                if n > 0 {
                    Some(Segment::plain(
                        widget,
                        format!("queue {n}"),
                        pal.thinking,
                        true,
                    ))
                } else if self.cfg.smart_hide {
                    None
                } else {
                    Some(Segment::plain(
                        widget,
                        "queue 0".to_string(),
                        pal.muted,
                        false,
                    ))
                }
            }
            WidgetId::Vim => {
                let mode = payload
                    .vim
                    .as_ref()
                    .and_then(|v| v.mode.as_deref())
                    .map(sanitize_inline_text)
                    .filter(|s| !s.is_empty());
                match mode {
                    Some(m) => {
                        let upper = m.to_ascii_uppercase();
                        let fg = match upper.as_str() {
                            "INSERT" => pal.ready,
                            "VISUAL" | "V-LINE" | "V-BLOCK" => pal.model,
                            "COMMAND" | "REPLACE" => pal.ctx_warn,
                            _ => pal.dir,
                        };
                        Some(Segment::plain(widget, format!("vim {upper}"), fg, true))
                    }
                    None if self.cfg.smart_hide => None,
                    None => Some(Segment::plain(
                        widget,
                        "vim off".to_string(),
                        pal.muted,
                        false,
                    )),
                }
            }
            WidgetId::Battle => {
                let battle = payload.battle.as_ref();
                let status = battle
                    .and_then(|b| b.status.as_deref())
                    .map(sanitize_inline_text)
                    .filter(|s| !s.is_empty());
                match status {
                    Some(st) => {
                        let upper = st.to_ascii_uppercase();
                        let text = match battle.and_then(|b| b.focused_arm) {
                            Some(arm) => format!("battle {upper} #{arm}"),
                            None => format!("battle {upper}"),
                        };
                        Some(Segment::plain(widget, text, pal.review, true))
                    }
                    None if self.cfg.smart_hide => None,
                    None => Some(Segment::plain(
                        widget,
                        "battle off".to_string(),
                        pal.muted,
                        false,
                    )),
                }
            }
            WidgetId::Sandbox => {
                let sb = payload.sandbox.as_ref();
                let on = sb.and_then(|s| s.enabled).unwrap_or(false);
                if self.cfg.smart_hide && !on {
                    return None;
                }
                let net = sb.and_then(|s| s.allow_network).unwrap_or(false);
                let text = match (on, net) {
                    (true, true) => "sandbox ON+net",
                    (true, false) => "sandbox ON",
                    (false, _) => "sandbox off",
                }
                .to_string();
                Some(Segment::plain(
                    widget,
                    text,
                    if on { pal.ready } else { pal.muted },
                    on,
                ))
            }
            _ => None,
        }
    }
}

fn credit_severity_style(severity: CreditSeverity, pal: &Palette) -> (u8, bool) {
    match severity {
        CreditSeverity::Critical => (pal.ctx_crit, true),
        CreditSeverity::Low => (pal.ctx_warn, false),
        CreditSeverity::Ok => (pal.ctx_ok, false),
    }
}

fn compact_credit_label(label: &str) -> &str {
    if label.eq_ignore_ascii_case("AI credits") {
        "credits"
    } else if label.eq_ignore_ascii_case("Prompt credits") {
        "prompt"
    } else if label.eq_ignore_ascii_case("Flow credits") {
        "flow"
    } else {
        label
    }
}

fn format_retry_anomaly_tag(anom: &RetryAnomalySummary, ascii: bool) -> String {
    let code = match anom.latest_class {
        RetryStatusClass::RateLimit429 => "429".to_string(),
        RetryStatusClass::Capacity503 => "503".to_string(),
        RetryStatusClass::Upstream5xx => anom
            .latest_status_code
            .map_or_else(|| "5xx".to_string(), |c| c.to_string()),
        RetryStatusClass::Timeout => "timeout".to_string(),
        RetryStatusClass::Disconnect => "disc".to_string(),
        RetryStatusClass::ModelOutput => "output".to_string(),
        RetryStatusClass::Canceled | RetryStatusClass::Other => anom
            .latest_status_code
            .map_or_else(|| "retry".to_string(), |c| c.to_string()),
    };
    let times = if ascii { "x" } else { "×" };
    if anom.retry_count > 1 {
        format!("! {code}{times}{}", anom.retry_count)
    } else {
        format!("! {code}")
    }
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

fn ctx_display_metrics(used_pct: f64, ctx_remaining: bool) -> (&'static str, f64) {
    if ctx_remaining {
        ("rem", (100.0 - used_pct).max(0.0))
    } else {
        ("ctx", used_pct)
    }
}

fn normalize_effort_words(raw: &str) -> Vec<String> {
    let tokens: Vec<String> = raw
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    let mut out = Vec::with_capacity(tokens.len());
    for (i, tok) in tokens.iter().enumerate() {
        if tok == "x" && i + 1 < tokens.len() {
            out.push("extra".to_string());
        } else if let Some(rest) = tok.strip_prefix('x') {
            if rest.len() >= 3 && rest.bytes().all(|b| b.is_ascii_alphabetic()) {
                out.push("extra".to_string());
                out.push(rest.to_string());
            } else {
                out.push(tok.clone());
            }
        } else {
            out.push(tok.clone());
        }
    }
    out
}

fn display_includes_effort(display_name: &str, effort: &str) -> bool {
    let eff_words = normalize_effort_words(effort);
    if eff_words.is_empty() {
        return false;
    }
    let trimmed = display_name.trim();
    if let Some(inner_start) = trimmed.rfind(['(', '（']) {
        let after = &trimmed[inner_start..];
        let mut chars = after.chars();
        if let Some(open) = chars.next() {
            let close = if open == '(' { ')' } else { '）' };
            if let Some(inner) = chars.as_str().strip_suffix(close) {
                let group_words = normalize_effort_words(inner);
                return !group_words.is_empty() && group_words == eff_words;
            }
        }
    }
    let disp_words = normalize_effort_words(trimmed);
    if disp_words.len() <= eff_words.len() {
        return false;
    }
    let split_idx = disp_words.len() - eff_words.len();
    if disp_words[split_idx..] != eff_words[..] {
        return false;
    }
    !(eff_words.len() == 1 && disp_words[split_idx - 1] == "extra")
}

fn abbreviate_leading_word(text: &str) -> String {
    if let Some((first, rest)) = text.split_once(' ') {
        if first.len() >= 4 && first.bytes().all(|b| b.is_ascii_alphabetic()) {
            if let Some(ch) = first.chars().next() {
                if rest
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphanumeric())
                {
                    return format!("{}{rest}", ch.to_ascii_uppercase());
                }
                if rest.starts_with('(') {
                    return format!("{} {rest}", ch.to_ascii_uppercase());
                }
            }
        }
    }
    text.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abbreviate_leading_word_is_vendor_agnostic() {
        assert_eq!(abbreviate_leading_word("AlphaModel M2 · max"), "AM2 · max");
        assert_eq!(abbreviate_leading_word("VendorName 3.1 Pro"), "V3.1 Pro");
        assert_eq!(
            abbreviate_leading_word("Orchestrator (Carbon) · high"),
            "O (Carbon) · high"
        );
        assert_eq!(abbreviate_leading_word("M2 · max"), "M2 · max");
        assert_eq!(abbreviate_leading_word("Model-X 120B"), "Model-X 120B");
    }

    #[test]
    fn sanitize_inline_text_strips_ansi_and_controls_and_collapses_whitespace() {
        assert_eq!(sanitize_inline_text("\x1b[31mhello\x1b[0m"), "hello");
        assert_eq!(
            sanitize_inline_text("  \x1b]0;title\x07foo\n\tbar\r\nbaz \x07 "),
            "foo bar baz"
        );
        assert_eq!(
            sanitize_inline_text("\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\"),
            "link"
        );
        assert_eq!(sanitize_inline_text("a\x00\u{0085}b\x7fc"), "a bc");
        assert_eq!(sanitize_inline_text("   \x1b[0m\n\t  "), "");
    }
}
