//! Model token pricing catalog (TTL-cached `models.dev` rates).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::scan::is_effort_token;
use crate::agy::db::is_absent_or_real_file;
use crate::sys::fmt::sanitize_inline_text;
use crate::sys::fs::{atomic_write, modified_timestamp};
use crate::sys::process::{run_command_stdout_bounded, BoundedCommandError};
use crate::sys::time::{unix_now_secs, SECS_PER_DAY};

/// Default cache TTL for auto-fetched `models.dev` pricing data (24 hours).
pub(crate) const DEFAULT_PRICING_TTL_SECS: u64 = SECS_PER_DAY as u64;
/// Cooldown after a failed network fetch before retrying automatically (5 minutes).
const PRICING_RETRY_COOLDOWN_SECS: u64 = 300;
/// Public unauthenticated `models.dev` API endpoint.
const MODELS_DEV_API_URL: &str = "https://models.dev/api.json";
pub(crate) const MODELS_DEV_CACHE_FILE: &str = "models_dev_pricing.json";
const MODELS_DEV_ERR_FILE: &str = "models_dev_pricing.err";
const MAX_PRICING_JSON_BYTES: u64 = 16 * 1024 * 1024;

/// Metadata-only change fingerprint for `models_dev_pricing.json`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PricingFileFingerprint {
    pub(crate) exists: bool,
    pub(crate) mtime_secs: i64,
    pub(crate) mtime_nanos: u32,
    pub(crate) len: u64,
}

/// Computes a metadata-only fingerprint for `models_dev_pricing.json` without reading or parsing it.
pub(crate) fn pricing_cache_fingerprint(cache_dir: Option<&Path>) -> PricingFileFingerprint {
    let Some(dir) = cache_dir else {
        return PricingFileFingerprint::default();
    };
    let Ok(meta) = fs::metadata(dir.join(MODELS_DEV_CACHE_FILE)) else {
        return PricingFileFingerprint::default();
    };
    if !meta.is_file() {
        return PricingFileFingerprint::default();
    }
    let (mtime_secs, mtime_nanos) = modified_timestamp(&meta);
    PricingFileFingerprint {
        exists: true,
        mtime_secs,
        mtime_nanos,
        len: meta.len(),
    }
}

/// Returns `true` when the pricing file exists and its file mtime is within the default 24h TTL.
pub(crate) fn is_pricing_fingerprint_fresh(fp: PricingFileFingerprint, now_secs: u64) -> bool {
    fp.exists
        && fp.mtime_secs >= 0
        && now_secs.saturating_sub(fp.mtime_secs as u64) < DEFAULT_PRICING_TTL_SECS
}

/// Spawns a detached background `pricing sync --if-stale` when `models_dev_pricing.json` is
/// missing or its file mtime is past the default TTL, without reading or parsing the JSON file.
pub(crate) fn maybe_spawn_pricing_sync_if_stale(cache_dir: Option<&Path>, now_secs: u64) {
    let Some(dir) = cache_dir else {
        return;
    };
    let fp = pricing_cache_fingerprint(Some(dir));
    if !is_pricing_fingerprint_fresh(fp, now_secs) && !in_retry_cooldown(dir, now_secs) {
        record_retry_cooldown(dir, now_secs);
        spawn_background_pricing_sync();
    }
}

/// Controls how [`PricingCatalog`] cache loading handles missing or expired pricing data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncMode {
    /// Never perform network I/O or spawn background sync; read local files only.
    Offline,
    /// Synchronously refresh from `models.dev` if the cache is missing or past TTL.
    BlockingIfStale,
    /// Spawn a detached background `pricing sync --if-stale` process if missing or past TTL.
    SpawnIfStale,
}

/// Context-length pricing tier overriding base rates above `min_context_tokens`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct ContextPricingTier {
    /// Strict lower bound (`prompt_context_tokens > min_context_tokens`) for activating this tier.
    min_context_tokens: u64,
    /// USD per 1M uncached input tokens in this tier.
    #[serde(default)]
    input_per_mtok: f64,
    /// USD per 1M prompt cache read tokens in this tier.
    #[serde(default)]
    cache_read_per_mtok: f64,
    /// USD per 1M prompt cache write tokens in this tier.
    #[serde(default)]
    cache_write_per_mtok: f64,
    /// USD per 1M output tokens in this tier.
    #[serde(default)]
    output_per_mtok: f64,
}

/// Per-million-token pricing rates supplied by `models.dev`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct ModelPricingRate {
    /// USD per 1M uncached input tokens.
    #[serde(default)]
    input_per_mtok: f64,
    /// USD per 1M prompt cache read tokens.
    #[serde(default)]
    cache_read_per_mtok: f64,
    /// USD per 1M prompt cache write tokens.
    #[serde(default)]
    cache_write_per_mtok: f64,
    /// USD per 1M output tokens.
    #[serde(default)]
    output_per_mtok: f64,
    /// Optional context-length pricing tiers sorted ascending by `min_context_tokens`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    context_tiers: Vec<ContextPricingTier>,
}

/// Persisted `models.dev` cache envelope stored in `$XDG_CACHE_HOME/agy-statusline/models_dev_pricing.json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct CachedPricingFile {
    fetched_at: u64,
    #[serde(default)]
    models: BTreeMap<String, ModelPricingRate>,
}

/// Diagnostic summary of the local pricing cache.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PricingCacheStatus {
    /// Path to `models_dev_pricing.json`.
    cache_path: PathBuf,
    /// Number of cached `models.dev` entries (`0` if missing).
    cached_models: usize,
    /// Unix timestamp when cache was last fetched, if present.
    fetched_at: Option<u64>,
    /// Whether the cache exists and is within `ttl_secs` at `now_secs`.
    fresh: bool,
    /// Configured TTL in seconds.
    ttl_secs: u64,
}

#[derive(Debug, Clone, PartialEq)]
struct IndexedEntry {
    key: String,
    pat_lower: String,
    pat_norm: String,
    rate: ModelPricingRate,
}

/// Indexed model pricing catalog.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PricingCatalog {
    entries: Vec<IndexedEntry>,
    exact_raw: HashMap<String, usize>,
    exact_norm: HashMap<String, usize>,
}

impl PricingCatalog {
    /// Builds a pre-indexed [`PricingCatalog`] from a map of model patterns to rates.
    fn from_models(models: BTreeMap<String, ModelPricingRate>) -> Self {
        let mut entries = Vec::with_capacity(models.len());
        let mut exact_raw: HashMap<String, usize> = HashMap::with_capacity(models.len());
        let mut exact_norm: HashMap<String, usize> = HashMap::with_capacity(models.len());

        for (pattern, mut rate) in models {
            if !is_valid_rate(&rate) {
                continue;
            }
            let pat_lower = pattern.trim().to_ascii_lowercase();
            if pat_lower.is_empty() {
                continue;
            }
            rate.context_tiers.retain(is_valid_tier_rate);
            rate.context_tiers
                .sort_by_key(|tier| tier.min_context_tokens);
            let pat_norm = normalize_model_key(&pat_lower);
            let idx = entries.len();
            entries.push(IndexedEntry {
                key: pattern,
                pat_lower: pat_lower.clone(),
                pat_norm: pat_norm.clone(),
                rate,
            });
            exact_raw.entry(pat_lower).or_insert(idx);
            if !pat_norm.is_empty() {
                exact_norm.entry(pat_norm).or_insert(idx);
            }
        }

        Self {
            entries,
            exact_raw,
            exact_norm,
        }
    }

