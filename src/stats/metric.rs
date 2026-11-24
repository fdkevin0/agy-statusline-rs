//! Usage metrics (`StatsMetric`) and cache-hit rate formatting for `agy-statusline stats`.
//!
//! Serves as the single source of truth for both the static terminal report
//! (`stats --plain`) and the interactive `ratatui` stats TUI.

use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::{DailyUsage, ModelStats, PeriodSummary};
use crate::statusline::format::format_tokens_string as fmt_tok;

/// Computes prompt cache hit percentage in `0.0..=100.0`, or `None` when no prompt tokens exist.
pub(super) fn cache_hit_pct(input: u64, cache_read: u64, cache_write: u64) -> Option<f64> {
    let denom = input.saturating_add(cache_read).saturating_add(cache_write);
    (denom > 0).then(|| (cache_read as f64) * 100.0 / (denom as f64))
}

/// Formats an optional percentage (`92.5%` or `-`).
pub(super) fn format_pct(pct: Option<f64>) -> String {
    match pct {
        Some(p) if p.is_finite() => format!("{p:.1}%"),
        _ => "-".to_string(),
    }
}

/// Shared read-only projection over usage aggregates (`PeriodSummary`, `DailyUsage`, `ModelStats`).
pub(super) trait UsageCounters {
    fn requests(&self) -> u64;
    fn input_tokens(&self) -> u64;
    fn cache_read_tokens(&self) -> u64;
    fn cache_write_tokens(&self) -> u64;
    fn output_tokens(&self) -> u64;
    fn total_tokens(&self) -> u64;
    fn priced_usd(&self) -> f64;
    fn priced_requests(&self) -> u64;
    fn unpriced_requests(&self) -> u64;

    fn cache_hit_pct(&self) -> Option<f64> {
        cache_hit_pct(
            self.input_tokens(),
            self.cache_read_tokens(),
            self.cache_write_tokens(),
        )
    }
}

impl UsageCounters for PeriodSummary {
    fn requests(&self) -> u64 {
        self.requests
    }
    fn input_tokens(&self) -> u64 {
        self.input_tokens
    }
    fn cache_read_tokens(&self) -> u64 {
        self.cache_read_tokens
    }
    fn cache_write_tokens(&self) -> u64 {
        self.cache_write_tokens
    }
    fn output_tokens(&self) -> u64 {
        self.output_tokens
    }
    fn total_tokens(&self) -> u64 {
        self.total_tokens
    }
    fn priced_usd(&self) -> f64 {
        self.priced_usd
    }
    fn priced_requests(&self) -> u64 {
        self.priced_requests
    }
    fn unpriced_requests(&self) -> u64 {
        self.unpriced_requests
    }
}

impl UsageCounters for DailyUsage {
    fn requests(&self) -> u64 {
        self.requests
    }
    fn input_tokens(&self) -> u64 {
        self.input_tokens
    }
    fn cache_read_tokens(&self) -> u64 {
        self.cache_read_tokens
    }
    fn cache_write_tokens(&self) -> u64 {
        self.cache_write_tokens
    }
    fn output_tokens(&self) -> u64 {
        self.output_tokens
    }
    fn total_tokens(&self) -> u64 {
        self.total_tokens
    }
    fn priced_usd(&self) -> f64 {
        self.priced_usd
    }
    fn priced_requests(&self) -> u64 {
        self.requests.saturating_sub(self.unpriced_requests)
    }
    fn unpriced_requests(&self) -> u64 {
        self.unpriced_requests
    }
}

impl UsageCounters for ModelStats {
    fn requests(&self) -> u64 {
        self.requests
    }
    fn input_tokens(&self) -> u64 {
        self.input_tokens
    }
    fn cache_read_tokens(&self) -> u64 {
        self.cache_read_tokens
    }
    fn cache_write_tokens(&self) -> u64 {
        self.cache_write_tokens
    }
    fn output_tokens(&self) -> u64 {
        self.output_tokens
    }
    fn total_tokens(&self) -> u64 {
        self.total_tokens
    }
    fn priced_usd(&self) -> f64 {
        self.priced_usd
    }
    fn priced_requests(&self) -> u64 {
        self.requests.saturating_sub(self.unpriced_requests)
    }
    fn unpriced_requests(&self) -> u64 {
        self.unpriced_requests
    }
}

/// Active additive aggregation metric for daily usage, heatmap, KPIs, and model rankings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum StatsMetric {
    #[default]
    Total,
    Cost,
    InputOutput,
    Output,
    Turns,
}

