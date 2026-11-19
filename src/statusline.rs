//! Null-safe AGY statusline JSON schema and pipeline orchestration.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::Path;

use serde::Deserialize;
use serde_json::Value;

use crate::calibration::calibrate_quota_views_at;
use crate::db::SpeedReader;
use crate::git::{GitInfo, GitProbe, SystemGitProbe};
use crate::layout::{LayoutConfig, WidgetId};
use crate::metrics::{resolve_quota_5h, resolve_quota_weekly, QuotaView};
use crate::paths::resolve_agy_cli_dir;
use crate::render::write_statusline_with_quotas;
use crate::stats::{estimate_conversation_cost_usd, PricingCatalog, SyncMode};

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
    /// Remaining context window percentage.
    pub remaining_percentage: Option<f64>,
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
    pub fn subagent_count(&self) -> usize {
        self.active_subagents().count()
    }

    /// Returns the role or name of the sole active subagent when `subagent_count() == 1`.
    pub fn single_active_subagent_label(&self) -> Option<&str> {
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

    /// Returns the raw workspace directory path preferring `workspace.current_dir`,
    /// then `workspace.project_dir`, then top-level `cwd`.
    pub fn workspace_path(&self) -> Option<&str> {
        self.workspace
            .as_ref()
            .and_then(|ws| {
                ws.current_dir
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .or_else(|| ws.project_dir.as_deref().filter(|s| !s.is_empty()))
            })
            .or_else(|| self.cwd.as_deref().filter(|s| !s.is_empty()))
    }

    /// Returns the directory basename preferring `workspace.project_dir`,
    /// then `workspace.current_dir`, then top-level `cwd`.
    pub fn workspace_basename(&self) -> Option<&str> {
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

/// Parses `input` JSON and writes the statusline using [`LayoutConfig::default`].
///
/// # Examples
///
/// ```
/// use agy_statusline::db::SpeedReader;
/// use agy_statusline::statusline::process_statusline;
///
/// struct FixedSpeed(Option<f64>);
/// impl SpeedReader for FixedSpeed {
///     fn last_speed(&self, _id: &str) -> Option<f64> { self.0 }
/// }
///
/// let json = r#"{"agent_state":"idle","conversation_id":"8d0371e1-5a05-423b-a8bc-ed75995f9a8b","context_window":{"current_usage":{"input_tokens":200,"cache_read_input_tokens":800,"cache_creation_input_tokens":0}}}"#;
/// let mut out = Vec::new();
/// process_statusline(json, &FixedSpeed(Some(60.0)), &mut out).unwrap();
/// let text = String::from_utf8(out).unwrap();
/// assert!(text.contains("READY"));
/// assert!(text.contains("80.0%"));
/// assert!(text.contains("60 tok/s"));
/// ```
pub fn process_statusline<W: Write>(
    input: &str,
    reader: &dyn SpeedReader,
    out: &mut W,
) -> io::Result<()> {
    process_statusline_with_config(input, reader, &LayoutConfig::default(), out)
}

/// Parses `input` JSON and writes the statusline according to `cfg`.
pub fn process_statusline_with_config<W: Write>(
    input: &str,
    reader: &dyn SpeedReader,
    cfg: &LayoutConfig,
    out: &mut W,
) -> io::Result<()> {
    process_statusline_with_probes(input, reader, &SystemGitProbe::default(), cfg, out)
}

fn probe_session_and_git(
    payload: &StatuslinePayload,
    reader: &dyn SpeedReader,
    git_probe: &dyn GitProbe,
    cfg: &LayoutConfig,
) -> (crate::db::SessionMetadata, GitInfo) {
    let meta = payload
        .conversation_id
        .as_deref()
        .map(|id| reader.session_metadata(id))
        .unwrap_or_default();
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
    let git_info = if needs_git {
        payload
            .workspace_path()
            .map(|dir| {
                let want_diff = cfg.show_git_diff && has_vcs;
                git_probe.probe(Path::new(dir), want_diff)
            })
            .unwrap_or_default()
    } else {
        GitInfo::default()
    };
    (meta, git_info)
}

/// Parses `input` JSON using explicit [`SpeedReader`] and [`GitProbe`] boundaries.
pub fn process_statusline_with_probes<W: Write>(
    input: &str,
    reader: &dyn SpeedReader,
    git_probe: &dyn GitProbe,
    cfg: &LayoutConfig,
    out: &mut W,
) -> io::Result<()> {
    let payload: StatuslinePayload = serde_json::from_str(input).unwrap_or_default();
    let (meta, git_info) = probe_session_and_git(&payload, reader, git_probe, cfg);
    let quota_5h = resolve_quota_5h(&payload);
    let quota_weekly = resolve_quota_weekly(&payload);
    write_statusline_with_quotas(&payload, &meta, &git_info, quota_5h, quota_weekly, cfg, out)
}

fn estimate_payload_cost_usd(
    payload: &StatuslinePayload,
    pricing: &PricingCatalog,
) -> Option<f64> {
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
                .estimate_cost(&candidates, inp, 0, 0, out)
                .map(|(_, usd)| usd);
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
        .map(|(_, usd)| usd)
}

/// Parses `input` JSON using explicit [`SpeedReader`], [`GitProbe`], and quota token calibration cache directory.
pub fn process_statusline_with_calibration<W: Write>(
    input: &str,
    reader: &dyn SpeedReader,
    git_probe: &dyn GitProbe,
    calibration_dir: Option<&Path>,
    now_unix_secs: u64,
    cfg: &LayoutConfig,
    out: &mut W,
) -> io::Result<()> {
    let payload: StatuslinePayload = serde_json::from_str(input).unwrap_or_default();
    let (mut meta, git_info) = probe_session_and_git(&payload, reader, git_probe, cfg);
    let needs_cost = cfg.rows.iter().flatten().any(|w| matches!(w, WidgetId::Cost))
        && payload.cost.as_ref().and_then(|c| c.total_usd).is_none();
    if needs_cost {
        let pricing = PricingCatalog::load_effective(SyncMode::SpawnIfStale, now_unix_secs);
        meta.estimated_cost_usd = payload
            .conversation_id
            .as_deref()
            .and_then(|cid| {
                resolve_agy_cli_dir()
                    .and_then(|cli_dir| estimate_conversation_cost_usd(&cli_dir, cid, &pricing))
            })
            .or_else(|| estimate_payload_cost_usd(&payload, &pricing));
    }
    let (quota_5h, quota_weekly): (Option<QuotaView>, Option<QuotaView>) =
        calibrate_quota_views_at(&payload, calibration_dir, now_unix_secs);
    write_statusline_with_quotas(&payload, &meta, &git_info, quota_5h, quota_weekly, cfg, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubReader(Option<f64>);
    impl SpeedReader for StubReader {
        fn last_speed(&self, _id: &str) -> Option<f64> {
            self.0
        }
    }

    struct StubGit(GitInfo);
    impl GitProbe for StubGit {
        fn probe(&self, _dir: &Path, _include_diff: bool) -> GitInfo {
            self.0.clone()
        }
    }

    #[test]
    fn explicit_null_model_preserves_other_fields() {
        let json = r#"{
            "agent_state": "thinking",
            "conversation_id": "8d0371e1-5a05-423b-a8bc-ed75995f9a8b",
            "model": null,
            "vcs": {"branch": "main", "dirty": true},
            "workspace": null,
            "cost": null
        }"#;
        let mut cfg = LayoutConfig::preset_minimal();
        cfg.theme = crate::layout::ThemeStyle::Mono;

        let mut out = Vec::new();
        process_statusline_with_config(json, &StubReader(Some(88.0)), &cfg, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("THINKING"));
        assert!(text.contains("main*"));
        assert!(text.contains("88 tok/s"));
    }

    #[test]
    fn empty_and_invalid_json_degrades_gracefully() {
        let mut cfg = LayoutConfig::preset_addon();
        cfg.theme = crate::layout::ThemeStyle::Mono;

        let mut out = Vec::new();
        process_statusline_with_config("", &StubReader(None), &cfg, &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "◈ cache -- ⚡ --\n");

        let mut out2 = Vec::new();
        process_statusline_with_config("{bad json", &StubReader(Some(100.0)), &cfg, &mut out2)
            .unwrap();
        assert_eq!(String::from_utf8(out2).unwrap(), "◈ cache -- ⚡ --\n");
    }

    #[test]
    fn falls_back_to_cwd_and_git_probe_when_vcs_branch_absent() {
        let json = r#"{
            "agent_state": "idle",
            "cwd": "/tmp/my-repo",
            "vcs": {"type": "git"}
        }"#;
        let mut cfg = LayoutConfig::preset_minimal();
        cfg.theme = crate::layout::ThemeStyle::Mono;
        cfg.show_git_diff = false;
        let git = StubGit(GitInfo {
            branch: Some("feat/probe".to_string()),
            worktree: None,
            added: 0,
            deleted: 0,
        });

        let mut out = Vec::new();
        process_statusline_with_probes(json, &StubReader(None), &git, &cfg, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("feat/probe"));
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

        let hg_json = r#"{
            "cwd": "/tmp/hg-repo",
            "vcs": {"type": "hg", "branch": "default"}
        }"#;
        let mut out = Vec::new();
        process_statusline_with_probes(hg_json, &StubReader(None), &CountingGit(&calls), &cfg, &mut out)
            .unwrap();
        assert_eq!(calls.get(), 0);

        let citc_json = r#"{
            "cwd": "/tmp/citc-ws",
            "vcs": {"type": "citc", "client": "my-client"}
        }"#;
        out.clear();
        process_statusline_with_probes(
            citc_json,
            &StubReader(None),
            &CountingGit(&calls),
            &cfg,
            &mut out,
        )
        .unwrap();
        assert_eq!(calls.get(), 0);

        // Explicit "git" still probes.
        let git_json = r#"{
            "cwd": "/tmp/git-repo",
            "vcs": {"type": "git"}
        }"#;
        out.clear();
        process_statusline_with_probes(git_json, &StubReader(None), &CountingGit(&calls), &cfg, &mut out)
            .unwrap();
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
}