    /// Loads rates from an explicit `cache_dir` using `mode` (returns empty catalog when `None`).
    pub(crate) fn load_effective_in(
        cache_dir: Option<&Path>,
        mode: SyncMode,
        now_secs: u64,
    ) -> Self {
        Self::load_effective_with(
            cache_dir,
            mode,
            now_secs,
            DEFAULT_PRICING_TTL_SECS,
            fetch_models_dev_via_curl,
        )
    }

    /// Loads effective pricing with an explicit cache path, TTL, and fetcher boundary for deterministic tests.
    pub(super) fn load_effective_with<F>(
        cache_dir: Option<&Path>,
        mode: SyncMode,
        now_secs: u64,
        ttl_secs: u64,
        fetcher: F,
    ) -> Self
    where
        F: FnOnce(&str) -> io::Result<String>,
    {
        let mut cached_models = BTreeMap::new();

        if let Some(dir) = cache_dir {
            let cache_path = dir.join(MODELS_DEV_CACHE_FILE);
            let cached = load_cached_file(&cache_path);
            let is_fresh = cached
                .as_ref()
                .is_some_and(|c| now_secs.saturating_sub(c.fetched_at) < ttl_secs);

            let active_cache = if is_fresh {
                cached
            } else {
                match mode {
                    SyncMode::Offline => cached,
                    SyncMode::BlockingIfStale => {
                        if in_retry_cooldown(dir, now_secs) {
                            cached
                        } else {
                            match sync_models_dev_to_dir(dir, now_secs, fetcher) {
                                Ok(fresh_file) => Some(fresh_file),
                                Err(_) => {
                                    record_retry_cooldown(dir, now_secs);
                                    cached
                                }
                            }
                        }
                    }
                    SyncMode::SpawnIfStale => {
                        if !in_retry_cooldown(dir, now_secs) {
                            record_retry_cooldown(dir, now_secs);
                            spawn_background_pricing_sync();
                        }
                        cached
                    }
                }
            };

            if let Some(c) = active_cache {
                cached_models = c.models;
            }
        }

        Self::from_models(cached_models)
    }

    /// Estimates USD cost for one turn if any candidate model string matches a valid entry.
    #[cfg(test)]
    fn estimate_cost(
        &self,
        candidates: &[&str],
        input_tokens: u64,
        cache_read_tokens: u64,
        cache_write_tokens: u64,
        output_tokens: u64,
    ) -> Option<(String, f64)> {
        let entry_idx = self.resolve_entry_idx(candidates)?;
        self.estimate_cost_for_entry(
            entry_idx,
            input_tokens,
            cache_read_tokens,
            cache_write_tokens,
            output_tokens,
        )
    }

    /// Resolves the best matching catalog entry index for `candidates`, independent of token counts.
    pub(super) fn resolve_entry_idx(&self, candidates: &[&str]) -> Option<usize> {
        if self.entries.is_empty() {
            return None;
        }
        let lowered_candidates: Vec<(String, String)> = candidates
            .iter()
            .map(|c| c.trim().to_ascii_lowercase())
            .filter(|c| !c.is_empty())
            .map(|raw| {
                let norm = normalize_model_key(&raw);
                (raw, norm)
            })
            .collect();
        if lowered_candidates.is_empty() {
            return None;
        }

        let mut matched = lowered_candidates
            .iter()
            .find_map(|(raw, _)| self.exact_raw.get(raw))
            .or_else(|| {
                lowered_candidates
                    .iter()
                    .find_map(|(_, norm)| self.exact_norm.get(norm))
            })
            .copied();

        // Substring fallback prefers the longest match, breaking equal-length ties by
        // preferring literal raw matches over separator-normalized matches, then key order.
        if matched.is_none() {
            let mut best: Option<(usize, bool, usize)> = None;
            for (idx, entry) in self.entries.iter().enumerate() {
                let (match_len, is_raw) = if lowered_candidates
                    .iter()
                    .any(|(raw, _)| raw.contains(&entry.pat_lower))
                {
                    (entry.pat_lower.len(), true)
                } else if !entry.pat_norm.is_empty()
                    && lowered_candidates
                        .iter()
                        .any(|(_, norm)| norm.contains(&entry.pat_norm))
                {
                    (entry.pat_norm.len(), false)
                } else {
                    continue;
                };
                if best.is_none_or(|(best_len, best_raw, prev_idx)| {
                    (match_len, is_raw) > (best_len, best_raw)
                        || ((match_len, is_raw) == (best_len, best_raw)
                            && entry.key < self.entries[prev_idx].key)
                }) {
                    best = Some((match_len, is_raw, idx));
                }
            }
            matched = best.map(|(_, _, idx)| idx);
        }

        matched.or_else(|| self.resolve_orchestrator_alias_idx(&lowered_candidates))
    }

    /// Computes USD cost for a pre-resolved catalog entry index.
    pub(super) fn estimate_cost_for_entry(
        &self,
        entry_idx: usize,
        input_tokens: u64,
        cache_read_tokens: u64,
        cache_write_tokens: u64,
        output_tokens: u64,
    ) -> Option<(String, f64)> {
        let entry = self.entries.get(entry_idx)?;
        let prompt_context_tokens = input_tokens
            .saturating_add(cache_read_tokens)
            .saturating_add(cache_write_tokens);
        let active_tier = entry
            .rate
            .context_tiers
            .iter()
            .rfind(|tier| prompt_context_tokens > tier.min_context_tokens);
        let (input_rate, cache_read_rate, cache_write_rate, output_rate) = match active_tier {
            Some(tier) => (
                tier.input_per_mtok,
                tier.cache_read_per_mtok,
                tier.cache_write_per_mtok,
                tier.output_per_mtok,
            ),
            None => (
                entry.rate.input_per_mtok,
                entry.rate.cache_read_per_mtok,
                entry.rate.cache_write_per_mtok,
                entry.rate.output_per_mtok,
            ),
        };
        let cost = ((input_tokens as f64) * input_rate
            + (cache_read_tokens as f64) * cache_read_rate
            + (cache_write_tokens as f64) * cache_write_rate
            + (output_tokens as f64) * output_rate)
            / 1_000_000.0;
        if !cost.is_finite() {
            return None;
        }
        Some((entry.key.clone(), round_usd_6(cost)))
    }

    /// Returns `true` when `response_model` carries an explicit orchestrator codename or
    /// `model_display_name` carries any specific model/version token beyond generic family,
    /// role, and effort words.
    pub(super) fn has_specific_model_qualifier(
        response_model: Option<&str>,
        model_display_name: Option<&str>,
    ) -> bool {
        let resp_has_codename = response_model
            .map(|s| normalize_model_key(s.trim()))
            .is_some_and(|norm| extract_orchestrator_codename(&norm).is_some());
        let disp_is_specific = model_display_name
            .map(|s| normalize_model_key(s.trim()))
            .is_some_and(|norm| norm.split_whitespace().any(|w| !is_generic_family_token(w)));
        resp_has_codename || disp_is_specific
    }

