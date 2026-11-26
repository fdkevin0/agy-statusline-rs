//! Null-safe AGY statusline JSON schema and pipeline orchestration.

pub mod format;
pub mod git;
pub(crate) mod metrics;
pub mod render;
pub(crate) mod theme;

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::agy::db::{
    open_readonly_db, query_session_metadata_from_conn, query_session_metadata_in,
    resolve_conversation_db_path, SessionMetadata, SpeedReader,
};
use crate::agy::quota::{
    load_quota_snapshot, save_quota_snapshot, BucketHistory, QuotaSnapshot, QuotaSource,
};
use crate::config::layout::{LayoutConfig, WidgetId};
use crate::stats::{
    estimate_conversation_cost_usd_with_parent_conn, is_pricing_fingerprint_fresh,
    lookup_cached_conversation_cost, maybe_spawn_pricing_sync_if_stale, pricing_cache_fingerprint,
    PricingCatalog, SyncMode,
};
use crate::statusline::git::{GitInfo, GitProbe};
use crate::statusline::metrics::resolve_credit_views;
use crate::statusline::render::{write_statusline_with, RenderExtras};
use crate::sys::fmt::{ascii_values_conflict, trimmed_non_empty};
use crate::sys::time::parse_rfc3339_unix_secs;

/// Maximum age (6 hours) for falling back to cached `quota_snapshot.json` on empty statusline ticks.
pub(crate) const MAX_QUOTA_FALLBACK_AGE_SECS: u64 = 6 * 3_600;

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

/// Subagent entry reported in AGY `subagents`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SubagentInfo {
    /// Subagent name.
    pub name: Option<String>,
    /// Subagent role label.
    pub role: Option<String>,
    /// Subagent lifecycle status (`running`, `completed`, etc.).
    pub status: Option<String>,
}

/// Rolling rate-limit bucket from AGY `quota`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct QuotaBucket {
    /// Fraction of quota remaining (`0.0..=1.0`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining_fraction: Option<f64>,
    /// Remaining credit amount when the bucket reports discrete units instead of a fraction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining_amount: Option<i64>,
    /// Countdown in seconds until the bucket resets (may be negative right after reset).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reset_in_seconds: Option<i64>,
    /// ISO-8601 reset timestamp.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reset_time: Option<String>,
    /// Whether this bucket is disabled by the server.
    #[serde(skip_serializing_if = "Option::is_none")]
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
    /// AGY CLI version string (`"1.5.16"`).
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
    /// Active and completed subagents.
    pub subagents: Option<Vec<SubagentInfo>>,
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

/// Returns the number of active subagents (`status != "completed"`).
pub(crate) fn subagent_count(payload: &StatuslinePayload) -> usize {
    active_subagents(payload).count()
}

/// Returns the role or name of the sole active subagent when `subagent_count(payload) == 1`.
pub(crate) fn single_active_subagent_label(payload: &StatuslinePayload) -> Option<&str> {
    let mut iter = active_subagents(payload);
    let first = iter.next()?;
    if iter.next().is_some() {
        return None;
    }
    trimmed_non_empty(first.role.as_deref()).or_else(|| trimmed_non_empty(first.name.as_deref()))
}

fn active_subagents(payload: &StatuslinePayload) -> impl Iterator<Item = &SubagentInfo> {
    payload.subagents.iter().flatten().filter(|item| {
        item.status
            .as_deref()
            .is_none_or(|status| !status.eq_ignore_ascii_case("completed"))
    })
}

fn first_workspace_candidate(payload: &StatuslinePayload, prefer_project: bool) -> Option<&str> {
    let ws = payload.workspace.as_ref();
    let cur = ws.and_then(|w| trimmed_non_empty(w.current_dir.as_deref()));
    let proj = ws.and_then(|w| trimmed_non_empty(w.project_dir.as_deref()));
    let (first, second) = if prefer_project {
        (proj, cur)
    } else {
        (cur, proj)
    };
    first
        .or(second)
        .or_else(|| trimmed_non_empty(payload.cwd.as_deref()))
}

/// Returns the trimmed workspace directory path preferring `workspace.current_dir`,
/// then `workspace.project_dir`, then top-level `cwd`.
fn workspace_path(payload: &StatuslinePayload) -> Option<&str> {
    first_workspace_candidate(payload, false)
}

