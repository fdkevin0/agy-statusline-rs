//! Model token pricing catalog (`pricing.json` + TTL-cached `models.dev` sync).

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::db::resolve_home_dir;
use crate::timeutil::unix_now_secs;

/// Default cache TTL for auto-fetched `models.dev` pricing data (24 hours).
pub const DEFAULT_PRICING_TTL_SECS: u64 = 86_400;
/// Cooldown after a failed network fetch before retrying automatically (5 minutes).
const PRICING_RETRY_COOLDOWN_SECS: u64 = 300;
/// Public unauthenticated `models.dev` API endpoint.
pub const MODELS_DEV_API_URL: &str = "https://models.dev/api.json";
const MODELS_DEV_CACHE_FILE: &str = "models_dev_pricing.json";
const MODELS_DEV_ERR_FILE: &str = "models_dev_pricing.err";
const MAX_PRICING_JSON_BYTES: u64 = 16 * 1024 * 1024;

/// Controls how [`PricingCatalog::load_effective`] handles missing or expired cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncMode {
    /// Never perform network I/O or spawn background sync; read local files only.
    Offline,
    /// Synchronously refresh from `models.dev` if the cache is missing or past TTL.
    BlockingIfStale,
    /// Spawn a detached background `pricing sync --if-stale` process if missing or past TTL.
    SpawnIfStale,
}

/// Per-million-token pricing rates supplied by user-configured `pricing.json` or `models.dev`.
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

/// Persisted `models.dev` cache envelope stored in `$XDG_CACHE_HOME/agy-statusline/models_dev_pricing.json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct CachedPricingFile {
    fetched_at: u64,
    #[serde(default)]
    models: BTreeMap<String, ModelPricingRate>,
}

/// Diagnostic summary of the local pricing cache and user override file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PricingCacheStatus {
    /// Path to `models_dev_pricing.json`.
    pub cache_path: PathBuf,
    /// Path to user override `pricing.json`.
    pub override_path: Option<PathBuf>,
    /// Number of cached `models.dev` entries (`0` if missing).
    pub cached_models: usize,
    /// Unix timestamp when cache was last fetched, if present.
    pub fetched_at: Option<u64>,
    /// Whether the cache exists and is within `ttl_secs` at `now_secs`.
    pub fresh: bool,
    /// Configured TTL in seconds.
    pub ttl_secs: u64,
}