impl StatsMetric {
    /// The 5 built-in preset metrics in shortcut key (`1`–`5`) order.
    pub const PRESETS: &'static [Self] = &[
        Self::Total,
        Self::Cost,
        Self::InputOutput,
        Self::Output,
        Self::Turns,
    ];

    /// Cycles forward (`dir = 1`) or backward (`dir = -1`) across [`Self::PRESETS`].
    pub fn step(self, dir: isize) -> Self {
        let idx = Self::PRESETS.iter().position(|&m| m == self).unwrap_or(0) as isize;
        let next = (idx + dir).rem_euclid(Self::PRESETS.len() as isize) as usize;
        Self::PRESETS[next]
    }

    /// Concise display label shown in pills, table headers, and pane titles.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Total => "Total",
            Self::Cost => "Cost ($)",
            Self::InputOutput => "In+Out",
            Self::Output => "Output",
            Self::Turns => "Turns",
        }
    }

    /// Ultra-short label for narrow terminals.
    pub const fn compact_label(self) -> &'static str {
        match self {
            Self::Total => "Tot",
            Self::Cost => "Cost",
            Self::InputOutput => "I+O",
            Self::Output => "Out",
            Self::Turns => "Trn",
        }
    }

    /// Extracts this metric's value from any usage aggregate (`PeriodSummary`, `DailyUsage`, `ModelStats`).
    pub(super) fn extract<U: UsageCounters>(self, rec: &U) -> MetricValue {
        match self {
            Self::Total => MetricValue::Count(rec.total_tokens()),
            Self::Cost => MetricValue::Usd {
                usd: rec.priced_usd(),
                priced_requests: rec.priced_requests(),
                unpriced_requests: rec.unpriced_requests(),
            },
            Self::InputOutput => {
                MetricValue::Count(rec.input_tokens().saturating_add(rec.output_tokens()))
            }
            Self::Output => MetricValue::Count(rec.output_tokens()),
            Self::Turns => MetricValue::Count(rec.requests()),
        }
    }

    /// Formats a [`MetricValue`] for full (`compact = false`) or column-aligned (`compact = true`) display.
    pub fn format_display(self, value: MetricValue, compact: bool) -> String {
        match (self, value) {
            (
                Self::Cost,
                MetricValue::Usd {
                    usd,
                    priced_requests,
                    unpriced_requests,
                },
            ) => {
                if priced_requests == 0 {
                    "-".to_string()
                } else if compact {
                    let base = format_usd_short(usd);
                    if unpriced_requests > 0 {
                        format!("{base}+")
                    } else {
                        base
                    }
                } else {
                    format_usd_full(usd, priced_requests, unpriced_requests)
                }
            }
            (Self::Turns, MetricValue::Count(n)) => format_grouped_u64(n),
            (_, MetricValue::Count(n)) => fmt_tok(n),
            (_, MetricValue::Usd { usd, .. }) => format_usd_short(usd),
        }
    }

    /// Formats a daily average scalar for this metric across `active_days`.
    pub fn format_average(self, total: MetricValue, active_days: usize) -> String {
        if active_days == 0 {
            return "-".to_string();
        }
        let denom = active_days as f64;
        match (self, total) {
            (
                Self::Cost,
                MetricValue::Usd {
                    usd,
                    priced_requests,
                    ..
                },
            ) => {
                if priced_requests == 0 {
                    "-".to_string()
                } else {
                    format_usd_short(usd / denom)
                }
            }
            (Self::Turns, MetricValue::Count(n)) => {
                let avg = (n as f64) / denom;
                if avg >= 100.0 {
                    format_grouped_u64(avg.round() as u64)
                } else {
                    format!("{avg:.1}")
                }
            }
            (_, MetricValue::Count(n)) => fmt_tok(((n as f64) / denom).round() as u64),
            (_, MetricValue::Usd { usd, .. }) => format_usd_short(usd / denom),
        }
    }
}

impl fmt::Display for StatsMetric {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Total => f.write_str("total"),
            Self::Cost => f.write_str("cost"),
            Self::InputOutput => f.write_str("io"),
            Self::Output => f.write_str("output"),
            Self::Turns => f.write_str("turns"),
        }
    }
}

impl FromStr for StatsMetric {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "total" | "all" => Ok(Self::Total),
            "cost" | "usd" | "price" => Ok(Self::Cost),
            "io" | "in+out" | "input+output" => Ok(Self::InputOutput),
            "output" | "out" => Ok(Self::Output),
            "turns" | "requests" | "req" => Ok(Self::Turns),
            "" => Err("metric cannot be empty".to_string()),
            _ => Err(format!(
                "unknown metric '{raw}'; expected total, cost, io, output, or turns"
            )),
        }
    }
}

