//! Terminal report and 20-week activity heatmap renderer for `agy-statusline stats`.

use std::io::{self, Write};

use jiff::civil::Date;
use jiff::ToSpan;
use unicode_width::UnicodeWidthStr;

use super::metric::{
    format_pct, format_usd_full, format_usd_short, heatmap_level_f64, MetricValue, StatsMetric,
    UsageCounters,
};
use super::query::{HeatmapWeek, WEEKDAY_NAMES};
use super::{
    ConversationListItem, ConversationStatsReport, DailyUsage, PeriodSummary, StatsReport,
    ACTIVITY_WEEKS,
};
use crate::sys::fmt::{
    format_duration_compact, format_tokens_string as fmt_tok, truncate_to_width,
};

const MONTH_NAMES: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
pub(super) const UNICODE_HEATMAP: [&str; 5] = ["··", "░░", "▒▒", "▓▓", "██"];
pub(super) const ASCII_HEATMAP: [&str; 5] = ["..", "==", "--", "##", "**"];
const HEATMAP_ANSI_CODES: [&str; 5] = ["2", "38;5;29", "38;5;36", "38;5;42", "1;38;5;78"];
const DEFAULT_STATIC_DAILY_LIMIT: usize = 14;
const DEFAULT_STATIC_TURN_LIMIT: usize = 20;

#[derive(Clone, Copy)]
struct TextStyle {
    no_color: bool,
    ascii: bool,
}

impl TextStyle {
    fn sep(self) -> &'static str {
        if self.ascii {
            "|"
        } else {
            "·"
        }
    }
}

fn fmt_usd(usd: f64, priced_requests: u64, unpriced_requests: u64) -> String {
    format_usd_full(usd, priced_requests, unpriced_requests)
}

pub(super) fn build_month_header(week_dates: &[Option<Date>]) -> String {
    let week_months: Vec<Option<usize>> = week_dates
        .iter()
        .map(|d| d.map(|date| (date.month() - 1) as usize))
        .collect();

    let mut out = String::new();

    for (w, maybe_m) in week_months.iter().copied().enumerate() {
        let Some(m) = maybe_m else {
            continue;
        };
        let show = if w == 0 {
            week_months
                .get(1)
                .copied()
                .flatten()
                .is_none_or(|m1| m1 == m)
        } else {
            week_months[w - 1] != Some(m)
        };
        let col_pos = w * 3;
        let min_col = if out.is_empty() { 0 } else { out.len() + 1 };
        if show && col_pos >= min_col {
            out.push_str(&" ".repeat(col_pos - out.len()));
            out.push_str(MONTH_NAMES[m]);
        }
    }

    out
}

fn weekly_usage_to_heatmap(report: &StatsReport) -> Vec<HeatmapWeek> {
    report
        .weekly
        .iter()
        .map(|week| {
            let mon_date: Option<Date> = week.week_start.parse().ok();
            let daily = std::array::from_fn(|wd_idx| {
                week.daily_tokens[wd_idx].map(|tok| {
                    let date = mon_date
                        .and_then(|m| m.checked_add((wd_idx as i64).days()).ok())
                        .map_or_else(|| week.week_start.clone(), |d| d.to_string());
                    (date, MetricValue::Count(tok))
                })
            });
            HeatmapWeek {
                week_start: week.week_start.clone(),
                week_end: week.week_end.clone(),
                daily,
            }
        })
        .collect()
}

fn style_wrap(no_color: bool, code: &str, text: &str) -> String {
    if no_color {
        text.to_string()
    } else {
        format!("\x1b[{code}m{text}\x1b[0m")
    }
}

fn write_window_row<W: Write>(
    out: &mut W,
    label: &str,
    summary: &PeriodSummary,
    show_cost: bool,
) -> io::Result<()> {
    write!(
        out,
        "  {:<10} {:>6}  {:>8}  {:>8}  {:>8}  {:>8}  {:>9}",
        label,
        summary.requests,
        fmt_tok(summary.input_tokens),
        fmt_tok(summary.cache_read_tokens),
        fmt_tok(summary.output_tokens),
        fmt_tok(summary.thinking_tokens),
        format_pct(summary.cache_hit_pct()),
    )?;
    if show_cost {
        write!(
            out,
            "  {}",
            fmt_usd(
                summary.priced_usd,
                summary.priced_requests,
                summary.unpriced_requests,
            )
        )?;
    }
    writeln!(out)
}

fn pad_model_cell(name: &str, width: usize, ascii: bool) -> String {
    let ellipsis = if ascii { "..." } else { "…" };
    let truncated = truncate_to_width(name, width, ellipsis);
    let pad = width.saturating_sub(truncated.width());
    format!("{truncated}{}", " ".repeat(pad))
}