/// Optional user-defined and cached pricing catalog.
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

    /// Loads effective pricing (`models.dev` cache + user `pricing.json` overrides).
    pub fn load_effective(mode: SyncMode, now_secs: u64) -> Self {
        let cache_dir = default_pricing_cache_dir();
        let override_path = default_pricing_path();
        Self::load_effective_with(
            cache_dir.as_deref(),
            override_path.as_deref(),
            mode,
            now_secs,
            DEFAULT_PRICING_TTL_SECS,
            fetch_models_dev_via_curl,
        )
    }

    /// Loads effective pricing with explicit paths, TTL, and fetcher boundary for deterministic tests.
    pub fn load_effective_with<F>(
        cache_dir: Option<&Path>,
        override_path: Option<&Path>,
        mode: SyncMode,
        now_secs: u64,
        ttl_secs: u64,
        fetcher: F,
    ) -> Self
    where
        F: FnOnce(&str) -> io::Result<String>,
    {
        let mut merged = BTreeMap::new();

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
                merged.extend(c.models);
            }
        }

        if let Some(path) = override_path {
            let user_cfg = Self::load_from_file(path);
            for (k, v) in user_cfg.models {
                merged.insert(k, v);
            }
        }

        Self { models: merged }
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

        let valid_entries: Vec<(&str, String, String, &ModelPricingRate)> = self
            .models
            .iter()
            .filter(|(_, rate)| is_valid_rate(rate))
            .filter_map(|(pattern, rate)| {
                let pat_lower = pattern.trim().to_ascii_lowercase();
                if pat_lower.is_empty() {
                    return None;
                }
                let pat_norm = normalize_model_key(&pat_lower);
                Some((pattern.as_str(), pat_lower, pat_norm, rate))
            })
            .collect();

        // 1. Exact matches (raw lowercase, then separator-normalized) in candidate specificity order.
        let mut exact_match: Option<(&str, &ModelPricingRate)> = None;
        for (cand_raw, cand_norm) in &lowered_candidates {
            if let Some((key, _, _, rate)) = valid_entries
                .iter()
                .find(|(_, pat_raw, pat_norm, _)| pat_raw == cand_raw || (!cand_norm.is_empty() && pat_norm == cand_norm))
            {
                exact_match = Some((*key, *rate));
                break;
            }
        }

        // 2. Substring fallback (raw or separator-normalized) prefers the longest pattern match.
        let mut substr_match: Option<(usize, &str, &ModelPricingRate)> = None;
        if exact_match.is_none() {
            for (pattern, pat_lower, pat_norm, rate) in &valid_entries {
                let matched = lowered_candidates.iter().any(|(cand_raw, cand_norm)| {
                    cand_raw.contains(pat_lower.as_str())
                        || (!pat_norm.is_empty() && cand_norm.contains(pat_norm.as_str()))
                });
                if matched {
                    let match_len = pat_lower.len().max(pat_norm.len());
                    let replace = match substr_match {
                        None => true,
                        Some((best_len, best_key, _)) => {
                            match_len > best_len
                                || (match_len == best_len && *pattern < best_key)
                        }
                    };
                    if replace {
                        substr_match = Some((match_len, *pattern, *rate));
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

fn normalize_model_key(raw: &str) -> String {
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

fn load_cached_file(path: &Path) -> Option<CachedPricingFile> {
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str::<CachedPricingFile>(&raw).ok()
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
    if fs::create_dir_all(cache_dir).is_ok() {
        let _ = fs::write(
            cache_dir.join(MODELS_DEV_ERR_FILE),
            format!("{now_secs}\n"),
        );
    }
}

fn clear_retry_cooldown(cache_dir: &Path) {
    let _ = fs::remove_file(cache_dir.join(MODELS_DEV_ERR_FILE));
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

/// Fetches `url` using `curl` with a bounded 8-second timeout.
pub fn fetch_models_dev_via_curl(url: &str) -> io::Result<String> {
    let output = Command::new("curl")
        .args(["-fsSL", "--max-time", "8", url])
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(io::Error::other(format!(
            "curl failed ({}): {}",
            output.status,
            stderr.trim()
        )));
    }
    if output.stdout.len() as u64 > MAX_PRICING_JSON_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "models.dev response exceeded size limit",
        ));
    }
    String::from_utf8(output.stdout).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
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

fn extract_rate_from_cost(cost_obj: &serde_json::Map<String, Value>) -> Option<ModelPricingRate> {
    let input = cost_obj.get("input").and_then(Value::as_f64).unwrap_or(0.0);
    let output = cost_obj.get("output").and_then(Value::as_f64).unwrap_or(0.0);
    let cache_read = cost_obj
        .get("cache_read")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let cache_write = cost_obj
        .get("cache_write")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);

    let rate = ModelPricingRate {
        input_per_mtok: input,
        cache_read_per_mtok: cache_read,
        cache_write_per_mtok: cache_write,
        output_per_mtok: output,
    };
    if !is_valid_rate(&rate) || (input == 0.0 && output == 0.0) {
        return None;
    }
    Some(rate)
}

/// Parses `https://models.dev/api.json` payload into a flat `model_id -> ModelPricingRate` map.
///
/// Canonical direct providers (`google`, `anthropic`, `openai`, etc.) take precedence over
/// aggregator/cloud-reseller providers when identical model IDs appear under multiple providers.
pub fn parse_models_dev_json(raw: &str) -> io::Result<BTreeMap<String, ModelPricingRate>> {
    let root: Value = serde_json::from_str(raw)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("invalid JSON: {e}")))?;
    let Some(providers) = root.as_object() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected top-level JSON object from models.dev",
        ));
    };

    let mut out = BTreeMap::new();

    // Pass 1: canonical direct providers take precedence.
    for &prov_id in CANONICAL_PROVIDERS {
        if let Some(prov_obj) = providers.get(prov_id).and_then(Value::as_object) {
            insert_provider_models(prov_obj, &mut out, true);
        }
    }
    // Pass 2: remaining providers fill in any models not already present.
    for (prov_id, prov_val) in providers {
        if CANONICAL_PROVIDERS.contains(&prov_id.as_str()) {
            continue;
        }
        if let Some(prov_obj) = prov_val.as_object() {
            insert_provider_models(prov_obj, &mut out, false);
        }
    }

    Ok(out)
}

