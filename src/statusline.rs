//! Null-safe AGY statusline JSON schema and pipeline orchestration.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::Path;

use serde::Deserialize;
use serde_json::Value;

use crate::db::{
    open_readonly_db, query_session_metadata_from_conn, resolve_conversation_db_path,
    SessionMetadata, SpeedReader,
};
use crate::git::{GitInfo, GitProbe};
use crate::layout::{LayoutConfig, WidgetId};
use crate::render::{write_statusline_at, CostCoverage};
use crate::stats::{
    estimate_conversation_cost_usd_with_parent_conn, is_pricing_fingerprint_fresh,
    lookup_cached_conversation_cost, maybe_spawn_pricing_sync_if_stale,
    pricing_cache_fingerprint, PricingCatalog, PricingFileFingerprint, SyncMode,
};

/// Active AI model metadata (`"model": null` before a model is selected).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ModelInfo {
    /// Model identifier.
    pub id: Option<String>,
    /// Formatted display name shown in the TUI.
    pub display_name: Option<String>,
    /// Reasoning effort level (`low`, `medium`, `high`, `max`).
    pub effort: Option<String>,
}

/// Git / VCS status metadata.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct VcsInfo {
    /// VCS type (`"git"`, `"hg"`, `"citc"`, etc.).
    #[serde(rename = "type")]
    pub vcs_type: Option<String>,
    /// Active branch name.
    pub branch: Option<String>,
    /// VCS client / workspace name (used by non-Git VCS such as CitC or Perforce).
    pub client: Option<String>,
    /// Whether the working tree has uncommitted changes.
    pub dirty: Option<bool>,
}

/// Active agent metadata.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct AgentInfo {
    /// Active agent name.
    pub name: Option<String>,
}

/// Sandbox mode status.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default)]
pub struct SandboxInfo {
    /// Whether terminal sandbox is enabled.
    pub enabled: Option<bool>,
    /// Whether network access is permitted inside the sandbox.
    pub allow_network: Option<bool>,
}

/// Vim editing mode status (`normal`, `insert`, `visual`, etc.).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct VimInfo {
    /// Active Vim mode name.
    pub mode: Option<String>,
}

/// Battle / multi-arm comparison mode status.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct BattleInfo {
    /// Battle mode status string.
    pub status: Option<String>,
    /// Currently focused arm index.
    pub focused_arm: Option<u64>,
}

/// Active workspace directory paths.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct WorkspaceInfo {
    /// Active working directory (`cwd`).
    pub current_dir: Option<String>,
    /// Root project directory.
    pub project_dir: Option<String>,
}

/// Token breakdown for the latest turn under `context_window.current_usage`.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default)]
pub struct JsonUsage {
    /// Uncached input tokens.
    pub input_tokens: Option<i64>,
    /// Output tokens generated in the turn.
    pub output_tokens: Option<i64>,
    /// Cache read input tokens.
    pub cache_read_input_tokens: Option<i64>,
    /// Cache creation input tokens.
    pub cache_creation_input_tokens: Option<i64>,
}

/// Context window capacity and consumption metrics.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default)]
pub struct ContextWindow {
    /// Maximum context window token capacity.
    pub context_window_size: Option<u64>,
    /// Percentage of the context window currently consumed (`0.0..=100.0`).
    pub used_percentage: Option<f64>,
    /// Cumulative input tokens over the session.
    pub total_input_tokens: Option<u64>,
    /// Cumulative output tokens over the session.
    pub total_output_tokens: Option<u64>,
    /// Token breakdown for the most recent turn.
    pub current_usage: Option<JsonUsage>,
}

/// Estimated session cost based on public model list pricing.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default)]
pub struct CostInfo {
    /// Estimated total spend in USD (including sub-agents).
    pub total_usd: Option<f64>,
    /// Estimated spend driven by background sub-agents.
    pub subagent_usd: Option<f64>,
    /// Whether cost figures are list-price estimates.
    pub estimated: Option<bool>,
}

/// Rolling rate-limit bucket from AGY `quota`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct QuotaBucket {
    /// Fraction of quota remaining (`0.0..=1.0`).
    pub remaining_fraction: Option<f64>,
    /// Remaining credit amount when the bucket reports discrete units instead of a fraction.
    pub remaining_amount: Option<i64>,
    /// Countdown in seconds until the bucket resets (may be negative right after reset).
    pub reset_in_seconds: Option<i64>,
    /// ISO-8601 reset timestamp.
    pub reset_time: Option<String>,
    /// Whether this bucket is disabled by the server.
    pub disabled: Option<bool>,
}

