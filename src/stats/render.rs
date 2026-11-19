//! Terminal report and 20-week activity heatmap renderer for `agy-statusline stats`.

use std::collections::HashMap;
use std::io::{self, Write};

use unicode_width::UnicodeWidthStr;

use super::tz::{format_day, parse_civil_date_day};
use super::{build_weekly_usage, PeriodSummary, StatsReport, WeeklyUsage, ACTIVITY_WEEKS};
use crate::format::format_tokens_string as fmt_tok;
use crate::render::truncate_to_width;
use crate::timeutil::civil_from_days;

const WEEKDAY_NAMES: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
const MONTH_NAMES: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const UNICODE_HEATMAP: [&str; 5] = ["··", "░░", "▒▒", "▓▓", "██"];
const ASCII_HEATMAP: [&str; 5] = ["..", "==", "--", "##", "**"];
const HEATMAP_ANSI_CODES: [&str; 5] = ["2", "38;5;29", "38;5;36", "38;5;42", "1;38;5;78"];

pub(super) fn fmt_cache_hit(summary: &PeriodSummary) -> String {
    let denom = summary
        .input_tokens
        .saturating_add(summary.cache_read_tokens)
        .saturating_add(summary.cache_write_tokens);
    if denom == 0 {
        return "-".to_string();
    }
    let pct = (summary.cache_read_tokens as f64) * 100.0 / (denom as f64);
    format!("{pct:.1}%")
}

pub(super) fn fmt_usd(usd: f64, priced_requests: u64, unpriced_requests: u64) -> String {
    if priced_requests == 0 {
        return "-".to_string();
    }
    let base = if usd > 0.0 && usd < 0.01 {
        format!("~${usd:.4}")
    } else {
        format!("~${usd:.2}")
    };
    if unpriced_requests > 0 {
        format!("{base} (+{unpriced_requests})")
    } else {
        base
    }
}

fn heatmap_level(tokens: u64, peak: u64) -> usize {
    if tokens == 0 || peak == 0 {
        return 0;
    }
    let pct = (tokens as f64) / (peak as f64);
    if pct <= 0.25 {
        1
    } else if pct <= 0.50 {
        2
    } else if pct <= 0.75 {
        3
    } else {
        4
    }
}

fn resolve_weekly_usage(report: &StatsReport) -> Vec<WeeklyUsage> {
    if !report.weekly.is_empty() {
        return report.weekly.clone();
    }
    let ref_date = if report.today_local.is_empty() {
        &report.today_utc
    } else {
        &report.today_local
    };
    let Some(today_idx) = parse_civil_date_day(ref_date) else {
        return Vec::new();
    };
    let daily_by_date: HashMap<&str, u64> = if !report.unfiltered_daily_tokens.is_empty() {
        report
            .unfiltered_daily_tokens
            .iter()
            .map(|(date, tokens)| (date.as_str(), *tokens))
            .collect()
    } else {
        report
            .daily
            .iter()
            .map(|d| (d.date.as_str(), d.total_tokens))
            .collect()
    };
    build_weekly_usage(today_idx, &daily_by_date)
}

