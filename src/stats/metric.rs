//! Composable daily/window usage metrics (`StatsMetric` & `TokenMask`).
//!
//! Serves as the single source of truth for both the static terminal report
//! (`stats --plain`) and the interactive `ratatui` stats TUI.

use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;

use bitflags::bitflags;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::{DailyUsage, ModelStats, PeriodSummary};
use crate::format::format_tokens_string as fmt_tok;

/// The four disjoint atomic token buckets recorded by AGY.
///
/// `thinking_tokens` is a subset of `output_tokens`, so excluding it from the
/// additive bitmask guarantees custom combinations never double-count tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TokenComponent {
    Input,
    Output,
    CacheRead,
    CacheWrite,
}

impl TokenComponent {
    /// Canonical display order for the TUI component toggles.
    pub const ALL: &'static [Self] = &[
        Self::Input,
        Self::Output,
        Self::CacheRead,
        Self::CacheWrite,
    ];

    pub const fn short_label(self) -> &'static str {
        match self {
            Self::Input => "In",
            Self::Output => "Out",
            Self::CacheRead => "CacheR",
            Self::CacheWrite => "CacheW",
        }
    }

    pub const fn key_hint(self) -> char {
        match self {
            Self::Input => 'i',
            Self::Output => 'o',
            Self::CacheRead => 'c',
            Self::CacheWrite => 'w',
        }
    }

    pub const fn flag(self) -> TokenMask {
        match self {
            Self::Input => TokenMask::INPUT,
            Self::Output => TokenMask::OUTPUT,
            Self::CacheRead => TokenMask::CACHE_READ,
            Self::CacheWrite => TokenMask::CACHE_WRITE,
        }
    }
}

bitflags! {
    /// Bitmask over the four disjoint atomic token buckets.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct TokenMask: u8 {
        const INPUT       = 0b0001;
        const OUTPUT      = 0b0010;
        const CACHE_READ  = 0b0100;
        const CACHE_WRITE = 0b1000;
        const IO          = Self::INPUT.bits() | Self::OUTPUT.bits();
        const CACHE       = Self::CACHE_READ.bits() | Self::CACHE_WRITE.bits();
        const ALL         = Self::IO.bits() | Self::CACHE.bits();
    }
}

impl TokenMask {
    const TABLE: [(Self, &'static str, &'static str); 4] = [
        (Self::INPUT, "In", "input"),
        (Self::OUTPUT, "Out", "output"),
        (Self::CACHE_READ, "CR", "cache_read"),
        (Self::CACHE_WRITE, "CW", "cache_write"),
    ];

    /// Returns a new mask with `comp` toggled, or `None` if toggling would leave it empty.
    pub fn toggled(self, comp: TokenComponent) -> Option<Self> {
        let next = self ^ comp.flag();
        (!next.is_empty()).then_some(next)
    }

    /// Sums the selected token buckets with saturation.
    pub fn sum(self, input: u64, output: u64, cache_read: u64, cache_write: u64) -> u64 {
        let mut total = 0u64;
        if self.contains(Self::INPUT) {
            total = total.saturating_add(input);
        }
        if self.contains(Self::OUTPUT) {
            total = total.saturating_add(output);
        }
        if self.contains(Self::CACHE_READ) {
            total = total.saturating_add(cache_read);
        }
        if self.contains(Self::CACHE_WRITE) {
            total = total.saturating_add(cache_write);
        }
        total
    }

    /// Compact human-readable label for custom masks (e.g. `CacheR`, `In+Out+CW`).
    pub fn short_label(self) -> String {
        if self == Self::CACHE_READ {
            return "CacheR".to_string();
        }
        if self == Self::CACHE_WRITE {
            return "CacheW".to_string();
        }
        Self::TABLE
            .iter()
            .filter(|(flag, _, _)| self.contains(*flag))
            .map(|(_, short, _)| *short)
            .collect::<Vec<_>>()
            .join("+")
    }