    fn find_best_canonical_gemini_entry(
        &self,
        codename: &str,
        requested_ver: Option<(u32, u32)>,
    ) -> Option<usize> {
        let mut best: Option<((u32, u32), bool, usize)> = None;
        for (idx, entry) in self.entries.iter().enumerate() {
            let Some((ver, entry_codename)) = parse_canonical_gemini_codename_entry(entry) else {
                continue;
            };
            if entry_codename != codename {
                continue;
            }
            if let Some(req) = requested_ver {
                if ver != req {
                    continue;
                }
            }
            let is_kebab = entry.pat_lower.contains('-');
            if best.is_none_or(|(best_ver, best_kebab, prev_idx)| {
                (ver, is_kebab) > (best_ver, best_kebab)
                    || ((ver, is_kebab) == (best_ver, best_kebab)
                        && entry.key < self.entries[prev_idx].key)
            }) {
                best = Some((ver, is_kebab, idx));
            }
        }
        best.map(|(_, _, idx)| idx)
    }

    fn resolve_orchestrator_alias_idx(&self, candidates: &[(String, String)]) -> Option<usize> {
        // Dynamically resolve an explicit orchestrator codename against canonical Gemini entries
        // in the loaded catalog (no hardcoded generation or cross-tier fallback).
        // Scan all post-`orchestrator` tokens across candidates so non-catalog routing tags
        // (e.g. `-v2`, `-b`, `-agent`) do not shadow a real codename later in the same string
        // or in a subsequent candidate such as `model_display_name`.
        let fallback_ver = candidates
            .iter()
            .filter(|(_, norm)| norm.split_whitespace().any(|w| w == "orchestrator"))
            .find_map(|(_, norm)| extract_explicit_version(norm));

        for (_, norm) in candidates {
            let requested_ver = extract_explicit_version(norm).or(fallback_ver);
            for codename in orchestrator_codename_candidates(norm) {
                if let Some(idx) = self.find_best_canonical_gemini_entry(codename, requested_ver) {
                    return Some(idx);
                }
            }
        }
        None
    }
}

fn is_generic_orchestrator_token(word: &str) -> bool {
    is_effort_token(word)
        || matches!(
            word,
            "pro"
                | "flash"
                | "agy"
                | "g1"
                | "worker"
                | "tiered"
                | "thinking"
                | "preview"
                | "latest"
        )
}

fn is_generic_family_token(word: &str) -> bool {
    is_generic_orchestrator_token(word)
        || matches!(word, "gemini" | "orchestrator" | "antigravity" | "google")
}

fn orchestrator_codename_candidates(norm: &str) -> impl Iterator<Item = &str> {
    norm.split_whitespace()
        .skip_while(|&w| w != "orchestrator")
        .skip(1)
        .filter(|w| !is_generic_orchestrator_token(w) && !w.bytes().all(|b| b.is_ascii_digit()))
}

fn extract_orchestrator_codename(norm: &str) -> Option<&str> {
    orchestrator_codename_candidates(norm).next()
}

fn extract_explicit_version(norm: &str) -> Option<(u32, u32)> {
    // Only inspect tokens before `orchestrator` so trailing date/build suffixes
    // (e.g. `gemini-pro-orchestrator-2026-04`) are never misread as generation numbers.
    let mut words = norm.split_whitespace().take_while(|&w| w != "orchestrator");
    while let Some(w) = words.next() {
        if let Ok(major) = w.parse::<u32>() {
            let minor = words
                .next()
                .and_then(|next| next.parse::<u32>().ok())
                .unwrap_or(0);
            return Some((major, minor));
        }
    }
    None
}

/// Parses a canonical unprefixed Gemini catalog key (`gemini-<major>[.<minor>]-<codename>` or
/// separator-equivalent `Gemini 4 Argon`), excluding provider-prefixed (`vertex/...`),
/// regional (`...@eu`), and variant-suffixed (`...-contributor`) keys.
fn parse_canonical_gemini_codename_entry(entry: &IndexedEntry) -> Option<((u32, u32), &str)> {
    if entry.pat_lower.contains('/') || entry.pat_lower.contains('@') {
        return None;
    }
    let mut words = entry.pat_norm.split_whitespace();
    if words.next()? != "gemini" {
        return None;
    }
    let major = words.next()?.parse::<u32>().ok()?;
    let third = words.next()?;
    let (minor, codename) = match third.parse::<u32>() {
        Ok(minor) => (minor, words.next()?),
        Err(_) => (0, third),
    };
    if words.next().is_some() {
        return None;
    }
    Some(((major, minor), codename))
}

pub(super) fn normalize_model_key(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut prev_space = true;
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_space = false;
        } else if !prev_space {
            out.push(' ');
            prev_space = true;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
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

fn is_valid_tier_rate(tier: &ContextPricingTier) -> bool {
    tier.min_context_tokens > 0
        && [
            tier.input_per_mtok,
            tier.cache_read_per_mtok,
            tier.cache_write_per_mtok,
            tier.output_per_mtok,
        ]
        .iter()
        .all(|v| v.is_finite() && *v >= 0.0)
}

fn read_cache_bytes_bounded(path: &Path) -> io::Result<Vec<u8>> {
    let file = fs::File::open(path)?;
    let mut buf = Vec::new();
    file.take(MAX_PRICING_JSON_BYTES.saturating_add(1))
        .read_to_end(&mut buf)?;
    if (buf.len() as u64) > MAX_PRICING_JSON_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "exceeds maximum inspected size",
        ));
    }
    Ok(buf)
}

fn load_cached_file(path: &Path) -> Option<CachedPricingFile> {
    if !is_absent_or_real_file(path) {
        return None;
    }
    let bytes = read_cache_bytes_bounded(path).ok()?;
    serde_json::from_slice::<CachedPricingFile>(&bytes).ok()
}

/// Inspects `models_dev_pricing.json` under `cache_dir` and returns a concise diagnostic status.
pub(crate) fn inspect_pricing_cache(cache_dir: &Path, now_unix_secs: u64) -> String {
    let path = cache_dir.join(MODELS_DEV_CACHE_FILE);
    if !is_absent_or_real_file(&path) {
        let is_symlink = fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink());
        return if is_symlink {
            "symlink (ignored)".to_string()
        } else {
            "not a regular file (ignored)".to_string()
        };
    }
    match fs::symlink_metadata(&path) {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return "not found".to_string(),
        Err(e) => return format!("unreadable ({e})"),
    }
    let bytes = match read_cache_bytes_bounded(&path) {
        Ok(b) => b,
        Err(e) => return format!("corrupt ({e})"),
    };
    #[derive(Deserialize)]
    struct Summary {
        fetched_at: u64,
        #[serde(default)]
        models: BTreeMap<String, Value>,
    }
    let parsed: Summary = match serde_json::from_slice(&bytes) {
        Ok(p) => p,
        Err(e) => {
            let reason: String = sanitize_inline_text(&e.to_string())
                .chars()
                .take(80)
                .collect();
            return format!("corrupt ({reason})");
        }
    };
    let freshness = if now_unix_secs.saturating_sub(parsed.fetched_at) < DEFAULT_PRICING_TTL_SECS {
        "fresh"
    } else {
        "stale"
    };
    format!("found, {} models ({freshness})", parsed.models.len())
}