fn write_heatmap_grid<W: Write>(
    out: &mut W,
    weeks: &[HeatmapWeek],
    metric: StatsMetric,
    include_metric_tag: bool,
    no_color: bool,
    ascii: bool,
) -> io::Result<()> {
    let week_dates: Vec<Option<Date>> = weeks.iter().map(|w| w.week_start.parse().ok()).collect();
    let mut peak: Option<(String, &'static str, MetricValue)> = None;

    for week in weeks {
        for (wd_idx, slot) in week.daily.iter().enumerate() {
            if let Some((date, val)) = slot {
                if val.is_positive()
                    && peak
                        .as_ref()
                        .is_none_or(|(_, _, p)| val.magnitude() > p.magnitude())
                {
                    peak = Some((date.clone(), WEEKDAY_NAMES[wd_idx], *val));
                }
            }
        }
    }

    let glyphs = if ascii {
        &ASCII_HEATMAP
    } else {
        &UNICODE_HEATMAP
    };
    let peak_mag = peak.as_ref().map_or(0.0, |(_, _, v)| v.magnitude());
    let peak_note = match peak {
        Some((date, wd, val)) => {
            format!("peak {} on {date} ({wd})", metric.format_display(val, true))
        }
        None => "no activity".to_string(),
    };
    let sep = if ascii { "|" } else { "·" };
    let legend_tiles: String = (0..5)
        .map(|lvl| style_wrap(no_color, HEATMAP_ANSI_CODES[lvl], glyphs[lvl]))
        .collect::<Vec<_>>()
        .join(" ");
    let legend = format!(
        "{} {} {}",
        style_wrap(no_color, "2", "Less"),
        legend_tiles,
        style_wrap(no_color, "2", "More"),
    );
    let span_weeks = if weeks.is_empty() {
        ACTIVITY_WEEKS
    } else {
        weeks.len()
    };
    let title = if include_metric_tag {
        format!("Activity ({span_weeks}w {sep} {}):", metric.label())
    } else {
        format!("Activity ({span_weeks}w):")
    };

    writeln!(
        out,
        "\n{} {peak_note} {sep} {legend}",
        style_wrap(no_color, "38;5;117", &title),
    )?;
    if weeks.is_empty() {
        return Ok(());
    }

    let month_hdr = build_month_header(&week_dates);
    if !month_hdr.is_empty() {
        writeln!(
            out,
            "{}",
            style_wrap(no_color, "2", &format!("       {month_hdr}"))
        )?;
    }

    for (wd_idx, wd_name) in WEEKDAY_NAMES.iter().enumerate() {
        let wd_label = style_wrap(no_color, "2", wd_name);
        if let Some(last_w) = weeks.iter().rposition(|w| w.daily[wd_idx].is_some()) {
            let mut row_cells = Vec::with_capacity(last_w + 1);
            for week in &weeks[..=last_w] {
                match &week.daily[wd_idx] {
                    Some((_, val)) => {
                        let lvl = heatmap_level_f64(val.magnitude(), peak_mag);
                        row_cells.push(style_wrap(no_color, HEATMAP_ANSI_CODES[lvl], glyphs[lvl]));
                    }
                    None => row_cells.push("  ".to_string()),
                }
            }
            writeln!(out, "  {wd_label}  {}", row_cells.join(" "))?;
        } else {
            writeln!(out, "  {wd_label}")?;
        }
    }

    Ok(())
}

fn static_bar(value: f64, max: f64, width: usize, ascii: bool) -> String {
    if width == 0 || !value.is_finite() || !max.is_finite() || value <= 0.0 || max <= 0.0 {
        return String::new();
    }
    let filled = ((value / max).clamp(0.0, 1.0) * (width as f64))
        .round()
        .max(1.0) as usize;
    let ch = if ascii { "#" } else { "█" };
    ch.repeat(filled.min(width))
}

fn write_daily_table<W: Write>(
    out: &mut W,
    report: &StatsReport,
    metric: StatsMetric,
    show_cost: bool,
    no_color: bool,
    ascii: bool,
) -> io::Result<()> {
    if report.daily.is_empty() {
        return Ok(());
    }

    // Show newest days first; cap unfiltered all-time static output to 14 active days.
    let mut rows: Vec<&DailyUsage> = report.daily.iter().rev().collect();
    let total_days = rows.len();
    let omitted = if report.filter_days.is_none() && rows.len() > DEFAULT_STATIC_DAILY_LIMIT {
        rows.truncate(DEFAULT_STATIC_DAILY_LIMIT);
        total_days - rows.len()
    } else {
        0
    };

    let sep = if ascii { "|" } else { "·" };
    let section_title = if omitted > 0 {
        format!(
            "Daily (latest {} of {total_days}d {sep} {}):",
            rows.len(),
            metric.label()
        )
    } else {
        format!("Daily ({total_days}d {sep} {}):", metric.label())
    };
    writeln!(
        out,
        "\n{}",
        style_wrap(no_color, "38;5;117", &section_title)
    )?;

    let metric_hdr = metric.label();
    let cost_extra = show_cost && metric != StatsMetric::Cost;
    let cost_col_hdr = if cost_extra { "  Est. Cost" } else { "" };
    let hdr = format!(
        "  {:<10} {:>6}  {:>8}  {:>8}  {:>8}  {:>9}  {:>10}{cost_col_hdr}  Bar",
        "Date", "Turns", "Input", "Cache", "Output", "Cache Hit", metric_hdr
    );
    writeln!(out, "{}", style_wrap(no_color, "2", &hdr))?;

    let max_mag = rows
        .iter()
        .map(|d| metric.extract(*d).magnitude())
        .fold(0.0_f64, f64::max);

    for d in &rows {
        let cache_tot = d.cache_read_tokens.saturating_add(d.cache_write_tokens);
        let hit = format_pct(d.cache_hit_pct());
        let val = metric.extract(*d);
        let val_str = metric.format_display(val, true);
        let bar = static_bar(val.magnitude(), max_mag, 10, ascii);
        write!(
            out,
            "  {:<10} {:>6}  {:>8}  {:>8}  {:>8}  {:>9}  {}",
            d.date,
            d.requests,
            fmt_tok(d.input_tokens),
            fmt_tok(cache_tot),
            fmt_tok(d.output_tokens),
            hit,
            style_wrap(no_color, "1;38;5;81", &format!("{val_str:>10}")),
        )?;
        if cost_extra {
            let cost_val = StatsMetric::Cost.extract(*d);
            write!(
                out,
                "  {:>9}",
                StatsMetric::Cost.format_display(cost_val, true)
            )?;
        }
        if !bar.is_empty() {
            write!(out, "  {}", style_wrap(no_color, "38;5;42", &bar))?;
        }
        writeln!(out)?;
    }

    if omitted > 0 {
        writeln!(
            out,
            "  {}",
            style_wrap(
                no_color,
                "2",
                &format!(
                    "... {omitted} older active days omitted (use --days N or interactive TUI)"
                ),
            )
        )?;
    }
    Ok(())
}

/// Renders [`StatsReport`] using `metric` for the daily usage table and optional metric-colored heatmap.
pub(super) fn render_stats_report_with_metric<W: Write>(
    report: &StatsReport,
    metric: StatsMetric,
    metric_heatmap: Option<&[HeatmapWeek]>,
    no_color: bool,
    ascii: bool,
    out: &mut W,
) -> io::Result<()> {
    let sep = if ascii { "|" } else { "·" };
    let mut header = format!(
        "agy-statusline stats {sep} Local {} ({})",
        report.today_local, report.timezone
    );
    if let Some(d) = report.filter_days {
        header.push_str(&format!(" {sep} Last {d}d"));
    }
    if metric != StatsMetric::Total {
        header.push_str(&format!(" {sep} Metric {}", metric.label()));
    }
    if let Some(first) = &report.first_active_date {
        header.push_str(&format!(" {sep} first {first}"));
    }
    writeln!(out, "{}", style_wrap(no_color, "1;38;5;78", &header))?;

    let show_cost =
        report.windows.all_time.priced_requests > 0 || report.totals.priced_requests > 0;
    let cost_hdr = if show_cost { "  Est. Cost" } else { "" };

    let window_header =
        format!("  Window      Turns     Input     Cache    Output  Thinking  Cache Hit{cost_hdr}");
    writeln!(out, "{}", style_wrap(no_color, "2", &window_header))?;

    if let Some(d) = report.filter_days {
        write_window_row(out, &format!("Last {d}d"), &report.totals, show_cost)?;
    }
    write_window_row(out, "Today", &report.windows.today, show_cost)?;
    write_window_row(out, "7d", &report.windows.last_7d, show_cost)?;
    write_window_row(out, "30d", &report.windows.last_30d, show_cost)?;
    write_window_row(out, "All-Time", &report.windows.all_time, show_cost)?;

    match (metric, metric_heatmap) {
        (m, Some(weeks)) if m != StatsMetric::Total => {
            write_heatmap_grid(out, weeks, m, true, no_color, ascii)?;
        }
        _ => {
            let fallback = weekly_usage_to_heatmap(report);
            write_heatmap_grid(out, &fallback, StatsMetric::Total, false, no_color, ascii)?;
        }
    }

    write_daily_table(out, report, metric, show_cost, no_color, ascii)?;

    if report.models.is_empty() {
        writeln!(out, "\nModels: none recorded")?;
    } else {
        let model_w = report
            .models
            .iter()
            .map(|m| m.model.width())
            .max()
            .unwrap_or(24)
            .clamp(24, 38);
        let model_hdr = format!(
            "  {:<model_w$} {:>6}  {:>8}  {:>8}  {:>8}  {:>9}  {:>8}{cost_hdr}",
            "Model", "Turns", "Tokens", "Output", "Thinking", "Med tok/s", "Med TTFT"
        );
        writeln!(out, "\n{}", style_wrap(no_color, "2", &model_hdr))?;

        for m in &report.models {
            let model_cell = pad_model_cell(&m.model, model_w, ascii);
            let speed_str = m
                .median_tok_per_sec
                .map_or_else(|| "-".to_string(), |s| format!("{s:.1}"));
            let ttft_str = m
                .median_ttft_secs
                .map_or_else(|| "-".to_string(), |t| format!("{t:.2}s"));
            write!(
                out,
                "  {} {:>6}  {:>8}  {:>8}  {:>8}  {:>9}  {:>8}",
                model_cell,
                m.requests,
                fmt_tok(m.total_tokens),
                fmt_tok(m.output_tokens),
                fmt_tok(m.thinking_tokens),
                speed_str,
                ttft_str
            )?;
            if show_cost {
                let priced_reqs = m.requests.saturating_sub(m.unpriced_requests);
                write!(
                    out,
                    "  {}",
                    fmt_usd(m.priced_usd, priced_reqs, m.unpriced_requests)
                )?;
            }
            writeln!(out)?;
        }
    }

    let db_speed = report
        .sqlite
        .median_tok_per_sec
        .map_or_else(|| "-".to_string(), |s| format!("{s:.1} tok/s"));
    let db_ttft = report
        .sqlite
        .median_ttft_secs
        .map_or_else(|| "-".to_string(), |t| format!("{t:.2}s"));
    writeln!(
        out,
        "\nSQLite Turns: {} turns across {} DBs {sep} med {} {sep} TTFT {}",
        report.sqlite.completed_turns, report.sqlite.db_files, db_speed, db_ttft
    )?;

    Ok(())
}

pub(super) fn format_subagent_short_label(id: &str, agent: Option<&str>) -> String {
    let short: String = id.chars().take(8).collect();
    match agent {
        Some(a) => format!("{short} [{a}]"),
        None => short,
    }
}

pub(super) fn format_conversation_display_title(item: &ConversationListItem) -> String {
    let base = item.title.as_deref().unwrap_or("(untitled)");
    match (
        item.parent_conversation_id.is_some(),
        item.agent_name.as_deref(),
    ) {
        (true, Some(agent)) => format!("[{agent}] {base}"),
        (true, None) => format!("[sub] {base}"),
        (false, _) => base.to_string(),
    }
}

pub(super) fn format_turn_time(ts: Option<&str>, multi_day: bool) -> &str {
    let Some(s) = ts else {
        return "-";
    };
    if multi_day {
        // "YYYY-MM-DD HH:MM:SS" -> "MM-DD HH:MM"
        s.get(5..16).unwrap_or(s)
    } else {
        // "YYYY-MM-DD HH:MM:SS" -> "HH:MM:SS"
        s.get(11..19).unwrap_or(s)
    }
}

pub(super) fn format_ttft_compact(secs: f64) -> String {
    if secs >= 60.0 {
        let total = secs.round() as u64;
        format!("{}m{:02}s", total / 60, total % 60)
    } else {
        format!("{secs:.2}s")
    }
}

fn cache_hit_ansi(pct: Option<f64>) -> &'static str {
    match pct.filter(|p| p.is_finite()) {
        None => "2",
        Some(p) if p >= 80.0 => "38;5;78",
        Some(p) if p >= 50.0 => "38;5;221",
        Some(_) => "38;5;203",
    }
}