    /// Canonical CLI/JSON representation for custom masks (e.g. `cache_read`, `input+output+cache_write`).
    pub fn canonical_cli(self) -> String {
        Self::TABLE
            .iter()
            .filter(|(flag, _, _)| self.contains(*flag))
            .map(|(_, _, cli)| *cli)
            .collect::<Vec<_>>()
            .join("+")
    }
}

/// Computes prompt cache hit percentage in `0.0..=100.0`, or `None` when no prompt tokens exist.
pub(super) fn cache_hit_pct(input: u64, cache_read: u64, cache_write: u64) -> Option<f64> {
    let denom = input
        .saturating_add(cache_read)
        .saturating_add(cache_write);
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
    fn thinking_tokens(&self) -> u64;
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
    fn requests(&self) -> u64 { self.requests }
    fn input_tokens(&self) -> u64 { self.input_tokens }
    fn cache_read_tokens(&self) -> u64 { self.cache_read_tokens }
    fn cache_write_tokens(&self) -> u64 { self.cache_write_tokens }
    fn output_tokens(&self) -> u64 { self.output_tokens }
    fn thinking_tokens(&self) -> u64 { self.thinking_tokens }
    fn priced_usd(&self) -> f64 { self.priced_usd }
    fn priced_requests(&self) -> u64 { self.priced_requests }
    fn unpriced_requests(&self) -> u64 { self.unpriced_requests }
}

impl UsageCounters for DailyUsage {
    fn requests(&self) -> u64 { self.requests }
    fn input_tokens(&self) -> u64 { self.input_tokens }
    fn cache_read_tokens(&self) -> u64 { self.cache_read_tokens }
    fn cache_write_tokens(&self) -> u64 { self.cache_write_tokens }
    fn output_tokens(&self) -> u64 { self.output_tokens }
    fn thinking_tokens(&self) -> u64 { self.thinking_tokens }
    fn priced_usd(&self) -> f64 { self.priced_usd }
    fn priced_requests(&self) -> u64 { self.requests.saturating_sub(self.unpriced_requests) }
    fn unpriced_requests(&self) -> u64 { self.unpriced_requests }
}

impl UsageCounters for ModelStats {
    fn requests(&self) -> u64 { self.requests }
    fn input_tokens(&self) -> u64 { self.input_tokens }
    fn cache_read_tokens(&self) -> u64 { self.cache_read_tokens }
    fn cache_write_tokens(&self) -> u64 { self.cache_write_tokens }
    fn output_tokens(&self) -> u64 { self.output_tokens }
    fn thinking_tokens(&self) -> u64 { self.thinking_tokens }
    fn priced_usd(&self) -> f64 { self.priced_usd }
    fn priced_requests(&self) -> u64 { self.requests.saturating_sub(self.unpriced_requests) }
    fn unpriced_requests(&self) -> u64 { self.unpriced_requests }
}

/// Active aggregation metric for daily usage, heatmap, KPIs, and model rankings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum StatsMetric {
    #[default]
    Total,
    Cost,
    InputOutput,
    Input,
    Output,
    Cache,
    Thinking,
    Turns,
    CacheHit,
    Custom(TokenMask),
}