/// Null-safe top-level AGY statusline JSON payload passed via `stdin`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct StatuslinePayload {
    /// Agent execution state (`idle`, `thinking`, `working`, `tool_use`, `reviewing`, etc.).
    pub agent_state: Option<String>,
    /// Active session identifier.
    pub session_id: Option<String>,
    /// Active conversation UUID.
    pub conversation_id: Option<String>,
    /// Human-readable title for the active conversation.
    pub conversation_title: Option<String>,
    /// Filesystem path to the active conversation transcript (`transcript.jsonl`).
    pub transcript_path: Option<String>,
    /// AGY CLI version string (`"1.5.11"`).
    pub version: Option<String>,
    /// Product name string (e.g. `"Antigravity"`).
    pub product: Option<String>,
    /// Top-level current working directory fallback.
    pub cwd: Option<String>,
    /// Active agent cycle mode (`plan`, `auto`, `edit`, etc.).
    pub cycle_mode: Option<String>,
    /// Active account plan tier (`Pro`, `Ultra`, etc.).
    pub plan_tier: Option<String>,
    /// Authenticated user email address.
    pub email: Option<String>,
    /// Active agent metadata.
    pub agent: Option<AgentInfo>,
    /// Whether context exceeds 200K tokens.
    pub exceeds_200k_tokens: Option<bool>,
    /// Terminal column width.
    pub terminal_width: Option<usize>,
    /// Number of generated artifacts.
    pub artifact_count: Option<u64>,
    /// Background task count.
    pub task_count: Option<u64>,
    /// Whether a tool execution is awaiting user confirmation.
    pub tool_confirmation_pending: Option<bool>,
    /// Number of queued user inputs waiting to be processed.
    pub pending_input_count: Option<u64>,
    /// Subagent array or null.
    pub subagents: Option<Value>,
    /// Active model info (`null` when unset).
    pub model: Option<ModelInfo>,
    /// Git / VCS status (`null` outside repos).
    pub vcs: Option<VcsInfo>,
    /// Sandbox status.
    pub sandbox: Option<SandboxInfo>,
    /// Vim editing mode state.
    pub vim: Option<VimInfo>,
    /// Battle mode state.
    pub battle: Option<BattleInfo>,
    /// Workspace directory paths.
    pub workspace: Option<WorkspaceInfo>,
    /// Context window metrics.
    pub context_window: Option<ContextWindow>,
    /// Session cost estimate.
    pub cost: Option<CostInfo>,
    /// Quota buckets keyed by server bucket ID.
    pub quota: Option<BTreeMap<String, QuotaBucket>>,
}

impl StatuslinePayload {
    /// Returns the number of active subagents when `subagents` is a JSON array.
    ///
    /// For objects containing a string `"status"` field, counts only entries where
    /// `status != "completed"`; entries lacking `"status"` fall back to direct counting.
    pub(crate) fn subagent_count(&self) -> usize {
        self.active_subagents().count()
    }

    /// Returns the role or name of the sole active subagent when `subagent_count() == 1`.
    pub(crate) fn single_active_subagent_label(&self) -> Option<&str> {
        let mut iter = self.active_subagents();
        let first = iter.next()?;
        if iter.next().is_some() {
            return None;
        }
        let obj = first.as_object()?;
        obj.get("role")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .or_else(|| {
                obj.get("name")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
            })
    }

    fn active_subagents(&self) -> impl Iterator<Item = &Value> {
        self.subagents
            .as_ref()
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|item| {
                item.as_object()
                    .and_then(|obj| obj.get("status"))
                    .and_then(Value::as_str)
                    .is_none_or(|status| !status.eq_ignore_ascii_case("completed"))
            })
    }

    /// Returns the trimmed workspace directory path preferring `workspace.current_dir`,
    /// then `workspace.project_dir`, then top-level `cwd`.
    fn workspace_path(&self) -> Option<&str> {
        self.workspace
            .as_ref()
            .and_then(|ws| {
                ws.current_dir
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .or_else(|| {
                        ws.project_dir
                            .as_deref()
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                    })
            })
            .or_else(|| self.cwd.as_deref().map(str::trim).filter(|s| !s.is_empty()))
    }

    /// Returns a validated non-negative finite `cost.total_usd`, if present.
    pub(crate) fn valid_cost_usd(&self) -> Option<f64> {
        self.cost
            .as_ref()?
            .total_usd
            .filter(|v| v.is_finite() && *v >= 0.0)
    }

    /// Returns the directory basename preferring `workspace.project_dir`,
    /// then `workspace.current_dir`, then top-level `cwd`.
    pub(crate) fn workspace_basename(&self) -> Option<&str> {
        let raw = self
            .workspace
            .as_ref()
            .and_then(|ws| {
                ws.project_dir
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .or_else(|| {
                        ws.current_dir
                            .as_deref()
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                    })
            })
            .or_else(|| self.cwd.as_deref().map(str::trim).filter(|s| !s.is_empty()))?;
        Path::new(raw)
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|s| !s.is_empty())
            .or(Some(raw))
    }
}