fn format_styled_cache_hit(pct: Option<f64>, width: usize, no_color: bool) -> String {
    let padded = format!("{:>width$}", format_pct(pct));
    style_wrap(no_color, cache_hit_ansi(pct), &padded)
}

fn format_styled_ttft(ttft: Option<f64>, width: usize, no_color: bool) -> String {
    let raw = ttft.map_or_else(|| "-".to_string(), format_ttft_compact);
    let padded = format!("{raw:>width$}");
    match ttft {
        Some(t) if t >= 60.0 => style_wrap(no_color, "38;5;203", &padded),
        Some(t) if t >= 15.0 => style_wrap(no_color, "38;5;221", &padded),
        _ => padded,
    }
}

pub(super) fn format_time_span(
    first: Option<&str>,
    last: Option<&str>,
    duration_secs: Option<u64>,
    arrow: &str,
) -> Option<String> {
    let first = first?;
    let last = last?;
    let base = if first == last {
        first.to_string()
    } else if first.get(..10) == last.get(..10) {
        let end_time = last.get(11..19).unwrap_or(last);
        format!("{first} {arrow} {end_time}")
    } else {
        format!("{first} {arrow} {last}")
    };
    Some(match duration_secs {
        Some(secs) if secs > 0 => format!("{base} ({})", format_duration_compact(secs)),
        _ => base,
    })
}