impl StatsMetric {
    /// The 9 built-in preset metrics in shortcut key (`1`–`9`) order.
    pub const PRESETS: &'static [Self] = &[
        Self::Total,
        Self::Cost,
        Self::InputOutput,
        Self::Input,
        Self::Output,
        Self::Cache,
        Self::Thinking,
        Self::Turns,
        Self::CacheHit,
    ];

    /// Returns `true` for non-additive percentage/rate metrics (such as [`Self::CacheHit`]).
    pub const fn is_rate(self) -> bool {
        matches!(self, Self::CacheHit)
    }

    /// Canonicalizes a non-empty [`TokenMask`] into a named preset when it matches one.
    pub fn from_mask(mask: TokenMask) -> Self {
        match mask {
            m if m.is_empty() || m == TokenMask::ALL => Self::Total,
            TokenMask::IO => Self::InputOutput,
            TokenMask::INPUT => Self::Input,
            TokenMask::OUTPUT => Self::Output,
            TokenMask::CACHE => Self::Cache,
            other => Self::Custom(other),
        }
    }

    /// Underlying [`TokenMask`] for additive token metrics, or `None` for non-token/rate metrics.
    pub const fn token_mask(self) -> Option<TokenMask> {
        match self {
            Self::Total => Some(TokenMask::ALL),
            Self::InputOutput => Some(TokenMask::IO),
            Self::Input => Some(TokenMask::INPUT),
            Self::Output => Some(TokenMask::OUTPUT),
            Self::Cache => Some(TokenMask::CACHE),
            Self::Custom(mask) => Some(mask),
            Self::Cost | Self::Thinking | Self::Turns | Self::CacheHit => None,
        }
    }

    /// Toggles an atomic token component from any metric state (never producing an empty mask).
    pub fn toggle_component(self, comp: TokenComponent) -> Self {
        match self.token_mask() {
            Some(mask) => mask.toggled(comp).map_or(self, Self::from_mask),
            None => Self::from_mask(comp.flag()),
        }
    }

    /// Cycles forward (`dir = 1`) or backward (`dir = -1`) across [`Self::PRESETS`].
    pub fn step(self, dir: isize) -> Self {
        let idx = Self::PRESETS
            .iter()
            .position(|&m| m == self)
            .map(|i| i as isize)
            .unwrap_or(if dir >= 0 { -1 } else { 0 });
        let next = (idx + dir).rem_euclid(Self::PRESETS.len() as isize) as usize;
        Self::PRESETS[next]
    }

    /// Concise display label shown in pills, table headers, and pane titles.
    pub fn label(self) -> String {
        match self {
            Self::Total => "Total".to_string(),
            Self::Cost => "Cost ($)".to_string(),
            Self::InputOutput => "In+Out".to_string(),
            Self::Input => "Input".to_string(),
            Self::Output => "Output".to_string(),
            Self::Cache => "Cache".to_string(),
            Self::Thinking => "Think".to_string(),
            Self::Turns => "Turns".to_string(),
            Self::CacheHit => "Cache%".to_string(),
            Self::Custom(mask) => mask.short_label(),
        }
    }

    /// Ultra-short label for narrow terminals.
    pub fn compact_label(self) -> String {
        match self {
            Self::Total => "Tot".to_string(),
            Self::Cost => "Cost".to_string(),
            Self::InputOutput => "I+O".to_string(),
            Self::Input => "In".to_string(),
            Self::Output => "Out".to_string(),
            Self::Cache => "Cch".to_string(),
            Self::Thinking => "Thk".to_string(),
            Self::Turns => "Trn".to_string(),
            Self::CacheHit => "Hit%".to_string(),
            Self::Custom(mask) => mask.short_label(),
        }
    }

    /// Extracts this metric's value from any usage aggregate (`PeriodSummary`, `DailyUsage`, `ModelStats`).
    pub(super) fn extract<U: UsageCounters>(self, rec: &U) -> MetricValue {
        match self {
            Self::Cost => MetricValue::Usd {
                usd: rec.priced_usd(),
                priced_requests: rec.priced_requests(),
                unpriced_requests: rec.unpriced_requests(),
            },
            Self::Thinking => MetricValue::Count(rec.thinking_tokens()),
            Self::Turns => MetricValue::Count(rec.requests()),
            Self::CacheHit => MetricValue::Ratio(rec.cache_hit_pct()),
            _ => {
                let mask = self.token_mask().unwrap_or(TokenMask::ALL);
                MetricValue::Count(mask.sum(
                    rec.input_tokens(),
                    rec.output_tokens(),
                    rec.cache_read_tokens(),
                    rec.cache_write_tokens(),
                ))
            }
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
                    if unpriced_requests > 0 { format!("{base}+") } else { base }
                } else {
                    format_usd_full(usd, priced_requests, unpriced_requests)
                }
            }
            (Self::Turns, MetricValue::Count(n)) => format_grouped_u64(n),
            (_, MetricValue::Count(n)) => fmt_tok(n),
            (_, MetricValue::Usd { usd, .. }) => format_usd_short(usd),
            (_, MetricValue::Ratio(pct)) => format_pct(pct),
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
            (_, MetricValue::Ratio(pct)) => format_pct(pct),
        }
    }
}