/// Returns a validated non-negative finite `cost.total_usd`, if present.
pub(crate) fn valid_cost_usd(payload: &StatuslinePayload) -> Option<f64> {
    payload
        .cost
        .as_ref()?
        .total_usd
        .filter(|v| v.is_finite() && *v >= 0.0)
}

/// Returns the directory basename preferring `workspace.project_dir`,
/// then `workspace.current_dir`, then top-level `cwd`.
pub(crate) fn workspace_basename(payload: &StatuslinePayload) -> Option<&str> {
    let raw = first_workspace_candidate(payload, true)?;
    Path::new(raw)
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|s| !s.is_empty())
        .or(Some(raw))
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
        version: Some("1.5.16".to_string()),
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
        subagents: Some(vec![SubagentInfo::default(), SubagentInfo::default()]),
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

/// Source of session SQLite metadata and conversation cost estimation.
#[derive(Clone, Copy, Default)]
pub enum SessionSource<'a> {
    /// Isolated mode with no local session database.
    #[default]
    None,
    /// Local AGY CLI directory (`~/.gemini/antigravity-cli`) owning `conversations/*.db`.
    LocalDir(&'a Path),
    /// Custom [`SpeedReader`] override for isolated unit tests.
    Reader(&'a dyn SpeedReader),
}

impl<'a> SessionSource<'a> {
    fn local_cli_dir(self) -> Option<&'a Path> {
        match self {
            Self::LocalDir(dir) => Some(dir),
            Self::None | Self::Reader(_) => None,
        }
    }
}

/// Explicit runtime dependencies for [`process_statusline`].
pub struct StatuslineRuntime<'a> {
    /// Session SQLite directory or custom metadata reader.
    pub session: SessionSource<'a>,
    /// Git branch/worktree/diff probe.
    pub git_probe: &'a dyn GitProbe,
    /// Optional statusline cache directory (`models_dev_pricing.json`, `conv_cost_cache.json`).
    pub cache_dir: Option<&'a Path>,
    /// Pricing source for fallback cost estimation when `cost.total_usd` is absent.
    pub pricing: PricingSource<'a>,
    /// Current Unix epoch timestamp in seconds.
    pub now_unix_secs: u64,
}