fn write_conv_summary_row<W: Write>(
    out: &mut W,
    scope: Option<&str>,
    summary: &PeriodSummary,
    speed_ttft: (Option<f64>, Option<f64>),
    show_cost: bool,
    no_color: bool,
) -> io::Result<()> {
    let (med_speed, med_ttft) = speed_ttft;
    let cache_tot = summary
        .cache_read_tokens
        .saturating_add(summary.cache_write_tokens);
    let hit_pct = summary.cache_hit_pct();
    let speed_str = med_speed.map_or_else(|| "-".to_string(), |s| format!("{s:.1}"));
    let ttft_str = format_styled_ttft(med_ttft, 8, no_color);
    let hit_str = format_styled_cache_hit(hit_pct, 9, no_color);
    if let Some(label) = scope {
        write!(
            out,
            "  {:<10} {:>6}  {:>8}  {:>8}  {:>8}  {:>8}  {}  {:>9}  {}",
            label,
            summary.requests,
            fmt_tok(summary.input_tokens),
            fmt_tok(cache_tot),
            fmt_tok(summary.output_tokens),
            fmt_tok(summary.thinking_tokens),
            hit_str,
            speed_str,
            ttft_str,
        )?;
    } else {
        write!(
            out,
            "  {:>5}  {:>8}  {:>8}  {:>8}  {:>8}  {}  {:>9}  {}",
            summary.requests,
            fmt_tok(summary.input_tokens),
            fmt_tok(cache_tot),
            fmt_tok(summary.output_tokens),
            fmt_tok(summary.thinking_tokens),
            hit_str,
            speed_str,
            ttft_str,
        )?;
    }
    if show_cost {
        let cost = fmt_usd(
            summary.priced_usd,
            summary.priced_requests,
            summary.unpriced_requests,
        );
        write!(out, "  {:>9}", cost)?;
    }
    writeln!(out)
}