fn in_retry_cooldown(cache_dir: &Path, now_secs: u64) -> bool {
    let Ok(raw) = fs::read_to_string(cache_dir.join(MODELS_DEV_ERR_FILE)) else {
        return false;
    };
    let Ok(ts) = raw.trim().parse::<u64>() else {
        return false;
    };
    now_secs.saturating_sub(ts) < PRICING_RETRY_COOLDOWN_SECS
}

fn record_retry_cooldown(cache_dir: &Path, now_secs: u64) {
    let _ = atomic_write(
        &cache_dir.join(MODELS_DEV_ERR_FILE),
        format!("{now_secs}\n").as_bytes(),
    );
}

fn spawn_background_pricing_sync() {
    let Ok(exe) = env::current_exe() else {
        return;
    };
    let _ = Command::new(exe)
        .args(["pricing", "sync", "--if-stale"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// Fetches `url` using `curl` with bounded connect/total timeouts and response size limit.
pub(super) fn fetch_models_dev_via_curl(url: &str) -> io::Result<String> {
    let mut cmd = Command::new("curl");
    cmd.args(["-fsSL", "--connect-timeout", "5", "--max-time", "15", url]);
    let out =
        match run_command_stdout_bounded(&mut cmd, Duration::from_secs(16), MAX_PRICING_JSON_BYTES)
        {
            Ok(out) => out,
            Err(BoundedCommandError::TimedOut) => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "curl timed out fetching models.dev pricing",
                ));
            }
            Err(BoundedCommandError::Io(err)) => return Err(err),
        };
    if out.truncated {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "models.dev response exceeded size limit",
        ));
    }
    if !out.status.is_some_and(|s| s.success()) {
        return Err(io::Error::other(match out.status {
            Some(status) => format!("curl failed ({status})"),
            None => "curl failed".to_string(),
        }));
    }
    String::from_utf8(out.stdout).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

const CANONICAL_PROVIDERS: &[&str] = &[
    "google",
    "anthropic",
    "openai",
    "deepseek",
    "xai",
    "mistral",
    "meta-llama",
];

fn extract_context_tier(
    tier_obj: &serde_json::Map<String, Value>,
    min_context_tokens: u64,
    base: &ModelPricingRate,
) -> Option<ContextPricingTier> {
    let tier = ContextPricingTier {
        min_context_tokens,
        input_per_mtok: tier_obj
            .get("input")
            .and_then(Value::as_f64)
            .unwrap_or(base.input_per_mtok),
        cache_read_per_mtok: tier_obj
            .get("cache_read")
            .and_then(Value::as_f64)
            .unwrap_or(base.cache_read_per_mtok),
        cache_write_per_mtok: tier_obj
            .get("cache_write")
            .and_then(Value::as_f64)
            .unwrap_or(base.cache_write_per_mtok),
        output_per_mtok: tier_obj
            .get("output")
            .and_then(Value::as_f64)
            .unwrap_or(base.output_per_mtok),
    };
    is_valid_tier_rate(&tier).then_some(tier)
}

fn extract_rate_from_cost(cost_obj: &serde_json::Map<String, Value>) -> Option<ModelPricingRate> {
    let input = cost_obj.get("input").and_then(Value::as_f64).unwrap_or(0.0);
    let output = cost_obj
        .get("output")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let cache_read = cost_obj
        .get("cache_read")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let cache_write = cost_obj
        .get("cache_write")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);

    let mut rate = ModelPricingRate {
        input_per_mtok: input,
        cache_read_per_mtok: cache_read,
        cache_write_per_mtok: cache_write,
        output_per_mtok: output,
        context_tiers: Vec::new(),
    };
    if !is_valid_rate(&rate) || (input == 0.0 && output == 0.0) {
        return None;
    }

    let mut context_tiers = Vec::new();
    if let Some(tiers) = cost_obj.get("tiers").and_then(Value::as_array) {
        for tier_val in tiers {
            let Some(tier_obj) = tier_val.as_object() else {
                continue;
            };
            let Some(tier_meta) = tier_obj.get("tier").and_then(Value::as_object) else {
                continue;
            };
            if tier_meta.get("type").and_then(Value::as_str) != Some("context") {
                continue;
            }
            let Some(size) = tier_meta.get("size").and_then(Value::as_u64) else {
                continue;
            };
            if size == 0 {
                continue;
            }
            if let Some(tier) = extract_context_tier(tier_obj, size, &rate) {
                context_tiers.push(tier);
            }
        }
    }

    if context_tiers.is_empty() {
        if let Some(over_200k_obj) = cost_obj.get("context_over_200k").and_then(Value::as_object) {
            if let Some(tier) = extract_context_tier(over_200k_obj, 200_000, &rate) {
                context_tiers.push(tier);
            }
        }
    }

    context_tiers.sort_by_key(|tier| tier.min_context_tokens);
    context_tiers.dedup_by_key(|tier| tier.min_context_tokens);
    rate.context_tiers = context_tiers;
    Some(rate)
}

/// Parses `https://models.dev/api.json` payload into a flat `model_id -> ModelPricingRate` map.
///
/// Canonical direct providers (`google`, `anthropic`, `openai`, etc.) take precedence over
/// aggregator/cloud-reseller providers when identical or separator-equivalent model IDs appear
/// under multiple providers.
fn parse_models_dev_json(raw: &str) -> io::Result<BTreeMap<String, ModelPricingRate>> {
    let root: Value = serde_json::from_str(raw)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("invalid JSON: {e}")))?;
    let Some(providers) = root.as_object() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected top-level JSON object from models.dev",
        ));
    };

    let mut out = BTreeMap::new();
    let mut canonical_norm_keys = HashSet::new();

    // Pass 1: canonical direct providers take precedence.
    for &prov_id in CANONICAL_PROVIDERS {
        if let Some(prov_obj) = providers.get(prov_id).and_then(Value::as_object) {
            insert_provider_models(prov_obj, &mut out, &mut canonical_norm_keys, true);
        }
    }
    // Pass 2: remaining providers fill in any models not already present (including normalized collisions).
    for (prov_id, prov_val) in providers {
        if CANONICAL_PROVIDERS.contains(&prov_id.as_str()) {
            continue;
        }
        if let Some(prov_obj) = prov_val.as_object() {
            insert_provider_models(prov_obj, &mut out, &mut canonical_norm_keys, false);
        }
    }

    Ok(out)
}

