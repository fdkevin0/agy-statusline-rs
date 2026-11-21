//! Terminal report and 20-week activity heatmap renderer for `agy-statusline stats`.

use std::io::{self, Write};

use jiff::civil::Date;
use jiff::ToSpan;
use unicode_width::UnicodeWidthStr;

use super::metric::{format_usd_full, heatmap_level_f64, MetricValue, StatsMetric};
use super::query::{HeatmapWeek, WEEKDAY_NAMES};
use super::{DailyUsage, PeriodSummary, StatsReport, ACTIVITY_WEEKS};
use crate::format::format_tokens_string as fmt_tok;
use crate::render::truncate_to_width;

const MONTH_NAMES: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
pub(super) const UNICODE_HEATMAP: [&str; 5] = ["··", "░░", "▒▒", "▓▓", "██"];
pub(super) const ASCII_HEATMAP: [&str; 5] = ["..", "==", "--", "##", "**"];
const HEATMAP_ANSI_CODES: [&str; 5] = ["2", "38;5;29", "38;5;36", "38;5;42", "1;38;5;78"];
const DEFAULT_STATIC_DAILY_LIMIT: usize = 14;

pub(super) fn fmt_cache_hit(summary: &PeriodSummary) -> String {
    fmt_cache_hit_parts(
        summary.input_tokens,
        summary.cache_read_tokens,
        summary.cache_write_tokens,
    )
}

fn fmt_cache_hit_parts(input: u64, cache_read: u64, cache_write: u64) -> String {
    let denom = input
        .saturating_add(cache_read)
        .saturating_add(cache_write);
    if denom == 0 {
        return "-".to_string();
    }
    let pct = (cache_read as f64) * 100.0 / (denom as f64);
    format!("{pct:.1}%")
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
        Some((date, wd, val)) => format!(
            "peak {} on {date} ({wd})",
            metric.format_display(val, true)
        ),
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
    writeln!(out, "\n{}", style_wrap(no_color, "38;5;117", &section_title))?;

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
        let hit = fmt_cache_hit_parts(d.input_tokens, d.cache_read_tokens, d.cache_write_tokens);
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
                &format!("... {omitted} older active days omitted (use --days N or interactive TUI)"),
            )
        )?;
    }
    Ok(())
}

/// Renders a concise terminal summary of [`StatsReport`], respecting `no_color` and `ascii`.
#[cfg(test)]
pub(super) fn render_stats_report<W: Write>(
    report: &StatsReport,
    no_color: bool,
    ascii: bool,
    out: &mut W,
) -> io::Result<()> {
    render_stats_report_with_metric(report, StatsMetric::Total, None, no_color, ascii, out)
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
    let mut header = format!(
        "agy-statusline stats · Local {} ({})",
        report.today_local, report.timezone
    );
    if let Some(d) = report.filter_days {
        header.push_str(&format!(" · Last {d}d"));
    }
    if metric != StatsMetric::Total {
        header.push_str(&format!(" · Metric {}", metric.label()));
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
    let archived_note = if report.sqlite.archived_db_files > 0 {
        format!(" ({} archived)", report.sqlite.archived_db_files)
    } else {
        String::new()
    };
    writeln!(
        out,
        "\nSQLite Turns: {} turns across {} DBs{archived_note} · med {} · TTFT {}",
        report.sqlite.completed_turns, report.sqlite.db_files, db_speed, db_ttft
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::stats::{build_weekly_usage, ModelStats, WeeklyUsage};
    use crate::timeutil::unix_day_from_civil_date;

    fn weekly_for(today: &str, daily: &[(&str, u64)]) -> Vec<WeeklyUsage> {
        let today_idx = unix_day_from_civil_date(today.parse().unwrap());
        let map: HashMap<&str, u64> = daily.iter().copied().collect();
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
        render_stats_report(&report, true, false, &mut unicode_out).unwrap();
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
        render_stats_report(&trailing, true, true, &mut trailing_out).unwrap();
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
        render_stats_report(&rollover, false, true, &mut rollover_out).unwrap();
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
        render_stats_report(&report, true, false, &mut out).unwrap();
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
}