/// Renders a single-conversation usage report (`agy-statusline stats <CONVERSATION_ID>`).
pub(super) fn render_conversation_stats_report<W: Write>(
    report: &ConversationStatsReport,
    metric: StatsMetric,
    show_all_turns: bool,
    no_color: bool,
    ascii: bool,
    out: &mut W,
) -> io::Result<()> {
    let sep = if ascii { "|" } else { "·" };
    let arrow = if ascii { "->" } else { "→" };
    let dash = if ascii { "-" } else { "–" };
    let mut header = format!(
        "agy-statusline stats {sep} Conversation {} {sep} {} {sep} {}",
        report.conversation_id, report.source, report.timezone
    );
    if metric != StatsMetric::Total {
        header.push_str(&format!(" {sep} Metric {}", metric.label()));
    }
    writeln!(out, "{}", style_wrap(no_color, "1;38;5;78", &header))?;

    if let Some(title) = report.title.as_deref() {
        writeln!(out, "  {}", style_wrap(no_color, "1", title))?;
    }
    if report.parent_conversation_id.is_some() || report.agent_name.is_some() {
        let mut meta_parts = Vec::new();
        if let Some(agent) = report.agent_name.as_deref() {
            meta_parts.push(format!("agent {agent}"));
        }
        if let Some(parent) = report.parent_conversation_id.as_deref() {
            meta_parts.push(format!("parent {parent}"));
        }
        writeln!(
            out,
            "  {}",
            style_wrap(no_color, "2", &meta_parts.join(&format!(" {sep} ")))
        )?;
    }

    let mut info_segments: Vec<String> = Vec::new();
    if report.models.len() == 1 {
        info_segments.push(style_wrap(no_color, "38;5;81", &report.models[0].model));
    }
    if let Some(span) = format_time_span(
        report.first_turn_local.as_deref(),
        report.last_turn_local.as_deref(),
        report.duration_secs,
        arrow,
    ) {
        info_segments.push(style_wrap(no_color, "2", &span));
    }
    if let Some(peak_ctx) = report.peak_context_tokens {
        info_segments.push(style_wrap(
            no_color,
            "2",
            &format!("peak ctx {}", fmt_tok(peak_ctx)),
        ));
    }
    if report.mid_session_misses > 0 {
        let noun = if report.mid_session_misses == 1 {
            "cache miss"
        } else {
            "cache misses"
        };
        let text = if report.mid_session_miss_usd > 0.0 {
            format!(
                "{} {noun} ({})",
                report.mid_session_misses,
                format_usd_short(report.mid_session_miss_usd)
            )
        } else {
            format!("{} {noun}", report.mid_session_misses)
        };
        info_segments.push(style_wrap(no_color, "38;5;203", &text));
    }
    if !info_segments.is_empty() {
        let joiner = style_wrap(no_color, "2", &format!(" {sep} "));
        writeln!(out, "  {}", info_segments.join(&joiner))?;
    }

    let show_cost = report.totals.priced_requests > 0;
    let cost_hdr = if show_cost { "  Est. Cost" } else { "" };

    if report.subagents.is_empty() {
        let summary_hdr = format!(
            "  Turns     Input     Cache    Output  Thinking  Cache Hit  Med tok/s  Med TTFT{cost_hdr}"
        );
        writeln!(out, "\n{}", style_wrap(no_color, "2", &summary_hdr))?;
        write_conv_summary_row(
            out,
            None,
            &report.totals,
            (
                report.sqlite.median_tok_per_sec,
                report.sqlite.median_ttft_secs,
            ),
            show_cost,
            no_color,
        )?;
    } else {
        let summary_hdr = format!(
            "  Scope       Turns     Input     Cache    Output  Thinking  Cache Hit  Med tok/s  Med TTFT{cost_hdr}"
        );
        writeln!(out, "\n{}", style_wrap(no_color, "2", &summary_hdr))?;
        write_conv_summary_row(
            out,
            Some("Main"),
            &report.main,
            (report.main_median_tok_per_sec, report.main_median_ttft_secs),
            show_cost,
            no_color,
        )?;
        let sub_label = format!("Sub ({})", report.subagents.len());
        write_conv_summary_row(
            out,
            Some(&sub_label),
            &report.subagents_summary,
            (
                report.subagents_median_tok_per_sec,
                report.subagents_median_ttft_secs,
            ),
            show_cost,
            no_color,
        )?;
        write_conv_summary_row(
            out,
            Some("Total"),
            &report.totals,
            (
                report.sqlite.median_tok_per_sec,
                report.sqlite.median_ttft_secs,
            ),
            show_cost,
            no_color,
        )?;
    }

    if !report.subagents.is_empty() {
        writeln!(
            out,
            "\n{}",
            style_wrap(
                no_color,
                "38;5;117",
                &format!("Subagents ({}):", report.subagents.len()),
            )
        )?;
        let sub_labels: Vec<String> = report
            .subagents
            .iter()
            .map(|sub| {
                let base =
                    format_subagent_short_label(&sub.conversation_id, sub.agent_name.as_deref());
                match sub.title.as_deref() {
                    Some(title) => format!("{base} {title}"),
                    None => base,
                }
            })
            .collect();
        let sub_w = sub_labels
            .iter()
            .map(|s| s.width())
            .max()
            .unwrap_or(16)
            .clamp(16, 36);
        let sub_hdr = format!(
            "  {:<sub_w$} {:>6}  {:>8}  {:>8}  {:>8}  {:>9}  {:>9}{cost_hdr}",
            "Subagent", "Turns", "Input", "Cache", "Output", "Cache Hit", "Med tok/s"
        );
        writeln!(out, "{}", style_wrap(no_color, "2", &sub_hdr))?;
        for (sub, label) in report.subagents.iter().zip(&sub_labels) {
            let cell = pad_model_cell(label, sub_w, ascii);
            let cache_tot = sub
                .summary
                .cache_read_tokens
                .saturating_add(sub.summary.cache_write_tokens);
            let hit = format_styled_cache_hit(sub.summary.cache_hit_pct(), 9, no_color);
            let speed_str = sub
                .median_tok_per_sec
                .map_or_else(|| "-".to_string(), |s| format!("{s:.1}"));
            write!(
                out,
                "  {} {:>6}  {:>8}  {:>8}  {:>8}  {}  {:>9}",
                cell,
                sub.summary.requests,
                fmt_tok(sub.summary.input_tokens),
                fmt_tok(cache_tot),
                fmt_tok(sub.summary.output_tokens),
                hit,
                speed_str
            )?;
            if show_cost {
                write!(
                    out,
                    "  {:>9}",
                    fmt_usd(
                        sub.summary.priced_usd,
                        sub.summary.priced_requests,
                        sub.summary.unpriced_requests,
                    )
                )?;
            }
            writeln!(out)?;
        }
    }

    if report.models.is_empty() {
        writeln!(out, "\nModels: none recorded")?;
    } else if report.models.len() > 1 {
        writeln!(
            out,
            "\n{}",
            style_wrap(
                no_color,
                "38;5;117",
                &format!("Models ({}):", report.models.len()),
            )
        )?;
        let model_w = report
            .models
            .iter()
            .map(|m| m.model.width())
            .max()
            .unwrap_or(16)
            .clamp(16, 36);
        let model_hdr = format!(
            "  {:<model_w$} {:>6}  {:>8}  {:>8}  {:>8}  {:>9}  {:>9}  {:>8}{cost_hdr}",
            "Model", "Turns", "Tokens", "Output", "Thinking", "Cache Hit", "Med tok/s", "Med TTFT"
        );
        writeln!(out, "{}", style_wrap(no_color, "2", &model_hdr))?;

        for m in &report.models {
            let model_cell = pad_model_cell(&m.model, model_w, ascii);
            let hit = format_styled_cache_hit(m.cache_hit_pct(), 9, no_color);
            let speed_str = m
                .median_tok_per_sec
                .map_or_else(|| "-".to_string(), |s| format!("{s:.1}"));
            let ttft_str = format_styled_ttft(m.median_ttft_secs, 8, no_color);
            write!(
                out,
                "  {} {:>6}  {:>8}  {:>8}  {:>8}  {}  {:>9}  {}",
                model_cell,
                m.requests,
                fmt_tok(m.total_tokens),
                fmt_tok(m.output_tokens),
                fmt_tok(m.thinking_tokens),
                hit,
                speed_str,
                ttft_str
            )?;
            if show_cost {
                let priced_reqs = m.requests.saturating_sub(m.unpriced_requests);
                write!(
                    out,
                    "  {:>9}",
                    fmt_usd(m.priced_usd, priced_reqs, m.unpriced_requests)
                )?;
            }
            writeln!(out)?;
        }
    }

    if !report.turns.is_empty() {
        let total_turns = report.turns.len();
        let start = if show_all_turns {
            0
        } else {
            total_turns.saturating_sub(DEFAULT_STATIC_TURN_LIMIT)
        };
        let visible = &report.turns[start..];
        let title = if start > 0 {
            format!("Turns (latest {} of {total_turns}):", visible.len())
        } else {
            format!("Turns ({total_turns}):")
        };
        writeln!(out, "\n{}", style_wrap(no_color, "38;5;117", &title))?;

        let multi_day = match (
            report.first_turn_local.as_deref().and_then(|s| s.get(..10)),
            report.last_turn_local.as_deref().and_then(|s| s.get(..10)),
        ) {
            (Some(a), Some(b)) => a != b,
            _ => false,
        };
        let idx_w = total_turns.to_string().len().max(2);
        let time_w = if multi_day { 11 } else { 8 };
        let show_model_col = report.models.len() > 1 || !report.subagents.is_empty();
        let labels: Vec<String> = if show_model_col {
            visible
                .iter()
                .map(|t| {
                    if t.is_subagent {
                        format!("sub:{}", t.model)
                    } else {
                        t.model.clone()
                    }
                })
                .collect()
        } else {
            Vec::new()
        };
        let model_w = labels
            .iter()
            .map(|s| s.width())
            .max()
            .unwrap_or(14)
            .clamp(14, 28);
        let turn_cost_hdr = if show_cost { "      Cost" } else { "" };
        let model_hdr_part = if show_model_col {
            format!("  {:<model_w$}", "Model")
        } else {
            String::new()
        };
        let num_w = if show_model_col { 7 } else { 8 };
        let turn_hdr = format!(
            "  {:>idx_w$}  {:<time_w$}{model_hdr_part}  {:>7}  {:>num_w$}  {:>num_w$}  {:>num_w$}  {:>9}  {:>7}  {:>7}{turn_cost_hdr}",
            "#", "Time", "Ctx", "Input", "Cache", "Output", "Cache Hit", "tok/s", "TTFT"
        );
        writeln!(out, "{}", style_wrap(no_color, "2", &turn_hdr))?;

        if start > 0 {
            writeln!(
                out,
                "  {}",
                style_wrap(
                    no_color,
                    "2",
                    &format!("... turns #1{dash}#{start} omitted (use --all or --json)"),
                )
            )?;
        }

        for (pos, t) in visible.iter().enumerate() {
            let time_str = format_turn_time(t.timestamp_local.as_deref(), multi_day);
            let cache_tot = t.cache_read_tokens.saturating_add(t.cache_write_tokens);
            let ctx_tot = t.input_tokens.saturating_add(cache_tot);
            let hit = format_styled_cache_hit(t.cache_hit_pct, 9, no_color);
            let speed_str = t
                .tok_per_sec
                .map_or_else(|| "-".to_string(), |s| format!("{s:.1}"));
            let ttft_str = format_styled_ttft(t.ttft_secs, 7, no_color);
            let model_part = if show_model_col {
                format!("  {}", pad_model_cell(&labels[pos], model_w, ascii))
            } else {
                String::new()
            };
            write!(
                out,
                "  {:>idx_w$}  {:<time_w$}{model_part}  {:>7}  {:>num_w$}  {:>num_w$}  {:>num_w$}  {}  {:>7}  {}",
                t.index,
                time_str,
                fmt_tok(ctx_tot),
                fmt_tok(t.input_tokens),
                fmt_tok(cache_tot),
                fmt_tok(t.output_tokens),
                hit,
                speed_str,
                ttft_str
            )?;
            if show_cost {
                let raw_cost = t
                    .priced_usd
                    .map_or_else(|| "-".to_string(), format_usd_short);
                let padded = format!("{raw_cost:>8}");
                let styled_cost = if t.priced_usd.is_some_and(|usd| usd >= 0.50) {
                    style_wrap(no_color, "38;5;221", &padded)
                } else {
                    padded
                };
                write!(out, "  {styled_cost}")?;
            }
            writeln!(out)?;
        }
    }

    if report.sqlite.db_files > 1 || report.sqlite.completed_turns != report.totals.requests {
        let db_unit = if report.sqlite.db_files == 1 {
            "DB"
        } else {
            "DBs"
        };
        writeln!(
            out,
            "\n{}",
            style_wrap(
                no_color,
                "2",
                &format!(
                    "SQLite: {} speed samples across {} {db_unit}",
                    report.sqlite.completed_turns, report.sqlite.db_files,
                ),
            )
        )?;
    }

    Ok(())
}