fn insert_provider_models(
    prov_obj: &serde_json::Map<String, Value>,
    out: &mut BTreeMap<String, ModelPricingRate>,
    canonical_norm_keys: &mut HashSet<String>,
    canonical: bool,
) {
    let Some(models_obj) = prov_obj.get("models").and_then(Value::as_object) else {
        return;
    };
    for (model_id, model_val) in models_obj {
        let Some(cost_obj) = model_val.get("cost").and_then(Value::as_object) else {
            continue;
        };
        let Some(rate) = extract_rate_from_cost(cost_obj) else {
            continue;
        };
        let key = model_id.trim().to_ascii_lowercase();
        if key.is_empty() {
            continue;
        }
        let norm_key = normalize_model_key(&key);
        if canonical {
            out.insert(key, rate);
            if !norm_key.is_empty() {
                canonical_norm_keys.insert(norm_key);
            }
        } else if !out.contains_key(&key)
            && (norm_key.is_empty() || !canonical_norm_keys.contains(&norm_key))
        {
            out.insert(key, rate);
        }
    }
}

fn sync_models_dev_to_dir<F>(
    cache_dir: &Path,
    now_secs: u64,
    fetcher: F,
) -> io::Result<CachedPricingFile>
where
    F: FnOnce(&str) -> io::Result<String>,
{
    let raw = fetcher(MODELS_DEV_API_URL)?;
    let models = parse_models_dev_json(&raw)?;
    let cached = CachedPricingFile {
        fetched_at: now_secs,
        models,
    };
    let target = cache_dir.join(MODELS_DEV_CACHE_FILE);
    let encoded = serde_json::to_string(&cached)
        .map_err(|e| io::Error::other(format!("serialize pricing cache: {e}")))?;
    atomic_write(&target, encoded.as_bytes())?;
    let _ = fs::remove_file(cache_dir.join(MODELS_DEV_ERR_FILE));
    Ok(cached)
}

/// Syncs `models.dev` pricing into the cache directory, returning `(status, did_fetch)`.
fn sync_models_dev_pricing_with<F>(
    cache_dir: &Path,
    if_stale_only: bool,
    now_secs: u64,
    ttl_secs: u64,
    fetcher: F,
) -> io::Result<(PricingCacheStatus, bool)>
where
    F: FnOnce(&str) -> io::Result<String>,
{
    let cache_path = cache_dir.join(MODELS_DEV_CACHE_FILE);
    if if_stale_only {
        if let Some(existing) = load_cached_file(&cache_path) {
            if now_secs.saturating_sub(existing.fetched_at) < ttl_secs {
                return Ok((
                    PricingCacheStatus {
                        cache_path,
                        cached_models: existing.models.len(),
                        fetched_at: Some(existing.fetched_at),
                        fresh: true,
                        ttl_secs,
                    },
                    false,
                ));
            }
        }
    }

    let synced = sync_models_dev_to_dir(cache_dir, now_secs, fetcher)?;
    Ok((
        PricingCacheStatus {
            cache_path,
            cached_models: synced.models.len(),
            fetched_at: Some(synced.fetched_at),
            fresh: true,
            ttl_secs,
        },
        true,
    ))
}

/// Inspects the current local pricing cache status without performing network I/O.
fn pricing_cache_status(cache_dir: &Path, now_secs: u64) -> PricingCacheStatus {
    let cache_path = cache_dir.join(MODELS_DEV_CACHE_FILE);
    let cached = load_cached_file(&cache_path);
    let fetched_at = cached.as_ref().map(|c| c.fetched_at);
    let fresh = fetched_at.is_some_and(|t| now_secs.saturating_sub(t) < DEFAULT_PRICING_TTL_SECS);
    PricingCacheStatus {
        cache_path,
        cached_models: cached.map_or(0, |c| c.models.len()),
        fetched_at,
        fresh,
        ttl_secs: DEFAULT_PRICING_TTL_SECS,
    }
}

/// Runs the `agy-statusline pricing` subcommand (`status` or `sync [--if-stale]`).
pub fn run_pricing_command<W: io::Write>(
    cache_dir: &Path,
    sync: bool,
    if_stale: bool,
    out: &mut W,
) -> io::Result<()> {
    let now_secs = unix_now_secs();

    if sync {
        let (status, did_fetch) = sync_models_dev_pricing_with(
            cache_dir,
            if_stale,
            now_secs,
            DEFAULT_PRICING_TTL_SECS,
            fetch_models_dev_via_curl,
        )?;
        if did_fetch {
            writeln!(
                out,
                "Synced {} models from models.dev -> {}",
                status.cached_models,
                status.cache_path.display()
            )?;
        } else {
            writeln!(
                out,
                "Pricing cache is fresh ({} models, ttl {}h): {}",
                status.cached_models,
                status.ttl_secs / 3_600,
                status.cache_path.display()
            )?;
        }
        return Ok(());
    }

    let status = pricing_cache_status(cache_dir, now_secs);
    let age_desc = match status.fetched_at {
        Some(ts) => {
            let age = now_secs.saturating_sub(ts);
            let state = if status.fresh { "fresh" } else { "stale" };
            format!("{age}s ago ({state}, ttl {}h)", status.ttl_secs / 3_600)
        }
        None => "not cached".to_string(),
    };
    writeln!(out, "Pricing cache: {}", status.cache_path.display())?;
    writeln!(out, "Models cached: {} ({age_desc})", status.cached_models)?;
    Ok(())
}