impl Serialize for StatsMetric {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for StatsMetric {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// Typed scalar extracted for a [`StatsMetric`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MetricValue {
    Count(u64),
    Usd {
        usd: f64,
        priced_requests: u64,
        unpriced_requests: u64,
    },
}

impl MetricValue {
    pub fn magnitude(self) -> f64 {
        match self {
            Self::Count(n) => n as f64,
            Self::Usd {
                usd,
                priced_requests,
                ..
            } if priced_requests > 0 && usd.is_finite() && usd > 0.0 => usd,
            Self::Usd { .. } => 0.0,
        }
    }

    pub fn is_positive(self) -> bool {
        match self {
            Self::Count(n) => n > 0,
            Self::Usd { .. } => self.magnitude() > 0.0,
        }
    }

    pub fn cmp_desc(self, other: Self) -> Ordering {
        match (self, other) {
            (Self::Count(a), Self::Count(b)) => b.cmp(&a),
            _ => other.magnitude().total_cmp(&self.magnitude()),
        }
    }
}

/// Maps `(value, peak)` into a 5-step heatmap intensity level (`0..=4`).
pub(super) fn heatmap_level_f64(value: f64, peak: f64) -> usize {
    if !value.is_finite() || !peak.is_finite() || value <= 0.0 || peak <= 0.0 {
        return 0;
    }
    let pct = value / peak;
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

/// Formats USD with `~` prefix and optional `(+N)` unpriced-turn annotation.
pub(super) fn format_usd_full(usd: f64, priced_requests: u64, unpriced_requests: u64) -> String {
    if priced_requests == 0 {
        return "-".to_string();
    }
    let base = format_usd_short(usd);
    if unpriced_requests > 0 {
        format!("{base} (+{unpriced_requests})")
    } else {
        base
    }
}

/// Formats a USD amount compactly (`~$0.0042` below `$0.01`, `~$12.34` otherwise).
pub(super) fn format_usd_short(usd: f64) -> String {
    if usd > 0.0 && usd < 0.01 {
        format!("~${usd:.4}")
    } else {
        format!("~${usd:.2}")
    }
}

/// Formats a `u64` with thousands separators (e.g. `20,271`).
pub(super) fn format_grouped_u64(n: u64) -> String {
    let raw = n.to_string();
    let len = raw.len();
    let mut out = String::with_capacity(len + len / 3);
    for (idx, ch) in raw.chars().enumerate() {
        if idx > 0 && (len - idx).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metric_parsing_and_serde_roundtrip() {
        assert_eq!("total".parse::<StatsMetric>().unwrap(), StatsMetric::Total);
        assert_eq!("cost".parse::<StatsMetric>().unwrap(), StatsMetric::Cost);
        assert_eq!("usd".parse::<StatsMetric>().unwrap(), StatsMetric::Cost);
        assert_eq!(
            "io".parse::<StatsMetric>().unwrap(),
            StatsMetric::InputOutput
        );
        assert_eq!(
            "input+output".parse::<StatsMetric>().unwrap(),
            StatsMetric::InputOutput
        );
        assert_eq!("out".parse::<StatsMetric>().unwrap(), StatsMetric::Output);
        assert_eq!("turns".parse::<StatsMetric>().unwrap(), StatsMetric::Turns);

        let encoded = serde_json::to_string(&StatsMetric::InputOutput).unwrap();
        assert_eq!(encoded, "\"io\"");
        assert_eq!(
            serde_json::from_str::<StatsMetric>(&encoded).unwrap(),
            StatsMetric::InputOutput
        );

        assert!("".parse::<StatsMetric>().is_err());
        assert!("bogus".parse::<StatsMetric>().is_err());
        assert!("input+".parse::<StatsMetric>().is_err());
    }

    #[test]
    fn metric_extracts_values_and_computes_cache_hit_pct() {
        let summary = PeriodSummary {
            requests: 12,
            input_tokens: 1_000,
            cache_read_tokens: 9_000,
            cache_write_tokens: 500,
            output_tokens: 400,
            thinking_tokens: 250,
            total_tokens: 10_900,
            priced_usd: 1.25,
            priced_requests: 10,
            unpriced_requests: 2,
        };

        assert_eq!(
            StatsMetric::Total.extract(&summary),
            MetricValue::Count(10_900)
        );
        assert_eq!(
            StatsMetric::InputOutput.extract(&summary),
            MetricValue::Count(1_400)
        );
        assert_eq!(
            StatsMetric::Output.extract(&summary),
            MetricValue::Count(400)
        );
        assert_eq!(StatsMetric::Turns.extract(&summary), MetricValue::Count(12));
        assert_eq!(
            StatsMetric::Cost.format_display(StatsMetric::Cost.extract(&summary), false),
            "~$1.25 (+2)"
        );
        assert_eq!(format_pct(summary.cache_hit_pct()), "85.7%");
        assert_eq!(format_pct(PeriodSummary::default().cache_hit_pct()), "-");
    }
}