/// Builds a rich sample payload used for live preview and `doctor`.
pub(crate) fn sample_preview_payload(width: usize) -> StatuslinePayload {
    let mut quota = BTreeMap::new();
    quota.insert(
        "gemini-5h".to_string(),
        QuotaBucket {
            remaining_fraction: Some(0.84),
            reset_in_seconds: Some(11_400),
            ..Default::default()
        },
    );
    quota.insert(
        "gemini-weekly".to_string(),
        QuotaBucket {
            remaining_fraction: Some(0.92),
            reset_in_seconds: Some(432_000),
            ..Default::default()
        },
    );
    StatuslinePayload {
        agent_state: Some("thinking".to_string()),
        session_id: Some("8d0371e1-5a05-423b-a8bc-ed75995f9a8b".to_string()),
        conversation_title: Some("Add AGY payload widgets".to_string()),
        transcript_path: Some("/tmp/agy/logs/transcript.jsonl".to_string()),
        version: Some("1.5.11".to_string()),
        product: Some("Antigravity".to_string()),
        cycle_mode: Some("plan".to_string()),
        plan_tier: Some("Pro".to_string()),
        email: Some("dev@example.com".to_string()),
        agent: Some(AgentInfo {
            name: Some("researcher".to_string()),
        }),
        model: Some(ModelInfo {
            id: None,
            display_name: Some("Gemini 4".to_string()),
            effort: Some("high".to_string()),
        }),
        workspace: Some(WorkspaceInfo {
            current_dir: Some("/work/agy-statusline".to_string()),
            project_dir: Some("/work/agy-statusline".to_string()),
        }),
        vcs: Some(VcsInfo {
            vcs_type: Some("git".to_string()),
            branch: Some("main".to_string()),
            client: None,
            dirty: Some(true),
        }),
        sandbox: Some(SandboxInfo {
            enabled: Some(true),
            allow_network: None,
        }),
        vim: Some(VimInfo {
            mode: Some("NORMAL".to_string()),
        }),
        pending_input_count: Some(1),
        artifact_count: Some(2),
        subagents: Some(serde_json::json!([1, 2])),
        task_count: Some(3),
        context_window: Some(ContextWindow {
            used_percentage: Some(42.5),
            total_input_tokens: Some(48_000),
            total_output_tokens: Some(4_200),
            current_usage: Some(JsonUsage {
                input_tokens: Some(18_000),
                output_tokens: Some(50),
                cache_read_input_tokens: Some(200_000),
                cache_creation_input_tokens: Some(0),
            }),
            ..Default::default()
        }),
        cost: Some(CostInfo {
            total_usd: Some(0.14),
            subagent_usd: None,
            estimated: Some(true),
        }),
        quota: Some(quota),
        terminal_width: Some(width),
        ..Default::default()
    }
}

/// Builds sample SQLite session metadata used for live TUI preview and `doctor`.
pub(crate) fn sample_preview_metadata() -> SessionMetadata {
    SessionMetadata {
        speed: Some(64.0),
        median_speed: Some(68.0),
        last_turn_speed: Some(72.0),
        e2e_speed: Some(46.0),
        ttft_secs: Some(1.4),
        thinking_tokens: Some(4_200),
        max_context_tokens: Some(512_000),
        ..Default::default()
    }
}

/// Builds sample Git probe metadata used for live TUI preview and `doctor`.
pub(crate) fn sample_preview_git() -> GitInfo {
    GitInfo {
        branch: Some("main".to_string()),
        worktree: Some("feat-wt".to_string()),
        added: 42,
        deleted: 11,
    }
}

/// Controls how fallback session cost estimation loads model pricing rates.
#[derive(Debug, Clone, Copy, Default)]
pub enum PricingSource<'a> {
    /// Disable fallback cost pricing lookups (default for isolated tests).
    #[default]
    Disabled,
    /// Use an explicitly provided in-memory pricing catalog.
    Catalog(&'a PricingCatalog),
    /// Load models.dev rates from the XDG cache using `SyncMode`.
    Effective(SyncMode),
}

/// Explicit runtime dependencies for [`process_statusline`].
pub struct StatuslineRuntime<'a> {
    /// Session SQLite metadata reader.
    pub reader: &'a dyn SpeedReader,
    /// Git branch/worktree/diff probe.
    pub git_probe: &'a dyn GitProbe,
    /// Optional statusline cache directory (`models_dev_pricing.json`, `conv_cost_cache.json`).
    pub cache_dir: Option<&'a Path>,
    /// Optional AGY CLI directory (`~/.gemini/antigravity-cli`) for session sniffing and cost estimation.
    pub agy_cli_dir: Option<&'a Path>,
    /// Pricing source for fallback cost estimation when `cost.total_usd` is absent.
    pub pricing: PricingSource<'a>,
    /// Current Unix epoch timestamp in seconds.
    pub now_unix_secs: u64,
}

impl<'a> StatuslineRuntime<'a> {
    /// Constructs an isolated runtime with no disk caches, no global AGY directory, and pricing disabled.
    pub fn minimal(reader: &'a dyn SpeedReader, git_probe: &'a dyn GitProbe) -> Self {
        Self {
            reader,
            git_probe,
            cache_dir: None,
            agy_cli_dir: None,
            pricing: PricingSource::Disabled,
            now_unix_secs: 0,
        }
    }
}

fn layout_needs_session_metadata(cfg: &LayoutConfig) -> bool {
    cfg.rows.iter().flatten().any(|w| {
        matches!(
            w,
            WidgetId::Speed
                | WidgetId::Ttft
                | WidgetId::Thinking
                | WidgetId::CtxBar
                | WidgetId::CtxPct
                | WidgetId::CtxTokens
        )
    })
}

fn read_session_metadata(
    conv_id: Option<&str>,
    cli_dir: Option<&Path>,
    reader: &dyn SpeedReader,
) -> SessionMetadata {
    match (conv_id, cli_dir) {
        (Some(id), Some(dir)) => reader.session_metadata_in(dir, id),
        (Some(id), None) => reader.session_metadata(id),
        (None, _) => SessionMetadata::default(),
    }
}

