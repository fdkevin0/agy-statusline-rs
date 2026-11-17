//! Null-safe AGY statusline JSON schema and pipeline orchestration.

use std::io::{self, Write};
use std::path::Path;

use serde::Deserialize;
use serde_json::Value;

use crate::db::SpeedReader;
use crate::git::{GitInfo, GitProbe, SystemGitProbe};
use crate::layout::{LayoutConfig, WidgetId};
use crate::render::write_statusline;

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
    /// Active branch name.
    pub branch: Option<String>,
    /// Whether the working tree has uncommitted changes.
    pub dirty: Option<bool>,
}

/// Sandbox mode status.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default)]
pub struct SandboxInfo {
    /// Whether terminal sandbox is enabled.
    pub enabled: Option<bool>,
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

/// Null-safe top-level AGY statusline JSON payload passed via `stdin`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct StatuslinePayload {
    /// Agent execution state (`idle`, `thinking`, `working`, `tool_use`).
    pub agent_state: Option<String>,
    /// Active conversation UUID.
    pub conversation_id: Option<String>,
    /// Terminal column width.
    pub terminal_width: Option<usize>,
    /// Number of generated artifacts.
    pub artifact_count: Option<u64>,
    /// Background task count.
    pub task_count: Option<u64>,
    /// Subagent array or null.
    pub subagents: Option<Value>,
    /// Active model info (`null` when unset).
    pub model: Option<ModelInfo>,
    /// Git / VCS status (`null` outside repos).
    pub vcs: Option<VcsInfo>,
    /// Sandbox status.
    pub sandbox: Option<SandboxInfo>,
    /// Workspace directory paths.
    pub workspace: Option<WorkspaceInfo>,
    /// Context window metrics.
    pub context_window: Option<ContextWindow>,
    /// Session cost estimate.
    pub cost: Option<CostInfo>,
}

impl StatuslinePayload {
    /// Returns the number of active subagents when `subagents` is a JSON array.
    pub fn subagent_count(&self) -> usize {
        match &self.subagents {
            Some(Value::Array(items)) => items.len(),
            _ => 0,
        }
    }

    /// Returns the raw workspace directory path preferring `current_dir`, then `project_dir`.
    pub fn workspace_path(&self) -> Option<&str> {
        let ws = self.workspace.as_ref()?;
        ws.current_dir
            .as_deref()
            .filter(|s| !s.is_empty())
            .or_else(|| ws.project_dir.as_deref().filter(|s| !s.is_empty()))
    }

    /// Returns the directory basename preferring `workspace.project_dir`, then `current_dir`.
    pub fn workspace_basename(&self) -> Option<&str> {
        let ws = self.workspace.as_ref()?;
        let raw = ws
            .project_dir
            .as_deref()
            .filter(|s| !s.is_empty())
            .or_else(|| ws.current_dir.as_deref().filter(|s| !s.is_empty()))?;
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
    process_statusline_with_probes(input, reader, &SystemGitProbe, cfg, out)
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
    let meta = payload
        .conversation_id
        .as_deref()
        .map(|id| reader.session_metadata(id))
        .unwrap_or_default();
    let needs_git = cfg.rows.iter().flatten().any(|w| {
        matches!(w, WidgetId::Worktree) || (cfg.show_git_diff && matches!(w, WidgetId::Vcs))
    });
    let git_info = if needs_git {
        payload
            .workspace_path()
            .map(|dir| {
                let want_diff = cfg.show_git_diff
                    && cfg.rows.iter().flatten().any(|w| matches!(w, WidgetId::Vcs));
                git_probe.probe(Path::new(dir), want_diff)
            })
            .unwrap_or_default()
    } else {
        GitInfo::default()
    };
    write_statusline(&payload, &meta, &git_info, cfg, out)
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
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "◈ cache -- ⚡ --\n"
        );

        let mut out2 = Vec::new();
        process_statusline_with_config("{bad json", &StubReader(Some(100.0)), &cfg, &mut out2)
            .unwrap();
        assert_eq!(
            String::from_utf8(out2).unwrap(),
            "◈ cache -- ⚡ --\n"
        );
    }
}