pub(super) fn round_usd_6(v: f64) -> f64 {
    (v * 1_000_000.0).round() / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pricing_catalog_prefers_exact_specificity_then_longest_deterministic_substring() {
        let mut models = BTreeMap::new();
        models.insert(
            "claude-sonnet".to_string(),
            ModelPricingRate {
                input_per_mtok: 1.0,
                output_per_mtok: 2.0,
                ..Default::default()
            },
        );
        models.insert(
            "claude-sonnet-4-6-thinking".to_string(),
            ModelPricingRate {
                input_per_mtok: 10.0,
                output_per_mtok: 20.0,
                ..Default::default()
            },
        );
        models.insert(
            "alpha-a".to_string(),
            ModelPricingRate {
                input_per_mtok: 3.0,
                ..Default::default()
            },
        );
        models.insert(
            "alpha-b".to_string(),
            ModelPricingRate {
                input_per_mtok: 9.0,
                ..Default::default()
            },
        );
        let catalog = PricingCatalog::from_models(models);

        // Exact specificity: response_model ("claude-sonnet-4-6-thinking") beats model_group ("claude-sonnet")
        // even though "claude-sonnet" < "claude-sonnet-4-6-thinking" in BTreeMap order.
        let (matched_key, cost) = catalog
            .estimate_cost(
                &[
                    "claude-sonnet-4-6-thinking",
                    "Claude Sonnet 4.6 (Thinking)",
                    "claude-sonnet",
                ],
                1_000_000,
                0,
                0,
                0,
            )
            .unwrap();
        assert_eq!(matched_key, "claude-sonnet-4-6-thinking");
        assert!((cost - 10.0).abs() < 1e-6);

        // Across all candidates, a literal catalog ID precedes a punctuation-normalized match.
        assert_eq!(
            catalog.estimate_cost(&["alpha.b", "alpha-a"], 1_000_000, 0, 0, 0),
            Some(("alpha-a".to_owned(), 3.0))
        );

        // Substring fallback: longest match wins, and equal-length tie breaks by BTreeMap key order ("alpha-a" < "alpha-b").
        let (tie_key, tie_cost) = catalog
            .estimate_cost(&["alpha-b-and-alpha-a-model"], 1_000_000, 0, 0, 0)
            .unwrap();
        assert_eq!(tie_key, "alpha-a");
        assert!((tie_cost - 3.0).abs() < 1e-6);
    }

    #[test]
    fn models_dev_sync_respects_ttl_and_normalizes_model_names() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().join("cache");
        fs::create_dir_all(&cache_dir).unwrap();

        let sample_api = r#"{
          "reseller": {
            "models": {
              "gemini-4-hydrogen": { "cost": { "input": 99.0, "output": 99.0 } }
            }
          },
          "google": {
            "models": {
              "gemini-4-hydrogen": { "cost": { "input": 1.5, "output": 7.5, "cache_read": 0.075 } },
              "gemini-4-carbon": { "cost": { "input": 2.0, "output": 10.0, "cache_read": 0.1 } },
              "gemini-4-argon": { "cost": { "input": 2.0, "output": 10.0, "cache_read": 0.1 } }
            }
          },
          "anthropic": {
            "models": {
              "claude-opus-5-5": { "cost": { "input": 4.0, "output": 20.0, "cache_read": 0.2, "cache_write": 5.0 } }
            }
          }
        }"#;

        let t0 = 1_700_000_000;
        let mut fetch_calls = 0usize;
        let cat1 = PricingCatalog::load_effective_with(
            Some(&cache_dir),
            SyncMode::BlockingIfStale,
            t0,
            DEFAULT_PRICING_TTL_SECS,
            |_| {
                fetch_calls += 1;
                Ok(sample_api.to_string())
            },
        );
        assert_eq!(fetch_calls, 1);
        // Canonical provider (`google`) takes precedence over `reseller`.
        let (h_key, h_usd) = cat1
            .estimate_cost(&["gemini-4-hydrogen"], 1_000_000, 0, 0, 0)
            .unwrap();
        assert_eq!(h_key, "gemini-4-hydrogen");
        assert!((h_usd - 1.5).abs() < 1e-6);

        // Separator-normalized matching (`Claude Opus 5.5` -> `claude-opus-5-5`, `Gemini 4 Carbon (High)` -> `gemini-4-carbon`).
        let (_, opus_usd) = cat1
            .estimate_cost(&["Claude Opus 5.5"], 1_000_000, 0, 0, 0)
            .unwrap();
        assert!((opus_usd - 4.0).abs() < 1e-6);
        let (_, carbon_usd) = cat1
            .estimate_cost(&["Gemini 4 Carbon (High)"], 1_000_000, 0, 0, 0)
            .unwrap();
        assert!((carbon_usd - 2.0).abs() < 1e-6);

        // Orchestrator aliases map to their underlying Carbon/Argon models when no explicit rate exists.
        let (orch_c_key, orch_c_usd) = cat1
            .estimate_cost(
                &["gemini-pro-orchestrator", "Orchestrator (Carbon)"],
                1_000_000,
                0,
                0,
                0,
            )
            .unwrap();
        assert_eq!(orch_c_key, "gemini-4-carbon");
        assert!((orch_c_usd - 2.0).abs() < 1e-6);

        let (orch_a_key, orch_a_usd) = cat1
            .estimate_cost(
                &["gemini-pro-orchestrator-argon", "Orchestrator (Argon)"],
                1_000_000,
                0,
                0,
                0,
            )
            .unwrap();
        assert_eq!(orch_a_key, "gemini-4-argon");
        assert!((orch_a_usd - 2.0).abs() < 1e-6);

        // Within TTL: fetcher must NOT be called.
        let _cat2 = PricingCatalog::load_effective_with(
            Some(&cache_dir),
            SyncMode::BlockingIfStale,
            t0 + 100,
            DEFAULT_PRICING_TTL_SECS,
            |_| -> io::Result<String> { panic!("unexpected fetch while cache is fresh") },
        );
    }

    #[test]
    fn orchestrator_alias_resolves_dynamically_across_generations_without_hardcoded_fallbacks() {
        let mut models = BTreeMap::new();
        models.insert(
            "gemini-4-carbon".to_string(),
            ModelPricingRate {
                input_per_mtok: 2.0,
                output_per_mtok: 10.0,
                ..Default::default()
            },
        );
        models.insert(
            "Gemini 4 Argon".to_string(),
            ModelPricingRate {
                input_per_mtok: 4.0,
                output_per_mtok: 20.0,
                ..Default::default()
            },
        );
        models.insert(
            "gemini-4-neon".to_string(),
            ModelPricingRate {
                input_per_mtok: 3.0,
                output_per_mtok: 15.0,
                ..Default::default()
            },
        );
        models.insert(
            "gemini-5.2-neon".to_string(),
            ModelPricingRate {
                input_per_mtok: 5.5,
                output_per_mtok: 25.0,
                ..Default::default()
            },
        );
        // Non-canonical reseller/regional/variant keys must be ignored even with higher versions.
        models.insert(
            "vertex/gemini-9-neon".to_string(),
            ModelPricingRate {
                input_per_mtok: 99.0,
                output_per_mtok: 99.0,
                ..Default::default()
            },
        );
        models.insert(
            "gemini-9-neon@eu".to_string(),
            ModelPricingRate {
                input_per_mtok: 88.0,
                output_per_mtok: 88.0,
                ..Default::default()
            },
        );
        models.insert(
            "gemini-9-neon-contributor".to_string(),
            ModelPricingRate {
                input_per_mtok: 77.0,
                output_per_mtok: 77.0,
                ..Default::default()
            },
        );
        models.insert(
            "Gemini 4 Krypton".to_string(),
            ModelPricingRate {
                input_per_mtok: 6.25,
                output_per_mtok: 30.0,
                ..Default::default()
            },
        );
        let catalog = PricingCatalog::from_models(models);

        // Without an explicit generation, resolves dynamically to the latest canonical entry.
        let (neon_key, neon_usd) = catalog
            .estimate_cost(
                &["gemini-pro-orchestrator", "Orchestrator (Neon)"],
                1_000_000,
                0,
                0,
                0,
            )
            .unwrap();
        assert_eq!(neon_key, "gemini-5.2-neon");
        assert!((neon_usd - 5.5).abs() < 1e-6);

        let (neon_id_key, neon_id_usd) = catalog
            .estimate_cost(&["gemini-pro-orchestrator-neon"], 1_000_000, 0, 0, 0)
            .unwrap();
        assert_eq!(neon_id_key, "gemini-5.2-neon");
        assert!((neon_id_usd - 5.5).abs() < 1e-6);

        // Explicit generation pins to that exact generation rather than latest.
        let (neon_v4_key, neon_v4_usd) = catalog
            .estimate_cost(
                &["gemini-pro-orchestrator", "Gemini 4 Orchestrator (Neon)"],
                1_000_000,
                0,
                0,
                0,
            )
            .unwrap();
        assert_eq!(neon_v4_key, "gemini-4-neon");
        assert!((neon_v4_usd - 3.0).abs() < 1e-6);

        // Trailing date/build numbers after `orchestrator` are not treated as generation numbers.
        let (dated_key, dated_usd) = catalog
            .estimate_cost(
                &["gemini-pro-orchestrator-2026-04", "Orchestrator (Neon)"],
                1_000_000,
                0,
                0,
                0,
            )
            .unwrap();
        assert_eq!(dated_key, "gemini-5.2-neon");
        assert!((dated_usd - 5.5).abs() < 1e-6);

        // Missing explicit generation stays unpriced rather than crossing generations.
        assert_eq!(
            catalog.estimate_cost(
                &["gemini-pro-orchestrator", "Gemini 6 Orchestrator (Neon)"],
                1_000_000,
                0,
                0,
                0,
            ),
            None
        );

        // Matches separator-normalized catalog names and ignores effort qualifiers.
        let (argon_key, argon_usd) = catalog
            .estimate_cost(
                &["gemini-pro-orchestrator-g1-pro", "Orchestrator (Argon)"],
                1_000_000,
                0,
                0,
                0,
            )
            .unwrap();
        assert_eq!(argon_key, "Gemini 4 Argon");
        assert!((argon_usd - 4.0).abs() < 1e-6);

        let (krypton_key, krypton_usd) = catalog
            .estimate_cost(
                &["gemini-pro-orchestrator", "Orchestrator (Krypton) (High)"],
                1_000_000,
                0,
                0,
                0,
            )
            .unwrap();
        assert_eq!(krypton_key, "Gemini 4 Krypton");
        assert!((krypton_usd - 6.25).abs() < 1e-6);

        // Bare routing IDs without explicit codenames and unpriced codenames stay unpriced
        // (no hardcoded Carbon/Argon/Hydrogen guesses).
        assert_eq!(
            catalog.estimate_cost(&["gemini-pro-orchestrator"], 1_000_000, 0, 0, 0),
            None
        );
        assert_eq!(
            catalog.estimate_cost(
                &["gemini-pro-orchestrator", "Orchestrator (High)"],
                1_000_000,
                0,
                0,
                0,
            ),
            None
        );
        assert_eq!(
            catalog.estimate_cost(&["gemini-pro-orchestrator-g1-pro"], 1_000_000, 0, 0, 0),
            None
        );
        assert_eq!(
            catalog.estimate_cost(
                &["gemini-flash-worker-agy", "Gemini Flash"],
                1_000_000,
                0,
                0,
                0
            ),
            None
        );
        assert_eq!(
            catalog.estimate_cost(
                &["gemini-pro-orchestrator", "Orchestrator (Xenon)"],
                1_000_000,
                0,
                0,
                0,
            ),
            None
        );
    }

    #[test]
    fn orchestrator_alias_skips_unmatched_tokens_across_candidates_and_preserves_version_pinning() {
        let mut models = BTreeMap::new();
        models.insert(
            "gemini-4-carbon".to_string(),
            ModelPricingRate {
                input_per_mtok: 2.0,
                output_per_mtok: 10.0,
                ..Default::default()
            },
        );
        models.insert(
            "gemini-4-argon".to_string(),
            ModelPricingRate {
                input_per_mtok: 4.0,
                output_per_mtok: 20.0,
                ..Default::default()
            },
        );
        models.insert(
            "gemini-4-neon".to_string(),
            ModelPricingRate {
                input_per_mtok: 3.0,
                output_per_mtok: 15.0,
                ..Default::default()
            },
        );
        models.insert(
            "gemini-5.2-neon".to_string(),
            ModelPricingRate {
                input_per_mtok: 5.5,
                output_per_mtok: 25.0,
                ..Default::default()
            },
        );
        let catalog = PricingCatalog::from_models(models);

        // Unmatched routing tags after `orchestrator` (`-v2`, `-b`, `-agent`) must not shadow
        // a real codename in `model_display_name`.
        assert_eq!(
            catalog.estimate_cost(
                &["gemini-pro-orchestrator-v2", "Orchestrator (Argon)"],
                1_000_000,
                0,
                0,
                0,
            ),
            Some(("gemini-4-argon".to_owned(), 4.0))
        );
        assert_eq!(
            catalog.estimate_cost(
                &["gemini-pro-orchestrator-b", "Orchestrator (Argon)"],
                1_000_000,
                0,
                0,
                0,
            ),
            Some(("gemini-4-argon".to_owned(), 4.0))
        );
        assert_eq!(
            catalog.estimate_cost(
                &["gemini-pro-orchestrator-agent", "Orchestrator (Argon)"],
                1_000_000,
                0,
                0,
                0,
            ),
            Some(("gemini-4-argon".to_owned(), 4.0))
        );

        // Within a single candidate, skip unmatched `v2` and resolve trailing `Carbon`.
        assert_eq!(
            catalog.estimate_cost(&["Orchestrator (v2) (Carbon)"], 1_000_000, 0, 0, 0),
            Some(("gemini-4-carbon".to_owned(), 2.0))
        );

        // Explicit version from an earlier candidate pins generation across candidates.
        assert_eq!(
            catalog.estimate_cost(
                &["gemini-4-orchestrator-v2", "Orchestrator (Neon)"],
                1_000_000,
                0,
                0,
                0,
            ),
            Some(("gemini-4-neon".to_owned(), 3.0))
        );

        // When only gen 5 is in the catalog, pinning gen 4 across candidates refuses gen 5.
        let mut gen5_only = BTreeMap::new();
        gen5_only.insert(
            "gemini-5.2-neon".to_string(),
            ModelPricingRate {
                input_per_mtok: 5.5,
                output_per_mtok: 25.0,
                ..Default::default()
            },
        );
        let gen5_catalog = PricingCatalog::from_models(gen5_only);
        assert_eq!(
            gen5_catalog.estimate_cost(
                &["gemini-4-orchestrator-v2", "Orchestrator (Neon)"],
                1_000_000,
                0,
                0,
                0,
            ),
            None
        );
    }

    #[test]
    fn parse_models_dev_json_blocks_reseller_punctuation_collisions() {
        let sample_api = r#"{
          "google": {
            "models": {
              "gemini-3.8-flash": { "cost": { "input": 0.75, "output": 3.0 } }
            }
          },
          "frogbot": {
            "models": {
              "gemini-3-8-flash": { "cost": { "input": 1.50, "output": 6.0 } }
            }
          },
          "anthropic": {
            "models": {
              "claude-opus-5-5": { "cost": { "input": 4.00, "output": 20.0 } }
            }
          },
          "aigrow": {
            "models": {
              "claude-opus-5.5": { "cost": { "input": 1.60, "output": 8.0 } },
              "reseller-only-model": { "cost": { "input": 0.50, "output": 1.0 } }
            }
          }
        }"#;

        let parsed = parse_models_dev_json(sample_api).unwrap();
        assert!(parsed.contains_key("gemini-3.8-flash"));
        assert!(!parsed.contains_key("gemini-3-8-flash"));
        assert!(parsed.contains_key("claude-opus-5-5"));
        assert!(!parsed.contains_key("claude-opus-5.5"));
        assert!(parsed.contains_key("reseller-only-model"));

        let catalog = PricingCatalog::from_models(parsed);
        let (flash_key, flash_usd) = catalog
            .estimate_cost(&["gemini-3.8-flash-tiered"], 1_000_000, 0, 0, 0)
            .unwrap();
        assert_eq!(flash_key, "gemini-3.8-flash");
        assert!((flash_usd - 0.75).abs() < 1e-6);

        let (opus_key, opus_usd) = catalog
            .estimate_cost(&["Claude Opus 5.5"], 1_000_000, 0, 0, 0)
            .unwrap();
        assert_eq!(opus_key, "claude-opus-5-5");
        assert!((opus_usd - 4.0).abs() < 1e-6);
    }

    #[test]
    fn substring_fallback_ranks_raw_match_above_normalized_match() {
        let mut models = BTreeMap::new();
        models.insert(
            "gemini-3-8-flash".to_string(),
            ModelPricingRate {
                input_per_mtok: 1.50,
                output_per_mtok: 6.0,
                ..Default::default()
            },
        );
        models.insert(
            "gemini-3.8-flash".to_string(),
            ModelPricingRate {
                input_per_mtok: 0.75,
                output_per_mtok: 3.0,
                ..Default::default()
            },
        );
        let catalog = PricingCatalog::from_models(models);

        let (dot_key, dot_usd) = catalog
            .estimate_cost(&["gemini-3.8-flash-tiered"], 1_000_000, 0, 0, 0)
            .unwrap();
        assert_eq!(dot_key, "gemini-3.8-flash");
        assert!((dot_usd - 0.75).abs() < 1e-6);

        let (dash_key, dash_usd) = catalog
            .estimate_cost(&["gemini-3-8-flash-tiered"], 1_000_000, 0, 0, 0)
            .unwrap();
        assert_eq!(dash_key, "gemini-3-8-flash");
        assert!((dash_usd - 1.50).abs() < 1e-6);

        // Longer normalized substring (`o3-pro`, len 6) still beats shorter raw substring (`o3`, len 2).
        let mut short_vs_long = BTreeMap::new();
        short_vs_long.insert(
            "o3".to_string(),
            ModelPricingRate {
                input_per_mtok: 2.0,
                output_per_mtok: 8.0,
                ..Default::default()
            },
        );
        short_vs_long.insert(
            "o3-pro".to_string(),
            ModelPricingRate {
                input_per_mtok: 20.0,
                output_per_mtok: 80.0,
                ..Default::default()
            },
        );
        let short_vs_long_catalog = PricingCatalog::from_models(short_vs_long);
        let (o3_key, o3_usd) = short_vs_long_catalog
            .estimate_cost(&["o3 Pro (High)"], 1_000_000, 0, 0, 0)
            .unwrap();
        assert_eq!(o3_key, "o3-pro");
        assert!((o3_usd - 20.0).abs() < 1e-6);
    }

    #[test]
    fn context_tiered_pricing_applies_cost_tiers_and_over_200k_fallback() {
        let sample_api = r#"{
          "openai": {
            "models": {
              "gpt-5.4": {
                "cost": {
                  "input": 2.5,
                  "output": 15.0,
                  "cache_read": 0.25,
                  "cache_write": 3.0,
                  "tiers": [
                    { "tier": { "type": "batch", "size": 1000 }, "input": 99.0 },
                    { "tier": { "type": "context", "size": 272000 }, "input": 5.0, "output": 22.5, "cache_read": 0.5 }
                  ]
                }
              }
            }
          },
          "anthropic": {
            "models": {
              "claude-haiku-next": {
                "cost": {
                  "input": 1.0,
                  "output": 5.0,
                  "cache_read": 0.1,
                  "cache_write": 1.25,
                  "tiers": [
                    { "tier": { "type": "context", "size": 200000 }, "input": 3.0, "output": 15.0 },
                    { "tier": { "type": "context", "size": 100000 }, "input": 2.0, "output": 10.0 }
                  ],
                  "context_over_200k": { "input": 99.0, "output": 99.0 }
                }
              },
              "claude-sonnet-legacy": {
                "cost": {
                  "input": 3.0,
                  "output": 15.0,
                  "cache_read": 0.3,
                  "cache_write": 3.75,
                  "context_over_200k": { "input": 6.0, "output": 22.5, "cache_read": 0.6 }
                }
              }
            }
          }
        }"#;

        let catalog = PricingCatalog::from_models(parse_models_dev_json(sample_api).unwrap());

        // OpenAI 272K context tier: <= 272_000 uses base rate, > 272_000 uses tier rate (inheriting cache_write).
        let (_, openai_at_threshold) = catalog
            .estimate_cost(&["gpt-5.4"], 172_000, 100_000, 0, 100_000)
            .unwrap();
        assert!((openai_at_threshold - 1.955).abs() < 1e-6);

        let (_, openai_above_threshold) = catalog
            .estimate_cost(&["gpt-5.4"], 172_001, 100_000, 0, 100_000)
            .unwrap();
        assert!((openai_above_threshold - 3.160005).abs() < 1e-6);

        let (_, openai_inherited_cache_write) = catalog
            .estimate_cost(&["gpt-5.4"], 200_000, 0, 100_000, 0)
            .unwrap();
        assert!((openai_inherited_cache_write - 1.3).abs() < 1e-6);

        // Multi-tier 100K/200K model (`tiers` takes precedence over `context_over_200k`).
        let (_, haiku_at_100k) = catalog
            .estimate_cost(&["claude-haiku-next"], 100_000, 0, 0, 0)
            .unwrap();
        assert!((haiku_at_100k - 0.1).abs() < 1e-6);

        let (_, haiku_above_100k) = catalog
            .estimate_cost(&["claude-haiku-next"], 100_001, 0, 0, 0)
            .unwrap();
        assert!((haiku_above_100k - 0.200002).abs() < 1e-6);

        let (_, haiku_at_200k) = catalog
            .estimate_cost(&["claude-haiku-next"], 200_000, 0, 0, 0)
            .unwrap();
        assert!((haiku_at_200k - 0.4).abs() < 1e-6);

        let (_, haiku_above_200k) = catalog
            .estimate_cost(&["claude-haiku-next"], 200_001, 0, 0, 0)
            .unwrap();
        assert!((haiku_above_200k - 0.600003).abs() < 1e-6);

        // Legacy `context_over_200k` fallback when `tiers` is absent.
        let (_, legacy_at_200k) = catalog
            .estimate_cost(&["claude-sonnet-legacy"], 100_000, 50_000, 50_000, 0)
            .unwrap();
        assert!((legacy_at_200k - 0.5025).abs() < 1e-6);

        let (_, legacy_above_200k) = catalog
            .estimate_cost(&["claude-sonnet-legacy"], 100_001, 50_000, 50_000, 0)
            .unwrap();
        assert!((legacy_above_200k - 0.817506).abs() < 1e-6);
    }
}