fn probe_git(payload: &StatuslinePayload, git_probe: &dyn GitProbe, cfg: &LayoutConfig) -> GitInfo {
    let has_vcs = cfg.rows.iter().flatten().any(|w| matches!(w, WidgetId::Vcs));
    let has_wt = cfg
        .rows
        .iter()
        .flatten()
        .any(|w| matches!(w, WidgetId::Worktree));
    let payload_has_vcs_ref = payload.vcs.as_ref().is_some_and(|v| {
        v.branch.as_deref().is_some_and(|b| !b.trim().is_empty())
            || v.client.as_deref().is_some_and(|c| !c.trim().is_empty())
    });
    let explicit_non_git_vcs = payload
        .vcs
        .as_ref()
        .and_then(|v| v.vcs_type.as_deref())
        .map(str::trim)
        .is_some_and(|t| !t.is_empty() && !t.eq_ignore_ascii_case("git"));
    let needs_git = !explicit_non_git_vcs
        && (has_wt || (has_vcs && (cfg.show_git_diff || !payload_has_vcs_ref)));
    if needs_git {
        payload
            .workspace_path()
            .map(|dir| {
                let want_diff = cfg.show_git_diff && has_vcs;
                git_probe.probe(Path::new(dir), want_diff)
            })
            .unwrap_or_default()
    } else {
        GitInfo::default()
    }
}

fn estimate_payload_cost_usd(
    payload: &StatuslinePayload,
    pricing: &PricingCatalog,
) -> Option<(f64, CostCoverage)> {
    let model = payload.model.as_ref()?;
    let mut candidates = Vec::with_capacity(2);
    if let Some(id) = model.id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        candidates.push(id);
    }
    if let Some(disp) = model
        .display_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        candidates.push(disp);
    }
    if candidates.is_empty() {
        return None;
    }

    let cw = payload.context_window.as_ref()?;
    if cw.total_input_tokens.is_some() || cw.total_output_tokens.is_some() {
        let inp = cw.total_input_tokens.unwrap_or(0);
        let out = cw.total_output_tokens.unwrap_or(0);
        if inp > 0 || out > 0 {
            return pricing
                .estimate_cost_base_rate(&candidates, inp, 0, 0, out)
                .map(|(_, usd)| (usd, CostCoverage::Session));
        }
    }

    let usage = cw.current_usage.as_ref()?;
    let inp = usage.input_tokens.unwrap_or(0).max(0) as u64;
    let cache_read = usage.cache_read_input_tokens.unwrap_or(0).max(0) as u64;
    let cache_write = usage.cache_creation_input_tokens.unwrap_or(0).max(0) as u64;
    let out = usage.output_tokens.unwrap_or(0).max(0) as u64;
    if inp == 0 && cache_read == 0 && cache_write == 0 && out == 0 {
        return None;
    }
    pricing
        .estimate_cost(&candidates, inp, cache_read, cache_write, out)
        .map(|(_, usd)| (usd, CostCoverage::LatestTurn))
}