impl fmt::Display for StatsMetric {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Total => f.write_str("total"),
            Self::Cost => f.write_str("cost"),
            Self::InputOutput => f.write_str("io"),
            Self::Input => f.write_str("input"),
            Self::Output => f.write_str("output"),
            Self::Cache => f.write_str("cache"),
            Self::Thinking => f.write_str("thinking"),
            Self::Turns => f.write_str("turns"),
            Self::CacheHit => f.write_str("cache_hit"),
            Self::Custom(mask) => f.write_str(&mask.canonical_cli()),
        }
    }
}

impl FromStr for StatsMetric {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let trimmed = raw.trim().to_ascii_lowercase();
        if trimmed.is_empty() {
            return Err("metric cannot be empty".to_string());
        }
        match trimmed.as_str() {
            "total" | "all" => return Ok(Self::Total),
            "cost" | "usd" | "price" => return Ok(Self::Cost),
            "io" | "in+out" | "input+output" => return Ok(Self::InputOutput),
            "input" | "in" => return Ok(Self::Input),
            "output" | "out" => return Ok(Self::Output),
            "cache" => return Ok(Self::Cache),
            "thinking" | "think" => return Ok(Self::Thinking),
            "turns" | "requests" | "req" => return Ok(Self::Turns),
            "cache_hit" | "cache-hit" | "cache_rate" | "cache-rate" | "cache%" | "hit"
            | "hit%" => return Ok(Self::CacheHit),
            _ => {}
        }

