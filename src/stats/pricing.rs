//! User-configured model token pricing catalog (`pricing.json`).

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::db::resolve_home_dir;

/// Per-million-token pricing rates supplied by user-configured `pricing.json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelPricingRate {
    /// USD per 1M uncached input tokens.
    #[serde(default)]
    pub input_per_mtok: f64,
    /// USD per 1M prompt cache read tokens.
    #[serde(default)]
    pub cache_read_per_mtok: f64,
    /// USD per 1M prompt cache write tokens.
    #[serde(default)]
    pub cache_write_per_mtok: f64,
    /// USD per 1M output tokens.
    #[serde(default)]
    pub output_per_mtok: f64,
}

/// Optional user-defined pricing catalog loaded from `$XDG_CONFIG_HOME/agy-statusline/pricing.json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PricingCatalog {
    /// Map of case-insensitive model name or substring pattern to per-million-token rates.
    #[serde(default)]
    pub models: BTreeMap<String, ModelPricingRate>,
}

impl PricingCatalog {
    /// Loads a [`PricingCatalog`] from `path`, returning an empty catalog if missing or invalid.
    pub fn load_from_file(path: &Path) -> Self {
        let Ok(raw) = fs::read_to_string(path) else {
            return Self::default();
        };
        serde_json::from_str::<Self>(&raw).unwrap_or_default()
    }

    /// Estimates USD cost for one turn if any candidate model string matches a valid entry.
    pub fn estimate_cost(
        &self,
        candidates: &[&str],
        input_tokens: u64,
        cache_read_tokens: u64,
        cache_write_tokens: u64,
        output_tokens: u64,
    ) -> Option<(String, f64)> {
        if self.models.is_empty() {
            return None;
        }
        let lowered_candidates: Vec<String> = candidates
            .iter()
            .map(|c| c.trim().to_ascii_lowercase())
            .filter(|c| !c.is_empty())
            .collect();
        if lowered_candidates.is_empty() {
            return None;
        }

        let valid_entries: Vec<(&str, String, &ModelPricingRate)> = self
            .models
            .iter()
            .filter(|(_, rate)| is_valid_rate(rate))
            .filter_map(|(pattern, rate)| {
                let pat_lower = pattern.trim().to_ascii_lowercase();
                (!pat_lower.is_empty()).then_some((pattern.as_str(), pat_lower, rate))
            })
            .collect();

        // Exact matches take precedence in candidate specificity order (`response_model`,
        // then `model_display_name`, then `model_group`), breaking ties via `BTreeMap` key order.
        let mut exact_match: Option<(&str, &ModelPricingRate)> = None;
        for cand in &lowered_candidates {
            if let Some((key, _, rate)) = valid_entries.iter().find(|(_, pat, _)| pat == cand) {
                exact_match = Some((*key, *rate));
                break;
            }
        }

        // Substring fallback prefers the longest pattern match, breaking ties via `BTreeMap` key order.
        let mut substr_match: Option<(usize, &str, &ModelPricingRate)> = None;
        if exact_match.is_none() {
            for (pattern, pat_lower, rate) in &valid_entries {
                if lowered_candidates.iter().any(|c| c.contains(pat_lower)) {
                    let replace = match substr_match {
                        None => true,
                        Some((best_len, best_key, _)) => {
                            pat_lower.len() > best_len
                                || (pat_lower.len() == best_len && *pattern < best_key)
                        }
                    };
                    if replace {
                        substr_match = Some((pat_lower.len(), *pattern, *rate));
                    }
                }
            }
        }

        let (matched_key, rate) = exact_match.or_else(|| substr_match.map(|(_, k, r)| (k, r)))?;
        let cost = ((input_tokens as f64) * rate.input_per_mtok
            + (cache_read_tokens as f64) * rate.cache_read_per_mtok
            + (cache_write_tokens as f64) * rate.cache_write_per_mtok
            + (output_tokens as f64) * rate.output_per_mtok)
            / 1_000_000.0;
        if !cost.is_finite() || cost < 0.0 {
            return None;
        }
        Some((matched_key.to_string(), round_usd_6(cost)))
    }
}

fn is_valid_rate(rate: &ModelPricingRate) -> bool {
    [
        rate.input_per_mtok,
        rate.cache_read_per_mtok,
        rate.cache_write_per_mtok,
        rate.output_per_mtok,
    ]
    .iter()
    .all(|v| v.is_finite() && *v >= 0.0)
}

/// Returns the default optional user pricing catalog path
/// (`$XDG_CONFIG_HOME/agy-statusline/pricing.json` or `~/.config/agy-statusline/pricing.json`).
pub fn default_pricing_path() -> Option<PathBuf> {
    let cfg_base = env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| resolve_home_dir().map(|h| h.join(".config")))?;
    Some(cfg_base.join("agy-statusline").join("pricing.json"))
}

pub(super) fn round_usd_6(v: f64) -> f64 {
    if v.is_finite() {
        (v * 1_000_000.0).round() / 1_000_000.0
    } else {
        0.0
    }
}