/// Parses `input` JSON and writes the statusline according to `runtime` and `cfg`.
///
/// # Examples
///
/// ```
/// use std::path::Path;
/// use agy_statusline::db::{SessionMetadata, SpeedReader};
/// use agy_statusline::git::{GitInfo, GitProbe};
/// use agy_statusline::layout::LayoutConfig;
/// use agy_statusline::statusline::{process_statusline, StatuslineRuntime};
///
/// struct FixedSpeed(Option<f64>);
/// impl SpeedReader for FixedSpeed {
///     fn session_metadata(&self, _id: &str) -> SessionMetadata {
///         SessionMetadata { speed: self.0, ..Default::default() }
///     }
/// }
/// struct NoopGit;
/// impl GitProbe for NoopGit {
///     fn probe(&self, _dir: &Path, _include_diff: bool) -> GitInfo { GitInfo::default() }
/// }
///
/// let json = r#"{"agent_state":"idle","conversation_id":"8d0371e1-5a05-423b-a8bc-ed75995f9a8b","context_window":{"current_usage":{"input_tokens":200,"cache_read_input_tokens":800,"cache_creation_input_tokens":0}}}"#;
/// let reader = FixedSpeed(Some(60.0));
/// let git = NoopGit;
/// let runtime = StatuslineRuntime::minimal(&reader, &git);
/// let mut out = Vec::new();
/// process_statusline(json, &runtime, &LayoutConfig::default(), &mut out).unwrap();
/// let text = String::from_utf8(out).unwrap();
/// assert!(text.contains("READY"));
/// assert!(text.contains("80.0%"));
/// assert!(text.contains("60 tok/s"));
/// ```
pub fn process_statusline<W: Write>(
    input: &str,
    runtime: &StatuslineRuntime<'_>,
    cfg: &LayoutConfig,
    out: &mut W,
) -> io::Result<()> {
    let payload: StatuslinePayload = serde_json::from_str(input).unwrap_or_default();
    let conv_id = payload
        .conversation_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let cli_dir = runtime.agy_cli_dir;
    let git_info = probe_git(&payload, runtime.git_probe, cfg);

    let needs_meta = layout_needs_session_metadata(cfg);
    let needs_cost = cfg.rows.iter().flatten().any(|w| matches!(w, WidgetId::Cost))
        && payload.valid_cost_usd().is_none()
        && !matches!(runtime.pricing, PricingSource::Disabled);

    let mut coverage = CostCoverage::Session;

    // Fast path: check bounded LRU conversation cost cache via lstat before parsing pricing JSON
    // or opening any SQLite DB for cost scanning.
    let cached_cost = if needs_cost {
        match (runtime.pricing, runtime.cache_dir, cli_dir, conv_id) {
            (PricingSource::Effective(mode), Some(cache_dir), Some(cli_dir), Some(cid)) => {
                let pricing_fp = pricing_cache_fingerprint(Some(cache_dir));
                let allow_hit = mode != SyncMode::BlockingIfStale
                    || is_pricing_fingerprint_fresh(pricing_fp, runtime.now_unix_secs);
                let hit = allow_hit
                    .then(|| lookup_cached_conversation_cost(cache_dir, cli_dir, cid, pricing_fp))
                    .flatten();
                if hit.is_some() && mode == SyncMode::SpawnIfStale {
                    maybe_spawn_pricing_sync_if_stale(Some(cache_dir), runtime.now_unix_secs);
                }
                hit
            }
            _ => None,
        }
    } else {
        None
    };

    // Reuse a single read-only parent SQLite connection when both metadata and uncached SQLite
    // cost read the same local conversation DB via SqliteSpeedReader.
    let shared_parent_conn = if needs_meta
        && needs_cost
        && cached_cost.is_none()
        && runtime.reader.uses_local_sqlite()
    {
        match (cli_dir, conv_id) {
            (Some(cli_dir), Some(cid)) => resolve_conversation_db_path(cli_dir, cid)
                .and_then(|db_path| open_readonly_db(&db_path).ok()),
            _ => None,
        }
    } else {
        None
    };

    let mut meta = if let Some(conn) = shared_parent_conn.as_ref() {
        query_session_metadata_from_conn(conn)
    } else if needs_meta {
        read_session_metadata(conv_id, cli_dir, runtime.reader)
    } else {
        SessionMetadata::default()
    };

    if needs_cost {
        if let Some(cost) = cached_cost {
            if cost.unpriced_turns > 0 {
                coverage = CostCoverage::Partial(cost.unpriced_turns);
            }
            meta.estimated_cost_usd = cost.usd;
            meta.estimated_subagent_cost_usd = cost.subagent_usd;
        } else {
            let mut estimate_with =
                |pricing: &PricingCatalog,
                 cache_target: Option<(&Path, PricingFileFingerprint)>|
                 -> (Option<f64>, Option<f64>) {
                    if let Some(cost) = conv_id.and_then(|cid| {
                        cli_dir.and_then(|dir| {
                            estimate_conversation_cost_usd_with_parent_conn(
                                dir,
                                cid,
                                pricing,
                                shared_parent_conn.as_ref(),
                                cache_target,
                            )
                        })
                    }) {
                        if cost.unpriced_turns > 0 {
                            coverage = CostCoverage::Partial(cost.unpriced_turns);
                        }
                        return (cost.usd, cost.subagent_usd);
                    }
                    match estimate_payload_cost_usd(&payload, pricing) {
                        Some((usd, source)) => {
                            coverage = source;
                            (Some(usd), None)
                        }
                        None => (None, None),
                    }
                };
            (meta.estimated_cost_usd, meta.estimated_subagent_cost_usd) = match runtime.pricing {
                PricingSource::Disabled => (None, None),
                PricingSource::Catalog(pricing) => estimate_with(pricing, None),
                PricingSource::Effective(mode) => {
                    let pricing = PricingCatalog::load_effective_in(
                        runtime.cache_dir,
                        mode,
                        runtime.now_unix_secs,
                    );
                    let cache_target = runtime
                        .cache_dir
                        .map(|dir| (dir, pricing_cache_fingerprint(Some(dir))));
                    estimate_with(&pricing, cache_target)
                }
            };
        }
    }

    write_statusline_at(
        &payload,
        &meta,
        &git_info,
        coverage,
        runtime.now_unix_secs,
        cfg,
        out,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubReader(Option<f64>);
    impl SpeedReader for StubReader {
        fn session_metadata(&self, _id: &str) -> SessionMetadata {
            SessionMetadata {
                speed: self.0,
                median_speed: self.0,
                last_turn_speed: self.0,
                ..Default::default()
            }
        }
    }

    struct NoopGit;
    impl GitProbe for NoopGit {
        fn probe(&self, _dir: &Path, _include_diff: bool) -> GitInfo {
            GitInfo::default()
        }
    }

    #[test]
    fn empty_and_invalid_json_degrades_gracefully() {
        let mut cfg = LayoutConfig::preset_addon();
        cfg.theme = crate::layout::ThemeStyle::Mono;
        let git = NoopGit;

        let reader_none = StubReader(None);
        let mut out = Vec::new();
        process_statusline("", &StatuslineRuntime::minimal(&reader_none, &git), &cfg, &mut out)
            .unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "◈ cache -- ⚡ --\n");

        let reader_some = StubReader(Some(100.0));
        let mut out2 = Vec::new();
        process_statusline(
            "{bad json",
            &StatuslineRuntime::minimal(&reader_some, &git),
            &cfg,
            &mut out2,
        )
        .unwrap();
        assert_eq!(String::from_utf8(out2).unwrap(), "◈ cache -- ⚡ --\n");
    }

    #[test]
    fn parses_negative_reset_credit_bucket_disabled_and_subagent_status() {
        let json = r#"{
            "subagents": [
                {"name": "worker-1", "status": "running"},
                {"name": "worker-2", "status": "completed"},
                {"name": "legacy-worker"}
            ],
            "quota": {
                "pool-5h": {
                    "remaining_amount": 420,
                    "reset_in_seconds": -15,
                    "disabled": false
                },
                "pool-7d": {
                    "remaining_fraction": 0.10,
                    "disabled": true
                }
            }
        }"#;
        let payload: StatuslinePayload = serde_json::from_str(json).unwrap();
        assert_eq!(payload.subagent_count(), 2);

        let quota = payload.quota.unwrap();
        assert_eq!(quota["pool-5h"].remaining_amount, Some(420));
        assert_eq!(quota["pool-5h"].reset_in_seconds, Some(-15));
        assert_eq!(quota["pool-5h"].disabled, Some(false));
        assert_eq!(quota["pool-7d"].disabled, Some(true));

        let legacy: StatuslinePayload =
            serde_json::from_str(r#"{"subagents":[1,2,3]}"#).unwrap();
        assert_eq!(legacy.subagent_count(), 3);
    }

    #[test]
    fn skips_git_probe_for_explicit_non_git_vcs() {
        use std::cell::Cell;

        struct CountingGit<'a>(&'a Cell<usize>);
        impl GitProbe for CountingGit<'_> {
            fn probe(&self, _dir: &Path, _include_diff: bool) -> GitInfo {
                self.0.set(self.0.get() + 1);
                GitInfo {
                    branch: Some("unexpected-git".to_string()),
                    ..Default::default()
                }
            }
        }

        let calls = Cell::new(0usize);
        let mut cfg = LayoutConfig::preset_minimal();
        cfg.theme = crate::layout::ThemeStyle::Mono;
        cfg.show_git_diff = true;
        cfg.rows = vec![vec![WidgetId::Vcs, WidgetId::Worktree]];
        let reader = StubReader(None);
        let git = CountingGit(&calls);
        let runtime = StatuslineRuntime::minimal(&reader, &git);

        let hg_json = r#"{
            "cwd": "/tmp/hg-repo",
            "vcs": {"type": "hg", "branch": "default"}
        }"#;
        let mut out = Vec::new();
        process_statusline(hg_json, &runtime, &cfg, &mut out).unwrap();
        assert_eq!(calls.get(), 0);

        let citc_json = r#"{
            "cwd": "/tmp/citc-ws",
            "vcs": {"type": "citc", "client": "my-client"}
        }"#;
        out.clear();
        process_statusline(citc_json, &runtime, &cfg, &mut out).unwrap();
        assert_eq!(calls.get(), 0);

        // Explicit "git" still probes.
        let git_json = r#"{
            "cwd": "/tmp/git-repo",
            "vcs": {"type": "git"}
        }"#;
        out.clear();
        process_statusline(git_json, &runtime, &cfg, &mut out).unwrap();
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn skips_session_metadata_read_when_no_sqlite_widget_active() {
        use std::cell::Cell;

        struct CountingReader<'a>(&'a Cell<usize>);
        impl SpeedReader for CountingReader<'_> {
            fn session_metadata(&self, _id: &str) -> SessionMetadata {
                self.0.set(self.0.get() + 1);
                SessionMetadata {
                    speed: Some(75.0),
                    ..Default::default()
                }
            }
        }

        let calls = Cell::new(0usize);
        let reader = CountingReader(&calls);
        let git = NoopGit;
        let runtime = StatuslineRuntime::minimal(&reader, &git);
        let json = r#"{
            "agent_state": "idle",
            "conversation_id": "8d0371e1-5a05-423b-a8bc-ed75995f9a8b"
        }"#;

        let mut cfg_no_db = LayoutConfig::preset_minimal();
        cfg_no_db.rows = vec![vec![WidgetId::State, WidgetId::Model]];
        let mut out = Vec::new();
        process_statusline(json, &runtime, &cfg_no_db, &mut out).unwrap();
        assert_eq!(calls.get(), 0);

        let mut cfg_with_speed = LayoutConfig::preset_minimal();
        cfg_with_speed.rows = vec![vec![WidgetId::State, WidgetId::Speed]];
        out.clear();
        process_statusline(json, &runtime, &cfg_with_speed, &mut out).unwrap();
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn workspace_basename_trims_whitespace_only_candidates() {
        let p1: StatuslinePayload = serde_json::from_str(
            r#"{
                "workspace": {"project_dir": "   ", "current_dir": " /tmp/active-dir "},
                "cwd": "/tmp/fallback-cwd"
            }"#,
        )
        .unwrap();
        assert_eq!(p1.workspace_basename(), Some("active-dir"));

        let p2: StatuslinePayload = serde_json::from_str(
            r#"{
                "workspace": {"project_dir": "\t ", "current_dir": "   "},
                "cwd": " /tmp/fallback-cwd "
            }"#,
        )
        .unwrap();
        assert_eq!(p2.workspace_basename(), Some("fallback-cwd"));

        let p3: StatuslinePayload = serde_json::from_str(
            r#"{"workspace": {"project_dir": "  "}, "cwd": "  "}"#,
        )
        .unwrap();
        assert_eq!(p3.workspace_basename(), None);
    }

    #[test]
    fn workspace_path_trims_whitespace_only_candidates() {
        let p1: StatuslinePayload = serde_json::from_str(
            r#"{
                "workspace": {"current_dir": "   ", "project_dir": " /tmp/proj-dir "},
                "cwd": "/tmp/fallback-cwd"
            }"#,
        )
        .unwrap();
        assert_eq!(p1.workspace_path(), Some("/tmp/proj-dir"));

        let p2: StatuslinePayload = serde_json::from_str(
            r#"{
                "workspace": {"current_dir": "\t ", "project_dir": "   "},
                "cwd": " /tmp/fallback-cwd "
            }"#,
        )
        .unwrap();
        assert_eq!(p2.workspace_path(), Some("/tmp/fallback-cwd"));

        let p3: StatuslinePayload = serde_json::from_str(
            r#"{"workspace": {"current_dir": "  "}, "cwd": "  "}"#,
        )
        .unwrap();
        assert_eq!(p3.workspace_path(), None);
    }

    #[test]
    fn negative_payload_cost_triggers_fallback_estimation() {
        let dir = tempfile::tempdir().unwrap();
        let conv_dir = dir.path().join("conversations");
        std::fs::create_dir_all(&conv_dir).unwrap();
        let uuid = "8d0371e1-5a05-423b-a8bc-ed75995f9a8b";
        let conn = rusqlite::Connection::open(conv_dir.join(format!("{uuid}.db"))).unwrap();
        conn.execute(
            "CREATE TABLE gen_metadata (idx INTEGER PRIMARY KEY, data BLOB)",
            [],
        )
        .unwrap();
        let blob = crate::protobuf::encode_gen_blob(crate::protobuf::GenBlobSpec {
            output_tok: 100,
            model_cost: Some(0.25),
            ..Default::default()
        });
        conn.execute("INSERT INTO gen_metadata (idx, data) VALUES (1, ?1)", [blob])
            .unwrap();
        drop(conn);

        let reader = StubReader(None);
        let git = NoopGit;
        let pricing = PricingCatalog::default();
        let runtime = StatuslineRuntime {
            reader: &reader,
            git_probe: &git,
            cache_dir: None,
            agy_cli_dir: Some(dir.path()),
            pricing: PricingSource::Catalog(&pricing),
            now_unix_secs: 0,
        };
        let mut cfg = LayoutConfig::preset_minimal();
        cfg.theme = crate::layout::ThemeStyle::Mono;
        cfg.rows = vec![vec![WidgetId::Cost]];

        let json = format!(
            r#"{{"conversation_id":"{uuid}","cost":{{"total_usd":-1.0,"estimated":false}}}}"#
        );
        let mut out = Vec::new();
        process_statusline(&json, &runtime, &cfg, &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "~$0.25\n");
    }

    fn seed_gen_turn_db(
        cli_dir: &Path,
        uuid: &str,
        idx: i64,
        spec: crate::protobuf::GenBlobSpec<'_>,
    ) -> std::path::PathBuf {
        let db_path = cli_dir.join("conversations").join(format!("{uuid}.db"));
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute(
            "CREATE TABLE IF NOT EXISTS gen_metadata (idx INTEGER PRIMARY KEY, data BLOB)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO gen_metadata (idx, data) VALUES (?1, ?2)",
            rusqlite::params![idx, crate::protobuf::encode_gen_blob(spec)],
        )
        .unwrap();
        db_path
    }

    fn bump_mtime(path: &Path, secs: u64) {
        let mtime = std::fs::metadata(path).unwrap().modified().unwrap();
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(mtime + std::time::Duration::from_secs(secs))
            .unwrap();
    }

    #[test]
    fn conversation_cost_cache_hits_skip_pricing_parse_and_invalidate_on_all_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let cli_dir = dir.path().join("cli");
        let cache_dir = dir.path().join("cache");
        std::fs::create_dir_all(&cache_dir).unwrap();

        let parent = "11111111-1111-4111-8111-111111111111";
        let child = "22222222-2222-4222-8222-222222222222";
        let child2 = "33333333-3333-4333-8333-333333333333";

        let pricing_path = cache_dir.join("models_dev_pricing.json");
        std::fs::write(
            &pricing_path,
            r#"{"fetched_at":1700000000,"models":{"gemini-4-carbon":{"input_per_mtok":2.0,"output_per_mtok":0.0}}}"#,
        )
        .unwrap();

        let parent_db = seed_gen_turn_db(
            &cli_dir,
            parent,
            1,
            crate::protobuf::GenBlobSpec {
                resp_model: Some("gemini-4-carbon"),
                input_tok: 1_000_000,
                output_tok: 100,
                dur_secs: 2,
                ..Default::default()
            },
        );
        let child_db = seed_gen_turn_db(
            &cli_dir,
            child,
            1,
            crate::protobuf::GenBlobSpec {
                resp_model: Some("gemini-4-carbon"),
                input_tok: 1_000_000,
                ..Default::default()
            },
        );
        let summaries_path = cli_dir.join("conversation_summaries.db");
        let summaries = rusqlite::Connection::open(&summaries_path).unwrap();
        summaries
            .execute(
                "CREATE TABLE conversation_summaries (
                    conversation_id TEXT,
                    parent_conversation_id TEXT,
                    nesting_depth INTEGER
                )",
                [],
            )
            .unwrap();
        summaries
            .execute(
                "INSERT INTO conversation_summaries VALUES (?1, ?2, 1)",
                [child, parent],
            )
            .unwrap();
        drop(summaries);

        let reader = crate::db::SqliteSpeedReader::default();
        let git = NoopGit;
        let runtime = StatuslineRuntime {
            reader: &reader,
            git_probe: &git,
            cache_dir: Some(&cache_dir),
            agy_cli_dir: Some(&cli_dir),
            pricing: PricingSource::Effective(SyncMode::Offline),
            now_unix_secs: 1_700_000_000,
        };
        let mut cfg = LayoutConfig::preset_minimal();
        cfg.theme = crate::layout::ThemeStyle::Mono;
        cfg.rows = vec![vec![WidgetId::Speed, WidgetId::Cost]];
        let json = format!(r#"{{"conversation_id":"{parent}"}}"#);

        // Run 1: cache miss populates conv_cost_cache.json ($2 parent + $2 child = $4, sub $2).
        let mut out = Vec::new();
        process_statusline(&json, &runtime, &cfg, &mut out).unwrap();
        let text1 = String::from_utf8(out).unwrap();
        assert!(text1.contains("50 tok/s"), "run 1: {text1:?}");
        assert!(text1.contains("~$4.00 (sub $2.00)"), "run 1: {text1:?}");
        assert!(cache_dir.join("conv_cost_cache.json").exists());

        // Cache hit skips parsing models_dev_pricing.json: corrupt bytes with identical length and
        // preserved mtime still serve the cached cost without parsing JSON.
        let pricing_meta = std::fs::metadata(&pricing_path).unwrap();
        let pricing_mtime = pricing_meta.modified().unwrap();
        std::fs::write(&pricing_path, vec![b'!'; pricing_meta.len() as usize]).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&pricing_path)
            .unwrap()
            .set_modified(pricing_mtime)
            .unwrap();
        let mut out_hit = Vec::new();
        process_statusline(&json, &runtime, &cfg, &mut out_hit).unwrap();
        let text_hit = String::from_utf8(out_hit).unwrap();
        assert!(text_hit.contains("50 tok/s"), "cache hit: {text_hit:?}");
        assert!(
            text_hit.contains("~$4.00 (sub $2.00)"),
            "cache hit: {text_hit:?}"
        );

        // (d) Pricing file change invalidates cache ($3.50 parent + $3.50 child = $7.00).
        std::fs::write(
            &pricing_path,
            r#"{"fetched_at":1700000001,"models":{"gemini-4-carbon":{"input_per_mtok":3.5,"output_per_mtok":0.0}}}"#,
        )
        .unwrap();
        bump_mtime(&pricing_path, 2);
        let mut out_pricing = Vec::new();
        process_statusline(&json, &runtime, &cfg, &mut out_pricing).unwrap();
        let text_pricing = String::from_utf8(out_pricing).unwrap();
        assert!(
            text_pricing.contains("~$7.00 (sub $3.50)"),
            "pricing invalidation: {text_pricing:?}"
        );

        // (a) Parent DB change invalidates cache ($7.00 parent + $3.50 child = $10.50).
        seed_gen_turn_db(
            &cli_dir,
            parent,
            2,
            crate::protobuf::GenBlobSpec {
                resp_model: Some("gemini-4-carbon"),
                input_tok: 1_000_000,
                output_tok: 100,
                dur_secs: 2,
                ..Default::default()
            },
        );
        bump_mtime(&parent_db, 2);
        let mut out_parent = Vec::new();
        process_statusline(&json, &runtime, &cfg, &mut out_parent).unwrap();
        let text_parent = String::from_utf8(out_parent).unwrap();
        assert!(
            text_parent.contains("~$10.50 (sub $3.50)"),
            "parent invalidation: {text_parent:?}"
        );

        // (c) Descendant subagent DB change invalidates cache while parent & summaries are untouched.
        seed_gen_turn_db(
            &cli_dir,
            child,
            2,
            crate::protobuf::GenBlobSpec {
                resp_model: Some("gemini-4-carbon"),
                input_tok: 1_000_000,
                ..Default::default()
            },
        );
        bump_mtime(&child_db, 2);
        let mut out_child = Vec::new();
        process_statusline(&json, &runtime, &cfg, &mut out_child).unwrap();
        let text_child = String::from_utf8(out_child).unwrap();
        assert!(
            text_child.contains("~$14.00 (sub $7.00)"),
            "descendant invalidation: {text_child:?}"
        );

        // (b) conversation_summaries.db change invalidates cache while parent & first child are untouched.
        seed_gen_turn_db(
            &cli_dir,
            child2,
            1,
            crate::protobuf::GenBlobSpec {
                resp_model: Some("gemini-4-carbon"),
                input_tok: 1_000_000,
                ..Default::default()
            },
        );
        let summaries2 = rusqlite::Connection::open(&summaries_path).unwrap();
        summaries2
            .execute(
                "INSERT INTO conversation_summaries VALUES (?1, ?2, 1)",
                [child2, parent],
            )
            .unwrap();
        drop(summaries2);
        bump_mtime(&summaries_path, 2);
        let mut out_summaries = Vec::new();
        process_statusline(&json, &runtime, &cfg, &mut out_summaries).unwrap();
        let text_summaries = String::from_utf8(out_summaries).unwrap();
        assert!(
            text_summaries.contains("~$17.50 (sub $10.50)"),
            "summaries invalidation: {text_summaries:?}"
        );
    }
}