fn insert_provider_models(
    prov_obj: &serde_json::Map<String, Value>,
    out: &mut BTreeMap<String, ModelPricingRate>,
    overwrite: bool,
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
        if overwrite || !out.contains_key(&key) {
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
    fs::create_dir_all(cache_dir)?;
    let target = cache_dir.join(MODELS_DEV_CACHE_FILE);
    let tmp = cache_dir.join(format!("{MODELS_DEV_CACHE_FILE}.{}.tmp", std::process::id()));
    let encoded = serde_json::to_string(&cached)
        .map_err(|e| io::Error::other(format!("serialize pricing cache: {e}")))?;
    fs::write(&tmp, encoded.as_bytes())?;
    fs::rename(&tmp, &target)?;
    clear_retry_cooldown(cache_dir);
    Ok(cached)
}

/// Syncs `models.dev` pricing into the cache directory, returning `(status, did_fetch)`.
pub fn sync_models_dev_pricing_with<F>(
    cache_dir: &Path,
    override_path: Option<&Path>,
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
                        override_path: override_path.map(Path::to_path_buf),
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
            override_path: override_path.map(Path::to_path_buf),
            cached_models: synced.models.len(),
            fetched_at: Some(synced.fetched_at),
            fresh: true,
            ttl_secs,
        },
        true,
    ))
}

/// Inspects the current local pricing cache status without performing network I/O.
pub fn pricing_cache_status(now_secs: u64) -> Option<PricingCacheStatus> {
    let dir = default_pricing_cache_dir()?;
    let cache_path = dir.join(MODELS_DEV_CACHE_FILE);
    let cached = load_cached_file(&cache_path);
    let fetched_at = cached.as_ref().map(|c| c.fetched_at);
    let fresh = fetched_at.is_some_and(|t| now_secs.saturating_sub(t) < DEFAULT_PRICING_TTL_SECS);
    Some(PricingCacheStatus {
        cache_path,
        override_path: default_pricing_path(),
        cached_models: cached.map_or(0, |c| c.models.len()),
        fetched_at,
        fresh,
        ttl_secs: DEFAULT_PRICING_TTL_SECS,
    })
}

/// Runs the `agy-statusline pricing` subcommand (`status` or `sync [--if-stale]`).
pub fn run_pricing_command<W: io::Write>(
    sync: bool,
    if_stale: bool,
    out: &mut W,
) -> io::Result<()> {
    let now_secs = unix_now_secs();
    let cache_dir = default_pricing_cache_dir().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "unable to resolve pricing cache directory",
        )
    })?;
    let override_path = default_pricing_path();

    if sync {
        let (status, did_fetch) = sync_models_dev_pricing_with(
            &cache_dir,
            override_path.as_deref(),
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

    let status = pricing_cache_status(now_secs).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "unable to resolve pricing cache status",
        )
    })?;
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
    if let Some(ov) = status.override_path {
        let exists = if ov.is_file() { "present" } else { "not set" };
        writeln!(out, "User override: {} ({exists})", ov.display())?;
    }
    Ok(())
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

/// Returns the default `models.dev` pricing cache directory
/// (`$XDG_CACHE_HOME/agy-statusline` or `~/.cache/agy-statusline`).
pub fn default_pricing_cache_dir() -> Option<PathBuf> {
    let cache_base = env::var_os("XDG_CACHE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| resolve_home_dir().map(|h| h.join(".cache")))?;
    Some(cache_base.join("agy-statusline"))
}

pub(super) fn round_usd_6(v: f64) -> f64 {
    if v.is_finite() {
        (v * 1_000_000.0).round() / 1_000_000.0
    } else {
        0.0
    }
}