        let mut mask = TokenMask::empty();
        for part in trimmed.split(['+', ',']) {
            match part.trim() {
                "" => return Err(format!("invalid empty token component in metric '{raw}'")),
                "input" | "in" => mask |= TokenMask::INPUT,
                "output" | "out" => mask |= TokenMask::OUTPUT,
                "cache_read" | "cache-read" | "cacher" | "cr" => mask |= TokenMask::CACHE_READ,
                "cache_write" | "cache-write" | "cachew" | "cw" => mask |= TokenMask::CACHE_WRITE,
                "cache" => mask |= TokenMask::CACHE,
                _ => {
                    return Err(format!(
                        "unknown metric '{raw}'; expected total, cost, io, input, output, cache, cache_hit, cache_read, cache_write, thinking, turns, or '+' combinations (e.g. input+output+cache_write)"
                    ));
                }
            }
        }
        if mask.is_empty() {
            return Err(format!("metric '{raw}' selected no token components"));
        }
        Ok(Self::from_mask(mask))
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
    /// Percentage in `0.0..=100.0`, or `None` when the denominator is zero.
    Ratio(Option<f64>),
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
            Self::Ratio(Some(pct)) if pct.is_finite() && pct > 0.0 => pct,
            Self::Ratio(_) => 0.0,
        }
    }

    pub fn is_positive(self) -> bool {
        match self {
            Self::Count(n) => n > 0,
            Self::Usd { .. } | Self::Ratio(_) => self.magnitude() > 0.0,
        }
    }

    /// Returns `true` when this value represents a recorded observation
    /// (including a valid `0.0%` ratio, while excluding missing `-` ratios or zero counts).
    pub fn has_sample(self) -> bool {
        match self {
            Self::Ratio(Some(pct)) => pct.is_finite(),
            Self::Ratio(None) => false,
            Self::Count(n) => n > 0,
            Self::Usd { .. } => self.magnitude() > 0.0,
        }
    }

    pub fn ratio_pct(self) -> Option<f64> {
        match self {
            Self::Ratio(pct) => pct.filter(|p| p.is_finite()),
            _ => None,
        }
    }

    pub fn cmp_desc(self, other: Self) -> Ordering {
        match (self, other) {
            (Self::Count(a), Self::Count(b)) => b.cmp(&a),
            (Self::Ratio(_), Self::Ratio(_)) => match (self.ratio_pct(), other.ratio_pct()) {
                (Some(x), Some(y)) => y.total_cmp(&x),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            },
            _ => other.magnitude().total_cmp(&self.magnitude()),
        }
    }

    pub fn cmp_asc(self, other: Self) -> Ordering {
        match (self, other) {
            (Self::Count(a), Self::Count(b)) => a.cmp(&b),
            (Self::Ratio(_), Self::Ratio(_)) => match (self.ratio_pct(), other.ratio_pct()) {
                (Some(x), Some(y)) => x.total_cmp(&y),
                // Missing `-` entries always sink to the bottom even in ascending sort.
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            },
            (
                Self::Usd {
                    usd: a,
                    priced_requests: pa,
                    ..
                },
                Self::Usd {
                    usd: b,
                    priced_requests: pb,
                    ..
                },
            ) => match (pa > 0 && a.is_finite(), pb > 0 && b.is_finite()) {
                (true, true) => a.total_cmp(&b),
                (true, false) => Ordering::Less,
                (false, true) => Ordering::Greater,
                (false, false) => Ordering::Equal,
            },
            _ => self.magnitude().total_cmp(&other.magnitude()),
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
    fn metric_parsing_canonicalization_and_serde_roundtrip() {
        assert_eq!("total".parse::<StatsMetric>().unwrap(), StatsMetric::Total);
        assert_eq!("cost".parse::<StatsMetric>().unwrap(), StatsMetric::Cost);
        assert_eq!("usd".parse::<StatsMetric>().unwrap(), StatsMetric::Cost);
        assert_eq!("io".parse::<StatsMetric>().unwrap(), StatsMetric::InputOutput);
        assert_eq!(
            "input+output".parse::<StatsMetric>().unwrap(),
            StatsMetric::InputOutput
        );
        assert_eq!(
            "cache-read".parse::<StatsMetric>().unwrap(),
            StatsMetric::Custom(TokenMask::CACHE_READ)
        );
        assert_eq!(
            "cache_write".parse::<StatsMetric>().unwrap(),
            StatsMetric::Custom(TokenMask::CACHE_WRITE)
        );
        assert_eq!(
            "cache_read+cache_write".parse::<StatsMetric>().unwrap(),
            StatsMetric::Cache
        );
        assert_eq!(
            "input+output+cache".parse::<StatsMetric>().unwrap(),
            StatsMetric::Total
        );
        assert_eq!(
            "cache_hit".parse::<StatsMetric>().unwrap(),
            StatsMetric::CacheHit
        );
        assert_eq!(
            "cache-rate".parse::<StatsMetric>().unwrap(),
            StatsMetric::CacheHit
        );
        assert_eq!(
            "hit%".parse::<StatsMetric>().unwrap(),
            StatsMetric::CacheHit
        );
        assert_eq!(StatsMetric::CacheHit.to_string(), "cache_hit");
        assert_eq!(StatsMetric::CacheHit.label(), "Cache%");
        assert!(StatsMetric::CacheHit.is_rate());
        assert!(!StatsMetric::Total.is_rate());
        assert_eq!(
            serde_json::to_string(&StatsMetric::CacheHit).unwrap(),
            "\"cache_hit\""
        );
        assert_eq!(
            serde_json::from_str::<StatsMetric>("\"cache_hit\"").unwrap(),
            StatsMetric::CacheHit
        );

        let combo: StatsMetric = "input+output+cache_write".parse().unwrap();
        assert_eq!(combo.to_string(), "input+output+cache_write");
        assert_eq!(combo.label(), "In+Out+CW");
        let encoded = serde_json::to_string(&combo).unwrap();
        assert_eq!(encoded, "\"input+output+cache_write\"");
        assert_eq!(serde_json::from_str::<StatsMetric>(&encoded).unwrap(), combo);

        assert!("".parse::<StatsMetric>().is_err());
        assert!("bogus".parse::<StatsMetric>().is_err());
        assert!("input+".parse::<StatsMetric>().is_err());
    }

    #[test]
    fn toggling_token_components_never_empties_mask_and_snaps_to_presets() {
        let m = StatsMetric::Total.toggle_component(TokenComponent::CacheRead);
        assert_eq!(
            m,
            StatsMetric::Custom(TokenMask::INPUT | TokenMask::OUTPUT | TokenMask::CACHE_WRITE)
        );
        assert_eq!(
            m.toggle_component(TokenComponent::CacheWrite),
            StatsMetric::InputOutput
        );
        assert_eq!(
            StatsMetric::Input.toggle_component(TokenComponent::Input),
            StatsMetric::Input
        );
        assert_eq!(
            StatsMetric::Cost.toggle_component(TokenComponent::CacheWrite),
            StatsMetric::Custom(TokenMask::CACHE_WRITE)
        );
    }

    #[test]
    fn metric_extracts_exact_values_without_double_counting() {
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

        assert_eq!(StatsMetric::Total.extract(&summary), MetricValue::Count(10_900));
        assert_eq!(
            StatsMetric::InputOutput.extract(&summary),
            MetricValue::Count(1_400)
        );
        assert_eq!(StatsMetric::Cache.extract(&summary), MetricValue::Count(9_500));
        assert_eq!(StatsMetric::Thinking.extract(&summary), MetricValue::Count(250));
        assert_eq!(StatsMetric::Turns.extract(&summary), MetricValue::Count(12));
        assert_eq!(
            StatsMetric::Cost.format_display(StatsMetric::Cost.extract(&summary), false),
            "~$1.25 (+2)"
        );
        assert_eq!(
            StatsMetric::Custom(TokenMask::CACHE_READ).extract(&summary),
            MetricValue::Count(9_000)
        );
        assert_eq!(
            StatsMetric::Custom(TokenMask::CACHE_WRITE).extract(&summary),
            MetricValue::Count(500)
        );

        // Cache hit: 9_000 / (1_000 + 9_000 + 500) = 85.714...%
        let hit = StatsMetric::CacheHit.extract(&summary);
        assert_eq!(StatsMetric::CacheHit.format_display(hit, true), "85.7%");
        assert_eq!(StatsMetric::CacheHit.format_average(hit, 7), "85.7%");
        assert!(hit.has_sample());

        let zero_hit = StatsMetric::CacheHit.extract(&PeriodSummary {
            requests: 1,
            input_tokens: 1_000,
            ..Default::default()
        });
        let missing_hit = StatsMetric::CacheHit.extract(&PeriodSummary::default());
        assert_eq!(StatsMetric::CacheHit.format_display(zero_hit, true), "0.0%");
        assert_eq!(StatsMetric::CacheHit.format_display(missing_hit, true), "-");
        assert!(zero_hit.has_sample());
        assert!(!missing_hit.has_sample());
        // Valid 0.0% always sorts ahead of missing `-` and non-finite ratios in both desc and asc order.
        let nan_hit = MetricValue::Ratio(Some(f64::NAN));
        assert_eq!(hit.cmp_desc(zero_hit), Ordering::Less);
        assert_eq!(zero_hit.cmp_desc(missing_hit), Ordering::Less);
        assert_eq!(zero_hit.cmp_desc(nan_hit), Ordering::Less);
        assert_eq!(nan_hit.cmp_desc(missing_hit), Ordering::Equal);
        assert_eq!(zero_hit.cmp_asc(hit), Ordering::Less);
        assert_eq!(zero_hit.cmp_asc(missing_hit), Ordering::Less);
        assert_eq!(zero_hit.cmp_asc(nan_hit), Ordering::Less);
        assert_eq!(nan_hit.cmp_asc(missing_hit), Ordering::Equal);
    }
}
