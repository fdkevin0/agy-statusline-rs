//! Model token pricing catalog (TTL-cached `models.dev` rates).

use std::collections::{BTreeMap, HashMap};
use std::env;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::fsutil::atomic_write;
use crate::paths::xdg_cache_dir;
use crate::timeutil::{unix_now_secs, SECS_PER_DAY};

/// Default cache TTL for auto-fetched `models.dev` pricing data (24 hours).
pub(super) const DEFAULT_PRICING_TTL_SECS: u64 = SECS_PER_DAY as u64;
/// Cooldown after a failed network fetch before retrying automatically (5 minutes).
const PRICING_RETRY_COOLDOWN_SECS: u64 = 300;
/// Public unauthenticated `models.dev` API endpoint.
const MODELS_DEV_API_URL: &str = "https://models.dev/api.json";
const MODELS_DEV_CACHE_FILE: &str = "models_dev_pricing.json";
const MODELS_DEV_ERR_FILE: &str = "models_dev_pricing.err";
const MAX_PRICING_JSON_BYTES: u64 = 16 * 1024 * 1024;

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

        for (pattern, rate) in models {
            if !is_valid_rate(&rate) {
                continue;
            }
            let pat_lower = pattern.trim().to_ascii_lowercase();
            if pat_lower.is_empty() {
                continue;
            }
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

    /// Loads rates from the models.dev cache using the requested refresh mode.
    pub(crate) fn load_effective(mode: SyncMode, now_secs: u64) -> Self {
        let cache_dir = xdg_cache_dir();
        Self::load_effective_with(
            cache_dir.as_deref(),
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
    pub(crate) fn estimate_cost(
        &self,
        candidates: &[&str],
        input_tokens: u64,
        cache_read_tokens: u64,
        cache_write_tokens: u64,
        output_tokens: u64,
    ) -> Option<(String, f64)> {
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
                lowered_candidates.iter().find_map(|(_, norm)| self.exact_norm.get(norm))
            })
            .map(|&idx| &self.entries[idx]);

        // Substring fallback prefers the longest match, then key order.
        if matched.is_none() {
            let mut best: Option<(usize, &IndexedEntry)> = None;
            for entry in &self.entries {
                let match_len = if lowered_candidates.iter().any(|(raw, _)| raw.contains(&entry.pat_lower)) {
                    entry.pat_lower.len()
                } else if !entry.pat_norm.is_empty()
                    && lowered_candidates.iter().any(|(_, norm)| norm.contains(&entry.pat_norm))
                {
                    entry.pat_norm.len()
                } else {
                    continue;
                };
                if best.is_none_or(|(len, previous)| {
                    match_len > len || (match_len == len && entry.key < previous.key)
                }) {
                    best = Some((match_len, entry));
                }
            }
            matched = best.map(|(_, e)| e);
        }

        // 3. Orchestrator alias fallback.
        let entry = matched.or_else(|| self.resolve_orchestrator_alias(&lowered_candidates))?;
        let cost = ((input_tokens as f64) * entry.rate.input_per_mtok
            + (cache_read_tokens as f64) * entry.rate.cache_read_per_mtok
            + (cache_write_tokens as f64) * entry.rate.cache_write_per_mtok
            + (output_tokens as f64) * entry.rate.output_per_mtok)
            / 1_000_000.0;
        if !cost.is_finite() || cost < 0.0 {
            return None;
        }
        Some((entry.key.clone(), round_usd_6(cost)))
    }

    fn resolve_orchestrator_alias(&self, candidates: &[(String, String)]) -> Option<&IndexedEntry> {
        let lookup = |raw: &str, norm: &str| {
            self.exact_raw
                .get(raw)
                .or_else(|| self.exact_norm.get(norm))
                .map(|&idx| &self.entries[idx])
        };

        // Prefer explicit Argon/Carbon qualifiers across all candidates before falling back
        // to the bare `gemini-pro-orchestrator` alias.
        candidates
            .iter()
            .find_map(|(_, norm)| {
                if !norm.split_whitespace().any(|w| w == "orchestrator") {
                    return None;
                }
                if norm.split_whitespace().any(|w| w == "argon")
                    || norm == "gemini pro orchestrator g1 pro"
                {
                    lookup("gemini-4-argon", "gemini 4 argon")
                } else if norm.split_whitespace().any(|w| w == "carbon") {
                    lookup("gemini-4-carbon", "gemini 4 carbon")
                } else {
                    None
                }
            })
            .or_else(|| {
                candidates.iter().find_map(|(_, norm)| {
                    (norm == "gemini pro orchestrator")
                        .then(|| lookup("gemini-4-carbon", "gemini 4 carbon"))
                        .flatten()
                })
            })
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

/// Fetches `url` using `curl` with bounded connect/total timeouts and response size limit.
pub(super) fn fetch_models_dev_via_curl(url: &str) -> io::Result<String> {
    let mut child = Command::new("curl")
        .args(["-fsSL", "--connect-timeout", "5", "--max-time", "15", url])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let mut stdout_buf = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        if let Err(err) = stdout
            .take(MAX_PRICING_JSON_BYTES + 1)
            .read_to_end(&mut stdout_buf)
        {
            let _ = child.kill();
            let _ = child.wait();
            return Err(err);
        }
    }

    if stdout_buf.len() as u64 > MAX_PRICING_JSON_BYTES {
        let _ = child.kill();
        let _ = child.wait();
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "models.dev response exceeded size limit",
        ));
    }

    let mut stderr_buf = Vec::new();
    if let Some(stderr) = child.stderr.take() {
        let _ = stderr.take(4096).read_to_end(&mut stderr_buf);
    }

    let status = child.wait()?;
    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr_buf);
        return Err(io::Error::other(format!(
            "curl failed ({status}): {}",
            stderr.trim()
        )));
    }

    String::from_utf8(stdout_buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
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
    let target = cache_dir.join(MODELS_DEV_CACHE_FILE);
    let encoded = serde_json::to_string(&cached)
        .map_err(|e| io::Error::other(format!("serialize pricing cache: {e}")))?;
    atomic_write(&target, encoded.as_bytes())?;
    clear_retry_cooldown(cache_dir);
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
fn pricing_cache_status(now_secs: u64) -> Option<PricingCacheStatus> {
    let dir = xdg_cache_dir()?;
    let cache_path = dir.join(MODELS_DEV_CACHE_FILE);
    let cached = load_cached_file(&cache_path);
    let fetched_at = cached.as_ref().map(|c| c.fetched_at);
    let fresh = fetched_at.is_some_and(|t| now_secs.saturating_sub(t) < DEFAULT_PRICING_TTL_SECS);
    Some(PricingCacheStatus {
        cache_path,
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
    let cache_dir = xdg_cache_dir().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "unable to resolve pricing cache directory",
        )
    })?;

    if sync {
        let (status, did_fetch) = sync_models_dev_pricing_with(
            &cache_dir,
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
    Ok(())
}

pub(super) fn round_usd_6(v: f64) -> f64 {
    if v.is_finite() {
        (v * 1_000_000.0).round() / 1_000_000.0
    } else {
        0.0
    }
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
    fn orchestrator_alias_prefers_explicit_qualifier_and_matches_normalized_catalog_name() {
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
                input_per_mtok: 7.5,
                output_per_mtok: 15.0,
                ..Default::default()
            },
        );
        let catalog = PricingCatalog::from_models(models);

        // Even when `gemini-4-carbon` is present, an explicit Argon display name wins over
        // the bare `gemini-pro-orchestrator` ID.
        let (matched_key, usd) = catalog
            .estimate_cost(
                &["gemini-pro-orchestrator", "Orchestrator (Argon)"],
                1_000_000,
                0,
                0,
                0,
            )
            .unwrap();
        assert_eq!(matched_key, "Gemini 4 Argon");
        assert!((usd - 7.5).abs() < 1e-6);

        // Without an explicit Argon qualifier, bare `gemini-pro-orchestrator` still resolves to Carbon.
        let (carbon_key, carbon_usd) = catalog
            .estimate_cost(&["gemini-pro-orchestrator"], 1_000_000, 0, 0, 0)
            .unwrap();
        assert_eq!(carbon_key, "gemini-4-carbon");
        assert!((carbon_usd - 2.0).abs() < 1e-6);
    }
}