const DEFAULT_STATIC_CONVERSATION_LIMIT: usize = 30;

/// Renders a static historical conversation table (`agy-statusline conversations --plain`).
pub(super) fn render_conversations_list<W: Write>(
    items: &[ConversationListItem],
    timezone: &str,
    query: Option<&str>,
    show_subagents: bool,
    limit: Option<usize>,
    opts: &super::StatsOptions<'_>,
    out: &mut W,
) -> io::Result<()> {
    let style = TextStyle {
        no_color: opts.no_color,
        ascii: opts.ascii,
    };
    let (no_color, ascii, sep) = (style.no_color, style.ascii, style.sep());
    let mut header = format!(
        "agy-statusline conversations {sep} {} found {sep} {timezone}",
        items.len()
    );
    if show_subagents {
        header.push_str(&format!(" {sep} +Sub"));
    }
    if let Some(q) = query.map(str::trim).filter(|q| !q.is_empty()) {
        header.push_str(&format!(" {sep} /{q}"));
    }
    writeln!(out, "{}", style_wrap(no_color, "1;38;5;78", &header))?;

    if items.is_empty() {
        writeln!(
            out,
            "\n  {}",
            style_wrap(no_color, "2", "No matching local conversations found.")
        )?;
        return Ok(());
    }

    let max_rows = limit.unwrap_or(DEFAULT_STATIC_CONVERSATION_LIMIT);
    let shown = &items[..items.len().min(max_rows)];
    let has_cli = items.iter().any(|it| it.source == "cli");
    let has_desktop = items.iter().any(|it| it.source == "desktop");
    let show_src = has_cli && has_desktop;

    let title_w = shown
        .iter()
        .map(|it| format_conversation_display_title(it).width())
        .max()
        .unwrap_or(20)
        .clamp(16, 48);
    let ws_w = shown
        .iter()
        .filter_map(|it| it.workspace.as_deref().map(UnicodeWidthStr::width))
        .max()
        .unwrap_or(10)
        .clamp(9, 20);

    let src_hdr = if show_src { "  Src" } else { "" };
    let tbl_hdr = format!(
        "  {:<16}  {:<8}  {:<title_w$}  {:<ws_w$}  {:>6}  {:>3}{src_hdr}",
        "Modified", "ID", "Title", "Workspace", "Steps", "Sub"
    );
    writeln!(out, "\n{}", style_wrap(no_color, "2", &tbl_hdr))?;

    for it in shown {
        let modified = if it.last_modified_local.is_empty() {
            "-"
        } else {
            &it.last_modified_local
        };
        let short_id: String = it.conversation_id.chars().take(8).collect();
        let is_sub = it.parent_conversation_id.is_some();
        let raw_title = format_conversation_display_title(it);
        let title_cell = pad_model_cell(&raw_title, title_w, ascii);
        let styled_title = if is_sub {
            style_wrap(no_color, "38;5;177", &title_cell)
        } else if it.title.is_none() {
            style_wrap(no_color, "2", &title_cell)
        } else {
            title_cell
        };
        let ws_cell = pad_model_cell(it.workspace.as_deref().unwrap_or("-"), ws_w, ascii);
        let info = match it.step_count {
            Some(steps) => format!("{steps}st"),
            None => fmt_tok(it.db_size_bytes),
        };
        let sub_raw = if it.subagent_count > 0 {
            format!("+{}", it.subagent_count)
        } else {
            "-".to_string()
        };
        let sub_padded = format!("{sub_raw:>3}");
        let sub_styled = if it.subagent_count > 0 {
            style_wrap(no_color, "38;5;177", &sub_padded)
        } else {
            style_wrap(no_color, "2", &sub_padded)
        };
        write!(
            out,
            "  {}  {}  {}  {}  {:>6}  {}",
            style_wrap(no_color, "2", &format!("{modified:<16}")),
            style_wrap(no_color, "38;5;81", &short_id),
            styled_title,
            style_wrap(no_color, "38;5;75", &ws_cell),
            info,
            sub_styled,
        )?;
        if show_src {
            let tag = if it.source == "desktop" { "app" } else { "cli" };
            write!(out, "  {}", style_wrap(no_color, "2", tag))?;
        }
        writeln!(out)?;
    }

    if shown.len() < items.len() {
        writeln!(
            out,
            "\n  {}",
            style_wrap(
                no_color,
                "2",
                &format!(
                    "... showing {} of {} conversations (use --limit N or run interactively in a TTY)",
                    shown.len(),
                    items.len(),
                ),
            )
        )?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::stats::{build_weekly_usage, ModelStats, WeeklyUsage};
    use crate::sys::time::unix_day_from_civil_date;

    fn weekly_for(today: &str, daily: &[(&str, u64)]) -> Vec<WeeklyUsage> {
        let today_idx = unix_day_from_civil_date(today.parse().unwrap());
        let map: HashMap<i64, u64> = daily
            .iter()
            .map(|&(d, tok)| (unix_day_from_civil_date(d.parse().unwrap()), tok))
            .collect();
        build_weekly_usage(today_idx, &map)
    }

    #[test]
    fn github_tile_activity_renders_20w_grid_months_and_blanks_future_days() {
        let report = StatsReport {
            today_local: "2026-11-18".to_string(),
            timezone: "UTC+08:00".to_string(),
            today_utc: "2026-11-18".to_string(),
            weekly: weekly_for(
                "2026-11-18",
                &[
                    ("2026-11-03", 250_000),
                    ("2026-11-15", 1_000_000),
                    ("2026-11-16", 500_000),
                    ("2026-11-17", 750_000),
                    // Clock-skewed future date after today_local ("2026-11-18" Wed) must stay blank and uncounted.
                    ("2026-11-20", 9_999_999),
                ],
            ),
            ..Default::default()
        };

        let mut unicode_out = Vec::new();
        render_stats_report_with_metric(
            &report,
            StatsMetric::Total,
            None,
            true,
            false,
            &mut unicode_out,
        )
        .unwrap();
        let unicode_text = String::from_utf8(unicode_out).unwrap();
        assert!(unicode_text
            .contains("Activity (20w): peak 1M on 2026-11-15 (Sun) · Less ·· ░░ ▒▒ ▓▓ ██ More"));
        assert!(
            unicode_text.contains("       Jul         Aug            Sep         Oct         Nov")
        );
        assert!(unicode_text
            .contains("  Mon  ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ▒▒"));
        assert!(unicode_text
            .contains("  Tue  ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ░░ ·· ▓▓"));
        assert!(unicode_text
            .contains("  Wed  ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ··"));
        assert!(unicode_text
            .contains("  Thu  ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ··\n"));
        assert!(unicode_text
            .contains("  Sun  ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ·· ██\n"));

        // Trailing-month week 0 (2026-11-11 -> w=0 is 2026-06-29 Jun, w=1 is 2026-07-06 Jul) omits Jun collision,
        // and year-rollover (2027-01-06 -> w=19 is 2027-01-04 Jan) renders Jan cleanly.
        let trailing = StatsReport {
            today_local: "2026-11-11".to_string(),
            timezone: "UTC".to_string(),
            today_utc: "2026-11-11".to_string(),
            weekly: weekly_for("2026-11-11", &[]),
            ..Default::default()
        };
        let mut trailing_out = Vec::new();
        render_stats_report_with_metric(
            &trailing,
            StatsMetric::Total,
            None,
            true,
            true,
            &mut trailing_out,
        )
        .unwrap();
        let trailing_text = String::from_utf8(trailing_out).unwrap();
        assert!(!trailing_text.contains("JunJul"));
        assert!(trailing_text.contains("          Jul         Aug"));

        let rollover = StatsReport {
            today_local: "2027-01-06".to_string(),
            timezone: "UTC".to_string(),
            today_utc: "2027-01-06".to_string(),
            weekly: weekly_for("2027-01-06", &[]),
            ..Default::default()
        };
        let mut rollover_out = Vec::new();
        render_stats_report_with_metric(
            &rollover,
            StatsMetric::Total,
            None,
            false,
            true,
            &mut rollover_out,
        )
        .unwrap();
        let rollover_text = String::from_utf8(rollover_out).unwrap();
        assert!(rollover_text.contains("\x1b["));
        assert!(rollover_text.contains("Dec         Jan"));
    }

    #[test]
    fn render_stats_report_aligns_long_and_unicode_model_names() {
        let report = StatsReport {
            today_local: "2026-11-18".to_string(),
            timezone: "UTC".to_string(),
            today_utc: "2026-11-18".to_string(),
            models: vec![
                ModelStats {
                    model: "extremely-long-model-display-name-45-chars-xy".to_string(),
                    requests: 3,
                    total_tokens: 1_500,
                    output_tokens: 200,
                    thinking_tokens: 50,
                    median_tok_per_sec: Some(64.0),
                    median_ttft_secs: Some(0.5),
                    ..Default::default()
                },
                ModelStats {
                    model: "模型-α-wide".to_string(),
                    requests: 1,
                    total_tokens: 500,
                    output_tokens: 100,
                    thinking_tokens: 0,
                    median_tok_per_sec: Some(32.0),
                    median_ttft_secs: Some(0.8),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        let mut out = Vec::new();
        render_stats_report_with_metric(&report, StatsMetric::Total, None, true, false, &mut out)
            .unwrap();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        let hdr_idx = lines
            .iter()
            .position(|l| l.contains("Model") && l.contains("Turns"))
            .unwrap();
        let hdr_col = lines[hdr_idx].find(" Turns").unwrap();
        let row1_col = lines[hdr_idx + 1].find("     3").unwrap();
        let row2_col = lines[hdr_idx + 2].find("     1").unwrap();
        assert_eq!(
            lines[hdr_idx][..hdr_col].width(),
            lines[hdr_idx + 1][..row1_col].width()
        );
        assert_eq!(
            lines[hdr_idx][..hdr_col].width(),
            lines[hdr_idx + 2][..row2_col].width()
        );
    }

    #[test]
    fn conversation_report_hides_redundant_single_model_and_formats_anomalies() {
        use crate::stats::{ConversationTurnStats, ModelStats, SqliteStats};

        let mut turns: Vec<ConversationTurnStats> = (1..=20)
            .map(|idx| ConversationTurnStats {
                index: idx,
                conversation_id: "a724a956-091b-4bc9-a168-c301b572b1ed".to_string(),
                is_subagent: false,
                timestamp_local: Some(format!("2026-11-24 17:{:02}:00", idx)),
                model: "Gemini 4 Neon".to_string(),
                input_tokens: 5_000,
                cache_read_tokens: 100_000,
                cache_write_tokens: 0,
                output_tokens: 500,
                thinking_tokens: 100,
                total_tokens: 105_500,
                cache_hit_pct: Some(95.2),
                priced_usd: Some(0.08),
                tok_per_sec: Some(135.0),
                ttft_secs: Some(5.2),
            })
            .collect();
        turns.push(ConversationTurnStats {
            index: 21,
            conversation_id: "a724a956-091b-4bc9-a168-c301b572b1ed".to_string(),
            is_subagent: false,
            timestamp_local: Some("2026-11-24 17:21:00".to_string()),
            model: "Gemini 4 Neon".to_string(),
            input_tokens: 300_000,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            output_tokens: 500,
            thinking_tokens: 100,
            total_tokens: 300_500,
            cache_hit_pct: Some(0.0),
            priced_usd: Some(1.02),
            tok_per_sec: Some(135.0),
            ttft_secs: Some(5.2),
        });
        turns.push(ConversationTurnStats {
            index: 22,
            conversation_id: "a724a956-091b-4bc9-a168-c301b572b1ed".to_string(),
            is_subagent: false,
            timestamp_local: Some("2026-11-24 17:22:00".to_string()),
            model: "Gemini 4 Neon".to_string(),
            input_tokens: 5_000,
            cache_read_tokens: 100_000,
            cache_write_tokens: 0,
            output_tokens: 500,
            thinking_tokens: 100,
            total_tokens: 105_500,
            cache_hit_pct: Some(95.2),
            priced_usd: Some(0.08),
            tok_per_sec: Some(135.0),
            ttft_secs: Some(121.7),
        });

        let report = ConversationStatsReport {
            conversation_id: "a724a956-091b-4bc9-a168-c301b572b1ed".to_string(),
            title: Some("Cache Audit".to_string()),
            source: "cli".to_string(),
            timezone: "UTC+08:00".to_string(),
            first_turn_local: Some("2026-11-24 17:01:00".to_string()),
            last_turn_local: Some("2026-11-24 17:22:00".to_string()),
            duration_secs: Some(1_260),
            peak_context_tokens: Some(300_000),
            mid_session_misses: 1,
            mid_session_miss_usd: 1.02,
            totals: PeriodSummary {
                requests: 22,
                input_tokens: 405_000,
                cache_read_tokens: 2_100_000,
                output_tokens: 11_000,
                thinking_tokens: 2_200,
                total_tokens: 2_516_000,
                priced_usd: 2.70,
                priced_requests: 22,
                ..Default::default()
            },
            models: vec![ModelStats {
                model: "Gemini 4 Neon".to_string(),
                requests: 22,
                ..Default::default()
            }],
            turns,
            sqlite: SqliteStats {
                db_files: 1,
                completed_turns: 22,
                median_tok_per_sec: Some(135.0),
                median_ttft_secs: Some(5.2),
                ..Default::default()
            },
            ..Default::default()
        };

        let mut truncated = Vec::new();
        render_conversation_stats_report(
            &report,
            StatsMetric::Total,
            false,
            true,
            false,
            &mut truncated,
        )
        .unwrap();
        let text = String::from_utf8(truncated).unwrap();
        assert!(text.contains("Gemini 4 Neon · 2026-11-24 17:01:00 → 17:22:00 (21m) · peak ctx 300K · 1 cache miss (~$1.02)"), "{text}");
        assert!(!text.contains("Scope"));
        assert!(!text.contains("Models ("));
        assert!(!text.contains("SQLite:"));
        assert!(text.contains("2m02s"), "{text}");

        // Omitted indicator sits ABOVE turn #3, not after turn #22.
        let omitted_pos = text
            .find("... turns #1–#2 omitted (use --all or --json)")
            .unwrap();
        let turn3_pos = text.find("  3  17:03:00").unwrap();
        assert!(omitted_pos < turn3_pos, "{text}");

        // `--all` prints every turn without an omitted notice.
        let mut full = Vec::new();
        render_conversation_stats_report(&report, StatsMetric::Total, true, true, false, &mut full)
            .unwrap();
        let full_text = String::from_utf8(full).unwrap();
        assert!(!full_text.contains("omitted"), "{full_text}");
        assert!(full_text.contains("Turns (22):"), "{full_text}");
        assert!(full_text.contains("  1  17:01:00"), "{full_text}");
    }
}