fn build_month_header(weeks: &[WeeklyUsage]) -> String {
    let week_months: Vec<Option<usize>> = weeks
        .iter()
        .map(|w| {
            let day_idx = parse_civil_date_day(&w.week_start)?;
            let (_, m, _) = civil_from_days(day_idx);
            (1..=12).contains(&m).then_some((m - 1) as usize)
        })
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

fn write_weekly_activity<W: Write>(
    out: &mut W,
    report: &StatsReport,
    no_color: bool,
    ascii: bool,
) -> io::Result<()> {
    let weeks = resolve_weekly_usage(report);
    let mut peak: Option<(String, &'static str, u64)> = None;

    for week in &weeks {
        let mon_idx = parse_civil_date_day(&week.week_start);
        for (wd_idx, maybe_tok) in week.daily_tokens.iter().enumerate() {
            if let Some(tok) = *maybe_tok {
                if tok > 0 && peak.as_ref().is_none_or(|(_, _, p)| tok > *p) {
                    let date = mon_idx.map_or_else(
                        || week.week_start.clone(),
                        |m| format_day(m + wd_idx as i64),
                    );
                    peak = Some((date, WEEKDAY_NAMES[wd_idx], tok));
                }
            }
        }
    }

    let glyphs = if ascii {
        &ASCII_HEATMAP
    } else {
        &UNICODE_HEATMAP
    };
    let peak_tokens = peak.as_ref().map_or(0, |(_, _, p)| *p);
    let peak_note = match peak {
        Some((date, wd, tok)) => format!("peak {} on {date} ({wd})", fmt_tok(tok)),
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

    writeln!(
        out,
        "\n{} {peak_note} {sep} {legend}",
        style_wrap(no_color, "38;5;117", &format!("Activity ({span_weeks}w):")),
    )?;
    if weeks.is_empty() {
        return Ok(());
    }

    let month_hdr = build_month_header(&weeks);
    if !month_hdr.is_empty() {
        writeln!(
            out,
            "{}",
            style_wrap(no_color, "2", &format!("       {month_hdr}"))
        )?;
    }

    for (wd_idx, wd_name) in WEEKDAY_NAMES.iter().enumerate() {
        let wd_label = style_wrap(no_color, "2", wd_name);
        if let Some(last_w) = weeks.iter().rposition(|w| w.daily_tokens[wd_idx].is_some()) {
            let mut row_cells = Vec::with_capacity(last_w + 1);
            for week in &weeks[..=last_w] {
                match week.daily_tokens[wd_idx] {
                    Some(tok) => {
                        let lvl = heatmap_level(tok, peak_tokens);
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
        fmt_cache_hit(summary),
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

/// Renders a concise terminal summary of [`StatsReport`], respecting `no_color` and `ascii`.
pub fn render_stats_report<W: Write>(
    report: &StatsReport,
    no_color: bool,
    ascii: bool,
    out: &mut W,
) -> io::Result<()> {
    let date_label = if report.today_local.is_empty() {
        &report.today_utc
    } else {
        &report.today_local
    };
    let tz_label = if report.timezone.is_empty() {
        "UTC"
    } else {
        &report.timezone
    };
    let mut header = format!("agy-statusline stats · Local {date_label} ({tz_label})");
    if let Some(d) = report.filter_days {
        header.push_str(&format!(" · Last {d}d"));
    }
    if let Some(first) = &report.first_active_date {
        header.push_str(&format!(" · first {first}"));
    }
    writeln!(out, "{}", style_wrap(no_color, "1;38;5;78", &header))?;

    let show_cost =
        report.windows.all_time.priced_requests > 0 || report.totals.priced_requests > 0;
    let cost_hdr = if show_cost { "  Est. Cost" } else { "" };

    let window_header = format!(
        "  Window      Turns     Input     Cache    Output  Thinking  Cache Hit{cost_hdr}"
    );
    writeln!(out, "{}", style_wrap(no_color, "2", &window_header))?;

    if let Some(d) = report.filter_days {
        write_window_row(out, &format!("Last {d}d"), &report.totals, show_cost)?;
    }
    write_window_row(out, "Today", &report.windows.today, show_cost)?;
    write_window_row(out, "7d", &report.windows.last_7d, show_cost)?;
    write_window_row(out, "30d", &report.windows.last_30d, show_cost)?;
    write_window_row(out, "All-Time", &report.windows.all_time, show_cost)?;

    write_weekly_activity(out, report, no_color, ascii)?;

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
        "\nSQLite Turns: {} turns across {} DBs · med {} · TTFT {}",
        report.sqlite.completed_turns, report.sqlite.db_files, db_speed, db_ttft
    )?;

    Ok(())
}