impl<'a> StatuslineRuntime<'a> {
    /// Constructs an isolated runtime with a custom reader, no disk caches, and pricing disabled.
    pub fn minimal(reader: &'a dyn SpeedReader, git_probe: &'a dyn GitProbe) -> Self {
        Self {
            session: SessionSource::Reader(reader),
            git_probe,
            cache_dir: None,
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

fn layout_needs_quota_or_plan(cfg: &LayoutConfig) -> bool {
    cfg.rows.iter().flatten().any(|w| {
        matches!(
            w,
            WidgetId::Quota5h | WidgetId::QuotaWeekly | WidgetId::Plan
        )
    })
}

fn filter_fresh_fallback_quota(
    snap: &QuotaSnapshot,
    now_unix_secs: u64,
) -> BTreeMap<String, QuotaBucket> {
    let elapsed = now_unix_secs.saturating_sub(snap.updated_at);
    let mut out = BTreeMap::new();
    for (key, bucket) in &snap.quota {
        if bucket.disabled == Some(true) {
            continue;
        }
        if let Some(reset_unix) = bucket
            .reset_time
            .as_deref()
            .and_then(parse_rfc3339_unix_secs)
        {
            if reset_unix <= now_unix_secs as i64 {
                continue;
            }
            let mut adjusted = bucket.clone();
            // Drop stale snapshot-relative countdown so renderer derives live remaining time from `reset_time`.
            adjusted.reset_in_seconds = None;
            out.insert(key.clone(), adjusted);
        } else if let Some(secs) = bucket.reset_in_seconds {
            let rem = secs.saturating_sub(elapsed.min(i64::MAX as u64) as i64);
            if rem <= 0 {
                continue;
            }
            let mut adjusted = bucket.clone();
            adjusted.reset_in_seconds = Some(rem);
            out.insert(key.clone(), adjusted);
        } else {
            out.insert(key.clone(), bucket.clone());
        }
    }
    out
}

fn extract_payload_ai_credits(
    quota: &BTreeMap<String, QuotaBucket>,
    now_unix_secs: u64,
) -> Option<i64> {
    resolve_credit_views(quota, now_unix_secs)
        .into_iter()
        .find(|c| {
            c.key.eq_ignore_ascii_case("google_one_ai") || c.key.eq_ignore_ascii_case("ai_credits")
        })
        .map(|c| c.remaining_amount)
}

fn should_enrich_plan_with_ultra(payload_tier: Option<&str>, snap: &QuotaSnapshot) -> bool {
    if snap.ultra_tier_label.is_none() {
        return false;
    }
    match payload_tier {
        None => true,
        Some(t) => t.eq_ignore_ascii_case("pro") || t.eq_ignore_ascii_case("ultra"),
    }
}

#[derive(Default)]
struct StatuslineQuotaState {
    fallback_quota: Option<BTreeMap<String, QuotaBucket>>,
    history: Option<BTreeMap<String, BucketHistory>>,
    plan_tier_override: Option<String>,
    ai_credits_fallback: Option<i64>,
}

fn new_empty_stdin_snapshot(
    now_unix_secs: u64,
    plan_tier: Option<&str>,
    email: Option<&str>,
) -> QuotaSnapshot {
    QuotaSnapshot {
        updated_at: now_unix_secs,
        source: QuotaSource::Stdin,
        plan_tier: plan_tier.map(str::to_string),
        email: email.map(str::to_string),
        ..Default::default()
    }
}

fn record_stdin_quota_observation(
    dir: &Path,
    prev: Option<QuotaSnapshot>,
    quota_map: &BTreeMap<String, QuotaBucket>,
    payload_tier: Option<&str>,
    payload_email: Option<&str>,
    now_unix_secs: u64,
) -> StatuslineQuotaState {
    // Stdin upstream often reports "Pro" for Google AI Ultra users and never carries
    // `ultra_tier_label`; preserve the cached Hub RPC Ultra multiplier for the same account.
    let (eff_tier, eff_ultra) = match prev.as_ref() {
        Some(p)
            if !ascii_values_conflict(p.email.as_deref(), payload_email)
                && should_enrich_plan_with_ultra(payload_tier, p) =>
        {
            (
                p.plan_tier.clone().or_else(|| Some("Ultra".to_string())),
                p.ultra_tier_label.clone(),
            )
        }
        _ => (payload_tier.map(str::to_string), None),
    };
    let incoming = QuotaSnapshot {
        updated_at: now_unix_secs,
        source: QuotaSource::Stdin,
        plan_tier: eff_tier,
        ultra_tier_label: eff_ultra,
        email: payload_email.map(str::to_string),
        ai_credits: extract_payload_ai_credits(quota_map, now_unix_secs),
        quota: quota_map.clone(),
        history: BTreeMap::new(),
    };
    let merged = match prev {
        Some(mut existing) => {
            existing.merge_observation(incoming, now_unix_secs);
            existing
        }
        None => {
            let mut fresh = incoming;
            fresh.record_bucket_samples(now_unix_secs);
            fresh
        }
    };
    let _ = save_quota_snapshot(dir, &merged);
    let plan_tier_override = (merged.ultra_tier_label.is_some() || payload_tier.is_none())
        .then(|| merged.display_plan_tier())
        .flatten();
    StatuslineQuotaState {
        fallback_quota: None,
        history: (!merged.history.is_empty()).then_some(merged.history),
        plan_tier_override,
        ai_credits_fallback: merged.ai_credits,
    }
}

fn resolve_cached_quota_fallback(
    dir: &Path,
    prev: Option<QuotaSnapshot>,
    payload_tier: Option<&str>,
    payload_email: Option<&str>,
    has_observation: bool,
    needs_fallback: bool,
    now_unix_secs: u64,
) -> StatuslineQuotaState {
    // Update account/tier metadata without refreshing stale bucket timestamps, then load a
    // fresh-enough (<= 6h) cached snapshot as fallback.
    let active_snap = match prev {
        Some(existing) if ascii_values_conflict(existing.email.as_deref(), payload_email) => {
            let fresh = new_empty_stdin_snapshot(now_unix_secs, payload_tier, payload_email);
            let _ = save_quota_snapshot(dir, &fresh);
            Some(fresh)
        }
        Some(mut existing) => {
            let mut dirty = false;
            if let Some(em) = payload_email.filter(|&em| existing.email.as_deref() != Some(em)) {
                existing.email = Some(em.to_string());
                dirty = true;
            }
            if let Some(tier) = payload_tier {
                let preserve_ultra = should_enrich_plan_with_ultra(Some(tier), &existing);
                if !preserve_ultra && existing.plan_tier.as_deref() != Some(tier) {
                    existing.plan_tier = Some(tier.to_string());
                    existing.ultra_tier_label = None;
                    dirty = true;
                }
            }
            if dirty {
                let _ = save_quota_snapshot(dir, &existing);
            }
            Some(existing)
        }
        None if has_observation => {
            let fresh = new_empty_stdin_snapshot(now_unix_secs, payload_tier, payload_email);
            let _ = save_quota_snapshot(dir, &fresh);
            Some(fresh)
        }
        None => None,
    };

    let Some(snap) = active_snap else {
        return StatuslineQuotaState::default();
    };
    if now_unix_secs.saturating_sub(snap.updated_at) > MAX_QUOTA_FALLBACK_AGE_SECS {
        return StatuslineQuotaState::default();
    }

    let plan_tier_override = (payload_tier.is_none()
        || should_enrich_plan_with_ultra(payload_tier, &snap))
    .then(|| snap.display_plan_tier())
    .flatten();
    if !needs_fallback {
        return StatuslineQuotaState {
            plan_tier_override,
            ..Default::default()
        };
    }

    let filtered = filter_fresh_fallback_quota(&snap, now_unix_secs);
    let fallback_quota = (!filtered.is_empty()).then_some(filtered);
    let history = (fallback_quota.is_some() && !snap.history.is_empty()).then_some(snap.history);
    StatuslineQuotaState {
        fallback_quota,
        history,
        plan_tier_override,
        ai_credits_fallback: snap.ai_credits,
    }
}

fn resolve_statusline_quota_state(
    payload: &StatuslinePayload,
    cache_dir: Option<&Path>,
    now_unix_secs: u64,
    cfg: &LayoutConfig,
) -> StatuslineQuotaState {
    let Some(dir) = cache_dir else {
        return StatuslineQuotaState::default();
    };

    let payload_quota = payload.quota.as_ref().filter(|q| !q.is_empty());
    let payload_tier = trimmed_non_empty(payload.plan_tier.as_deref());
    let payload_email = trimmed_non_empty(payload.email.as_deref());
    let has_observation =
        payload_quota.is_some() || payload_tier.is_some() || payload_email.is_some();
    let needs_fallback = payload_quota.is_none() && layout_needs_quota_or_plan(cfg);

    if !has_observation && !needs_fallback {
        return StatuslineQuotaState::default();
    }

    let prev = load_quota_snapshot(dir).ok().flatten();
    match payload_quota {
        Some(quota_map) => record_stdin_quota_observation(
            dir,
            prev,
            quota_map,
            payload_tier,
            payload_email,
            now_unix_secs,
        ),
        None => resolve_cached_quota_fallback(
            dir,
            prev,
            payload_tier,
            payload_email,
            has_observation,
            needs_fallback,
            now_unix_secs,
        ),
    }
}

fn probe_git(payload: &StatuslinePayload, git_probe: &dyn GitProbe, cfg: &LayoutConfig) -> GitInfo {
    let has_vcs = cfg
        .rows
        .iter()
        .flatten()
        .any(|w| matches!(w, WidgetId::Vcs));
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
        workspace_path(payload)
            .map(|dir| {
                let want_diff = cfg.show_git_diff && has_vcs;
                git_probe.probe(Path::new(dir), want_diff)
            })
            .unwrap_or_default()
    } else {
        GitInfo::default()
    }
}

fn resolve_session_metadata_and_cost(
    payload: &StatuslinePayload,
    runtime: &StatuslineRuntime<'_>,
    cfg: &LayoutConfig,
) -> (SessionMetadata, u64) {
    let conv_id = trimmed_non_empty(payload.conversation_id.as_deref());
    let cli_dir = runtime.session.local_cli_dir();
    let needs_meta = layout_needs_session_metadata(cfg);
    let needs_cost = cfg
        .rows
        .iter()
        .flatten()
        .any(|w| matches!(w, WidgetId::Cost))
        && valid_cost_usd(payload).is_none()
        && !matches!(runtime.pricing, PricingSource::Disabled);

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
    // cost read the same local conversation DB.
    let shared_parent_conn = (needs_meta && needs_cost && cached_cost.is_none())
        .then(|| {
            cli_dir
                .zip(conv_id)
                .and_then(|(dir, cid)| resolve_conversation_db_path(dir, cid))
                .and_then(|db_path| open_readonly_db(&db_path).ok())
        })
        .flatten();

    let mut meta = if let Some(conn) = shared_parent_conn.as_ref() {
        query_session_metadata_from_conn(conn)
    } else if needs_meta {
        match (runtime.session, conv_id) {
            (SessionSource::LocalDir(dir), Some(id)) => query_session_metadata_in(dir, id),
            (SessionSource::Reader(reader), Some(id)) => reader.session_metadata(id),
            _ => SessionMetadata::default(),
        }
    } else {
        SessionMetadata::default()
    };

    let mut unpriced_turns = 0u64;
    if needs_cost {
        let resolved = cached_cost.or_else(|| {
            let (dir, cid) = cli_dir.zip(conv_id)?;
            match runtime.pricing {
                PricingSource::Disabled => None,
                PricingSource::Catalog(pricing) => estimate_conversation_cost_usd_with_parent_conn(
                    dir,
                    cid,
                    pricing,
                    shared_parent_conn.as_ref(),
                    None,
                ),
                PricingSource::Effective(mode) => {
                    let pricing = PricingCatalog::load_effective_in(
                        runtime.cache_dir,
                        mode,
                        runtime.now_unix_secs,
                    );
                    let cache_target = runtime
                        .cache_dir
                        .map(|cdir| (cdir, pricing_cache_fingerprint(Some(cdir))));
                    estimate_conversation_cost_usd_with_parent_conn(
                        dir,
                        cid,
                        &pricing,
                        shared_parent_conn.as_ref(),
                        cache_target,
                    )
                }
            }
        });
        if let Some(cost) = resolved {
            unpriced_turns = cost.unpriced_turns;
            meta.estimated_cost_usd = cost.usd;
            meta.estimated_subagent_cost_usd = cost.subagent_usd;
        }
    }

    (meta, unpriced_turns)
}

/// Parses `input` JSON and writes the statusline according to `runtime` and `cfg`.
///
/// # Examples
///
/// ```
/// use std::path::Path;
/// use agy_statusline::agy::db::{SessionMetadata, SpeedReader};
/// use agy_statusline::statusline::git::{GitInfo, GitProbe};
/// use agy_statusline::config::layout::LayoutConfig;
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
    let git_info = probe_git(&payload, runtime.git_probe, cfg);
    let (meta, unpriced_turns) = resolve_session_metadata_and_cost(&payload, runtime, cfg);
    let quota_state =
        resolve_statusline_quota_state(&payload, runtime.cache_dir, runtime.now_unix_secs, cfg);
    let extras = RenderExtras {
        unpriced_turns,
        quota_override: quota_state.fallback_quota.as_ref(),
        quota_history: quota_state.history.as_ref(),
        plan_tier_override: quota_state.plan_tier_override.as_deref(),
        ai_credits_fallback: quota_state.ai_credits_fallback,
    };

    write_statusline_with(
        &payload,
        &meta,
        &git_info,
        &extras,
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
        cfg.theme = crate::config::layout::ThemeStyle::Mono;
        let git = NoopGit;

        let reader_none = StubReader(None);
        let mut out = Vec::new();
        process_statusline(
            "",
            &StatuslineRuntime::minimal(&reader_none, &git),
            &cfg,
            &mut out,
        )
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
        assert_eq!(subagent_count(&payload), 2);

        let quota = payload.quota.unwrap();
        assert_eq!(quota["pool-5h"].remaining_amount, Some(420));
        assert_eq!(quota["pool-5h"].reset_in_seconds, Some(-15));
        assert_eq!(quota["pool-5h"].disabled, Some(false));
        assert_eq!(quota["pool-7d"].disabled, Some(true));
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
        cfg.theme = crate::config::layout::ThemeStyle::Mono;
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
        assert_eq!(workspace_basename(&p1), Some("active-dir"));

        let p2: StatuslinePayload = serde_json::from_str(
            r#"{
                "workspace": {"project_dir": "\t ", "current_dir": "   "},
                "cwd": " /tmp/fallback-cwd "
            }"#,
        )
        .unwrap();
        assert_eq!(workspace_basename(&p2), Some("fallback-cwd"));

        let p3: StatuslinePayload =
            serde_json::from_str(r#"{"workspace": {"project_dir": "  "}, "cwd": "  "}"#).unwrap();
        assert_eq!(workspace_basename(&p3), None);
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
        assert_eq!(workspace_path(&p1), Some("/tmp/proj-dir"));

        let p2: StatuslinePayload = serde_json::from_str(
            r#"{
                "workspace": {"current_dir": "\t ", "project_dir": "   "},
                "cwd": " /tmp/fallback-cwd "
            }"#,
        )
        .unwrap();
        assert_eq!(workspace_path(&p2), Some("/tmp/fallback-cwd"));

        let p3: StatuslinePayload =
            serde_json::from_str(r#"{"workspace": {"current_dir": "  "}, "cwd": "  "}"#).unwrap();
        assert_eq!(workspace_path(&p3), None);
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
        let blob = crate::agy::protobuf::encode_gen_blob(crate::agy::protobuf::GenBlobSpec {
            output_tok: 100,
            model_cost: Some(0.25),
            ..Default::default()
        });
        conn.execute(
            "INSERT INTO gen_metadata (idx, data) VALUES (1, ?1)",
            [blob],
        )
        .unwrap();
        drop(conn);

        let git = NoopGit;
        let pricing = PricingCatalog::default();
        let runtime = StatuslineRuntime {
            session: SessionSource::LocalDir(dir.path()),
            git_probe: &git,
            cache_dir: None,
            pricing: PricingSource::Catalog(&pricing),
            now_unix_secs: 0,
        };
        let mut cfg = LayoutConfig::preset_minimal();
        cfg.theme = crate::config::layout::ThemeStyle::Mono;
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
        spec: crate::agy::protobuf::GenBlobSpec<'_>,
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
            rusqlite::params![idx, crate::agy::protobuf::encode_gen_blob(spec)],
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
            crate::agy::protobuf::GenBlobSpec {
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
            crate::agy::protobuf::GenBlobSpec {
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

        let git = NoopGit;
        let runtime = StatuslineRuntime {
            session: SessionSource::LocalDir(&cli_dir),
            git_probe: &git,
            cache_dir: Some(&cache_dir),
            pricing: PricingSource::Effective(SyncMode::Offline),
            now_unix_secs: 1_700_000_000,
        };
        let mut cfg = LayoutConfig::preset_minimal();
        cfg.theme = crate::config::layout::ThemeStyle::Mono;
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
            crate::agy::protobuf::GenBlobSpec {
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
            crate::agy::protobuf::GenBlobSpec {
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
            crate::agy::protobuf::GenBlobSpec {
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

    #[test]
    fn quota_snapshot_persists_burn_rate_falls_back_when_empty_and_enriches_ultra_tier() {
        use crate::agy::quota::BucketHistory;
        use crate::config::layout::{GlyphMode, ThemeStyle};

        let cache = tempfile::tempdir().unwrap();
        let t0 = 1_700_000_000u64;
        // Seed a Hub RPC snapshot with Ultra 20x, low AI credits, one active bucket, and one expired bucket.
        let mut hist = BucketHistory::default();
        hist.record_sample(t0, 0.80, Some((t0 + 18_000) as i64));
        let seeded = QuotaSnapshot {
            updated_at: t0,
            source: QuotaSource::HubRpc,
            plan_tier: Some("Ultra".to_string()),
            ultra_tier_label: Some("20x".to_string()),
            email: Some("dev@example.com".to_string()),
            ai_credits: Some(30),
            quota: BTreeMap::from([
                (
                    "gemini-5h".to_string(),
                    QuotaBucket {
                        remaining_fraction: Some(0.80),
                        reset_in_seconds: Some(18_000),
                        ..Default::default()
                    },
                ),
                (
                    "3p-5h".to_string(),
                    QuotaBucket {
                        remaining_fraction: Some(0.10),
                        reset_time: Some("2020-01-01T00:00:00Z".to_string()),
                        ..Default::default()
                    },
                ),
            ]),
            history: BTreeMap::from([("gemini-5h".to_string(), hist)]),
        };
        save_quota_snapshot(cache.path(), &seeded).unwrap();

        let mut cfg = LayoutConfig::preset_minimal();
        cfg.theme = ThemeStyle::Mono;
        cfg.glyph_mode = GlyphMode::Ascii;
        cfg.no_color = true;
        cfg.responsive = false;
        cfg.rows = vec![vec![
            WidgetId::Plan,
            WidgetId::Quota5h,
            WidgetId::QuotaWeekly,
        ]];
        let reader = StubReader(None);
        let git = NoopGit;

        // Tick 1: empty quota payload (10 min after Hub RPC) -> falls back to fresh snapshot,
        // enriches "Pro" -> "Ultra 20x", adjusts reset countdown, skips expired 3p-5h, surfaces credits.
        let rt1 = StatuslineRuntime {
            session: SessionSource::Reader(&reader),
            git_probe: &git,
            cache_dir: Some(cache.path()),
            pricing: PricingSource::Disabled,
            now_unix_secs: t0 + 600,
        };
        let idle_json =
            r#"{"plan_tier":"Pro","email":"dev@example.com","model":{"id":"gemini-4"},"quota":{}}"#;
        let mut out1 = Vec::new();
        process_statusline(idle_json, &rt1, &cfg, &mut out1).unwrap();
        let text1 = String::from_utf8(out1).unwrap();
        assert!(text1.contains("Ultra 20x"), "text1: {text1:?}");
        assert!(text1.contains("5h 80% r:4h50m"), "text1: {text1:?}");
        assert!(text1.contains("credits 30"), "text1: {text1:?}");
        assert!(!text1.contains("3p 5h"), "expired bucket leaked: {text1:?}");

        // Tick 2: stdin observation 1h later at 60% remaining (20%/hr burn rate -> 3h ETA < 4h reset).
        let rt2 = StatuslineRuntime {
            session: SessionSource::Reader(&reader),
            git_probe: &git,
            cache_dir: Some(cache.path()),
            pricing: PricingSource::Disabled,
            now_unix_secs: t0 + 3_600,
        };
        let active_json = r#"{
            "plan_tier":"Pro",
            "email":"dev@example.com",
            "model":{"id":"gemini-4"},
            "quota":{"gemini-5h":{"remaining_fraction":0.60,"reset_in_seconds":14400}}
        }"#;
        let mut out2 = Vec::new();
        process_statusline(active_json, &rt2, &cfg, &mut out2).unwrap();
        let text2 = String::from_utf8(out2).unwrap();
        assert!(text2.contains("Ultra 20x"), "text2: {text2:?}");
        assert!(text2.contains("5h 60% r:4h !3h"), "text2: {text2:?}");

        let persisted = load_quota_snapshot(cache.path()).unwrap().unwrap();
        assert_eq!(persisted.source, QuotaSource::Stdin);
        assert_eq!(persisted.display_plan_tier().as_deref(), Some("Ultra 20x"));
        assert_eq!(persisted.history["gemini-5h"].samples.len(), 2);

        // Tick 3: > 6h later with empty quota -> stale fallback is skipped cleanly.
        let rt3 = StatuslineRuntime {
            session: SessionSource::Reader(&reader),
            git_probe: &git,
            cache_dir: Some(cache.path()),
            pricing: PricingSource::Disabled,
            now_unix_secs: t0 + 3_600 + MAX_QUOTA_FALLBACK_AGE_SECS + 60,
        };
        let empty_json = r#"{"email":"dev@example.com","model":{"id":"gemini-4"}}"#;
        let mut out3 = Vec::new();
        process_statusline(empty_json, &rt3, &cfg, &mut out3).unwrap();
        let text3 = String::from_utf8(out3).unwrap();
        assert!(!text3.contains("5h 60%"), "stale quota leaked: {text3:?}");
    }
}
