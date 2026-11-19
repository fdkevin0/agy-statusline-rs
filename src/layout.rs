//! XDG-compliant statusline layout configuration, presets, and persistence.
//!
//! Stores user-defined multi-row layouts at
//! `$XDG_CONFIG_HOME/agy-statusline/config.json` (defaulting to
//! `~/.config/agy-statusline/config.json`).

use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::fsutil::atomic_write;

/// Maximum number of configurable statusline rows.
pub const MAX_ROWS: usize = 3;

/// Default speed threshold (tokens/s) below which speed highlights in warning amber.
pub const DEFAULT_SPEED_WARN_THRESHOLD: f64 = 50.0;

/// All 33 statusline widgets available for placement on any row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WidgetId {
    /// Agent execution state (`● READY`, `◆ THINKING`, etc.).
    State,
    /// Active model display name and reasoning effort.
    Model,
    /// Active workspace/project directory basename.
    Directory,
    /// Git branch, dirty marker, and optional diff stats.
    Vcs,
    /// Linked `git worktree` name (`⎇ wt:feat-x`).
    Worktree,
    /// Unicode context usage bar + percentage.
    CtxBar,
    /// Context usage/remaining percentage only (`ctx 42.5%`).
    CtxPct,
    /// Current/limit or cumulative session tokens.
    CtxTokens,
    /// Cache hit percentage (`◈ cache 80.0%`).
    CacheHit,
    /// Formatted cache read token count (`↻ 800`).
    CacheRead,
    /// Output generation speed (`⚡ 64 tok/s`).
    Speed,
    /// Time to first token (`⏱ 1.8s`).
    Ttft,
    /// Thinking output tokens (`◆ think 4.2K`).
    Thinking,
    /// Estimated session cost (`$0.14`).
    Cost,
    /// Active cycle mode (`◇ PLAN`, `◇ AUTO`, etc.) and `>200K` indicator.
    Mode,
    /// Rolling 5-hour quota remaining % + reset timer + pace warning.
    Quota5h,
    /// Weekly (or multi-day) quota remaining % + reset timer + pace warning.
    QuotaWeekly,
    /// Artifact count (`art 2`).
    Artifacts,
    /// Active subagent count (`sub 2`).
    Subagents,
    /// Background task count (`tasks 3`).
    Tasks,
    /// Queued user inputs (`queue 1`).
    Queue,
    /// Vim mode (`vim NORMAL`).
    Vim,
    /// Battle mode (`battle ACTIVE #1`).
    Battle,
    /// Sandbox status (`sandbox ON` / `sandbox ON+net` / `sandbox off`).
    Sandbox,
    /// AGY CLI version (`v1.5.11`).
    Version,
    /// Account subscription tier (`Pro`, `Ultra`, etc.).
    Plan,
    /// Authenticated account email (`alice@example.com`).
    Account,
    /// Active conversation title (truncated to fit).
    Title,
    /// Active agent name (`agent researcher`).
    Agent,
    /// Product name (`Antigravity`).
    Product,
    /// Short active session identifier (`sid:8d0371e1`).
    Session,
    /// Active transcript filename (`log:transcript.jsonl`).
    Transcript,
    /// Elastic left/center/right alignment spacer.
    Spacer,
}

const WIDGET_META: [(WidgetId, &str, &str); 33] = [
    (WidgetId::State, "state", "Agent State"),
    (WidgetId::Model, "model", "Model + Effort"),
    (WidgetId::Directory, "directory", "Workspace Dir"),
    (WidgetId::Vcs, "vcs", "Git Branch + Diff"),
    (WidgetId::Worktree, "worktree", "Git Worktree"),
    (WidgetId::CtxBar, "ctx_bar", "Context Progress Bar"),
    (WidgetId::CtxPct, "ctx_pct", "Context % Only"),
    (WidgetId::CtxTokens, "ctx_tokens", "Session Tokens"),
    (WidgetId::CacheHit, "cache_hit", "Cache Hit %"),
    (WidgetId::CacheRead, "cache_read", "Cache Read Tokens"),
    (WidgetId::Speed, "speed", "Output Speed (tok/s)"),
    (WidgetId::Ttft, "ttft", "First Token (TTFT)"),
    (WidgetId::Thinking, "thinking", "Thinking Tokens"),
    (WidgetId::Cost, "cost", "Session Cost ($)"),
    (WidgetId::Mode, "mode", "Cycle Mode (Plan/Auto)"),
    (WidgetId::Quota5h, "quota_5h", "5h Quota + Pace"),
    (WidgetId::QuotaWeekly, "quota_weekly", "Weekly Quota"),
    (WidgetId::Artifacts, "artifacts", "Artifact Count"),
    (WidgetId::Subagents, "subagents", "Active Subagents"),
    (WidgetId::Tasks, "tasks", "Background Tasks"),
    (WidgetId::Queue, "queue", "Queued Inputs"),
    (WidgetId::Vim, "vim", "Vim Mode"),
    (WidgetId::Battle, "battle", "Battle Mode"),
    (WidgetId::Sandbox, "sandbox", "Sandbox Status"),
    (WidgetId::Version, "version", "AGY Version"),
    (WidgetId::Plan, "plan", "Plan Tier"),
    (WidgetId::Account, "account", "Account Email"),
    (WidgetId::Title, "title", "Conversation Title"),
    (WidgetId::Agent, "agent", "Active Agent"),
    (WidgetId::Product, "product", "Product Name"),
    (WidgetId::Session, "session", "Session ID"),
    (WidgetId::Transcript, "transcript", "Transcript File"),
    (WidgetId::Spacer, "spacer", "Flex Spacer (L|R)"),
];

impl WidgetId {
    /// Canonical list of all 33 widgets in catalog display order.
    pub const ALL: [WidgetId; 33] = [
        Self::State,
        Self::Model,
        Self::Directory,
        Self::Vcs,
        Self::Worktree,
        Self::CtxBar,
        Self::CtxPct,
        Self::CtxTokens,
        Self::CacheHit,
        Self::CacheRead,
        Self::Speed,
        Self::Ttft,
        Self::Thinking,
        Self::Cost,
        Self::Mode,
        Self::Quota5h,
        Self::QuotaWeekly,
        Self::Artifacts,
        Self::Subagents,
        Self::Tasks,
        Self::Queue,
        Self::Vim,
        Self::Battle,
        Self::Sandbox,
        Self::Version,
        Self::Plan,
        Self::Account,
        Self::Title,
        Self::Agent,
        Self::Product,
        Self::Session,
        Self::Transcript,
        Self::Spacer,
    ];

    /// Short machine/pill label.
    pub fn label(self) -> &'static str {
        WIDGET_META.iter().find(|m| m.0 == self).map_or("", |m| m.1)
    }

    /// Descriptive display name shown in the widget catalog.
    pub fn display_name(self) -> &'static str {
        WIDGET_META.iter().find(|m| m.0 == self).map_or("", |m| m.2)
    }

    /// Responsive fitting priority (`5` = anchor kept last, `1` = auxiliary dropped first).
    pub const fn priority(self) -> u8 {
        match self {
            Self::State | Self::Spacer => 5,
            Self::Model | Self::CtxBar | Self::CtxPct | Self::Quota5h => 4,
            Self::Mode | Self::Vcs | Self::Speed | Self::Cost | Self::QuotaWeekly => 3,
            Self::Directory
            | Self::CacheHit
            | Self::CtxTokens
            | Self::Worktree
            | Self::Vim
            | Self::Queue
            | Self::Title
            | Self::Agent => 2,
            Self::CacheRead
            | Self::Ttft
            | Self::Thinking
            | Self::Subagents
            | Self::Tasks
            | Self::Artifacts
            | Self::Battle
            | Self::Sandbox
            | Self::Version
            | Self::Plan
            | Self::Account
            | Self::Product
            | Self::Session
            | Self::Transcript => 1,
        }
    }
}

/// Visual rendering style for statusline rows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VisualStyle {
    /// Clean foreground colors joined by separator.
    #[default]
    Plain,
    /// Subtle dark capsule pills with semantic accents.
    Pill,
    /// Powerline arrow-connected segments (`\u{e0b0}`).
    Powerline,
}

impl VisualStyle {
    const ALL: [Self; 3] = [Self::Plain, Self::Pill, Self::Powerline];

    /// Cycles forward (`dir = 1`) or backward (`dir = -1`).
    pub const fn step(self, dir: isize) -> Self {
        let idx = match self {
            Self::Plain => 0isize,
            Self::Pill => 1,
            Self::Powerline => 2,
        };
        Self::ALL[(idx + dir).rem_euclid(3) as usize]
    }

    /// Cycles forward to the next style.
    pub const fn next(self) -> Self {
        self.step(1)
    }
    /// Cycles backward to the previous style.
    pub const fn prev(self) -> Self {
        self.step(-1)
    }

    /// Human-readable style name.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Plain => "Plain",
            Self::Pill => "Pill",
            Self::Powerline => "Powerline",
        }
    }
}

/// Separator inserted between widgets in [`VisualStyle::Plain`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeparatorStyle {
    /// Middle dot: ` · `
    #[default]
    Dot,
    /// Vertical bar: ` │ `
    Pipe,
    /// Diagonal slash: ` ╱ `
    Slash,
    /// Single space: ` `
    Space,
}

impl SeparatorStyle {
    const ALL: [Self; 4] = [Self::Dot, Self::Pipe, Self::Slash, Self::Space];

    /// Cycles forward or backward.
    pub const fn step(self, dir: isize) -> Self {
        let idx = match self {
            Self::Dot => 0isize,
            Self::Pipe => 1,
            Self::Slash => 2,
            Self::Space => 3,
        };
        Self::ALL[(idx + dir).rem_euclid(4) as usize]
    }

    /// Cycles forward to the next separator.
    pub const fn next(self) -> Self {
        self.step(1)
    }
    /// Cycles backward to the previous separator.
    pub const fn prev(self) -> Self {
        self.step(-1)
    }

    /// Plain text separator string.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Dot => " · ",
            Self::Pipe => " │ ",
            Self::Slash => " ╱ ",
            Self::Space => " ",
        }
    }

    /// ASCII-safe separator string.
    pub const fn as_ascii_str(self) -> &'static str {
        match self {
            Self::Dot => " * ",
            Self::Pipe => " | ",
            Self::Slash => " / ",
            Self::Space => " ",
        }
    }

    /// Human-readable separator label.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Dot => "dot (·)",
            Self::Pipe => "pipe (│)",
            Self::Slash => "slash (╱)",
            Self::Space => "space",
        }
    }
}

/// Glyph rendering mode across widgets and separators.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GlyphMode {
    /// Crisp cross-platform Unicode glyphs (default).
    #[default]
    Unicode,
    /// Nerd Font icons and rounded Powerline capsule caps.
    Nerd,
    /// Pure 7-bit ASCII symbols for basic terminals.
    Ascii,
}

impl GlyphMode {
    const ALL: [Self; 3] = [Self::Unicode, Self::Nerd, Self::Ascii];

    /// Cycles forward or backward.
    pub const fn step(self, dir: isize) -> Self {
        let idx = match self {
            Self::Unicode => 0isize,
            Self::Nerd => 1,
            Self::Ascii => 2,
        };
        Self::ALL[(idx + dir).rem_euclid(3) as usize]
    }

    /// Cycles forward to the next glyph mode.
    pub const fn next(self) -> Self {
        self.step(1)
    }

    /// Cycles backward to the previous glyph mode.
    pub const fn prev(self) -> Self {
        self.step(-1)
    }

    /// Human-readable glyph mode label.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Unicode => "Unicode",
            Self::Nerd => "Nerd Font",
            Self::Ascii => "ASCII",
        }
    }
}

/// Color palette applied across segments.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeStyle {
    /// Vibrant dark-terminal 256-color palette.
    #[default]
    Default,
    /// Arctic slate and frost-cyan Nord palette.
    Nord,
    /// Catppuccin Mocha pastel palette.
    Catppuccin,
    /// Deep twilight Tokyo Night palette.
    TokyoNight,
    /// Warm retro Gruvbox palette.
    Gruvbox,
    /// High-contrast light-terminal palette.
    Light,
    /// Plain text without ANSI escapes.
    Mono,
}

impl ThemeStyle {
    const ALL: [Self; 7] = [
        Self::Default,
        Self::Nord,
        Self::Catppuccin,
        Self::TokyoNight,
        Self::Gruvbox,
        Self::Light,
        Self::Mono,
    ];

    /// Cycles forward or backward.
    pub const fn step(self, dir: isize) -> Self {
        let idx = match self {
            Self::Default => 0isize,
            Self::Nord => 1,
            Self::Catppuccin => 2,
            Self::TokyoNight => 3,
            Self::Gruvbox => 4,
            Self::Light => 5,
            Self::Mono => 6,
        };
        Self::ALL[(idx + dir).rem_euclid(7) as usize]
    }

    /// Cycles forward to the next theme.
    pub const fn next(self) -> Self {
        self.step(1)
    }
    /// Cycles backward to the previous theme.
    pub const fn prev(self) -> Self {
        self.step(-1)
    }

    /// Human-readable theme name.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Default => "Default",
            Self::Nord => "Nord",
            Self::Catppuccin => "Catppuccin",
            Self::TokyoNight => "Tokyo Night",
            Self::Gruvbox => "Gruvbox",
            Self::Light => "Light",
            Self::Mono => "Mono",
        }
    }
}

/// Speed calculation mode for the `speed` widget.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpeedMode {
    /// 5-turn token-weighted decode average (`sum(output) / sum(decode_secs)`).
    #[default]
    Weighted,
    /// Median single-turn decode speed across up to 9 valid completed turns.
    Median,
    /// Latest single completed turn decode speed (`output / decode_secs`).
    Instant,
    /// Latest turn end-to-end throughput including TTFT (`output / (ttft + decode_secs)`).
    E2e,
}

impl SpeedMode {
    const ALL: [Self; 4] = [Self::Weighted, Self::Median, Self::Instant, Self::E2e];

    /// Cycles forward or backward.
    pub const fn step(self, dir: isize) -> Self {
        let idx = match self {
            Self::Weighted => 0isize,
            Self::Median => 1,
            Self::Instant => 2,
            Self::E2e => 3,
        };
        Self::ALL[(idx + dir).rem_euclid(4) as usize]
    }

    /// Human-readable name.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Weighted => "Weighted 5t",
            Self::Median => "Median",
            Self::Instant => "Instant",
            Self::E2e => "E2E (+TTFT)",
        }
    }
}

/// Persistent user configuration for multi-row statusline layout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LayoutConfig {
    /// Ordered rows of widgets (normalized to 3 rows).
    pub rows: Vec<Vec<WidgetId>>,
    /// Visual segment style (`plain`, `pill`, `powerline`).
    pub style: VisualStyle,
    /// Color theme (`default`, `nord`, `catppuccin`, `tokyo_night`, `gruvbox`, `light`, `mono`).
    pub theme: ThemeStyle,
    /// Separator between widgets in `Plain` style.
    pub separator: SeparatorStyle,
    /// Glyph set (`unicode`, `nerd`, `ascii`).
    pub glyph_mode: GlyphMode,
    /// Trim overflowing widgets when terminal width is known.
    pub responsive: bool,
    /// Character length of `ctx_bar` (`4..=40`).
    pub bar_length: usize,
    /// Speed threshold (token/s) below which speed highlights in warning amber.
    pub speed_warn_threshold: f64,
    /// Whether AGY `settings.json` stacks this bar under its built-in bar or replaces it.
    pub stack_with_default: bool,
    /// Speed calculation method (`weighted`, `median`, `instant`, `e2e`).
    pub speed_mode: SpeedMode,
    /// Display remaining context % (`rem 57.5%`) instead of used % (`ctx 42.5%`).
    pub ctx_remaining: bool,
    /// Automatically hide zero/inactive widgets (`art 0`, `sub 0`, `tasks 0`, `sandbox off`, main `worktree`).
    pub smart_hide: bool,
    /// Show `+ins -del` working-tree diff stats alongside `vcs` branch.
    pub show_git_diff: bool,
    /// Runtime `NO_COLOR` flag injected at the CLI boundary (not serialized).
    #[serde(skip)]
    pub no_color: bool,
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self::preset_full()
    }
}

impl LayoutConfig {
    fn base_with_rows(rows: Vec<Vec<WidgetId>>) -> Self {
        let mut cfg = Self {
            rows,
            style: VisualStyle::Plain,
            theme: ThemeStyle::Default,
            separator: SeparatorStyle::Dot,
            glyph_mode: GlyphMode::Unicode,
            responsive: true,
            bar_length: 12,
            speed_warn_threshold: DEFAULT_SPEED_WARN_THRESHOLD,
            stack_with_default: false,
            speed_mode: SpeedMode::Weighted,
            ctx_remaining: false,
            smart_hide: true,
            show_git_diff: true,
            no_color: false,
        };
        cfg.normalize_rows();
        cfg
    }

    /// Preset 1: Single-line Full Dashboard (replaces default AGY bar).
    pub fn preset_full() -> Self {
        Self::base_with_rows(vec![vec![
            WidgetId::State,
            WidgetId::Mode,
            WidgetId::Directory,
            WidgetId::Vcs,
            WidgetId::Worktree,
            WidgetId::CtxBar,
            WidgetId::CacheHit,
            WidgetId::CacheRead,
            WidgetId::Speed,
            WidgetId::Ttft,
            WidgetId::Cost,
            WidgetId::Quota5h,
            WidgetId::Sandbox,
        ]])
    }

    /// Preset 2: Powerline Pro with Catppuccin palette.
    pub fn preset_powerline() -> Self {
        let mut cfg = Self::base_with_rows(vec![
            vec![
                WidgetId::State,
                WidgetId::Mode,
                WidgetId::Model,
                WidgetId::Directory,
                WidgetId::Vcs,
            ],
            vec![
                WidgetId::CtxBar,
                WidgetId::CacheHit,
                WidgetId::Speed,
                WidgetId::Cost,
                WidgetId::Quota5h,
            ],
        ]);
        cfg.style = VisualStyle::Powerline;
        cfg.theme = ThemeStyle::Catppuccin;
        cfg
    }

    /// Preset 3: Minimal single-line statusline.
    pub fn preset_minimal() -> Self {
        Self::base_with_rows(vec![vec![
            WidgetId::State,
            WidgetId::Model,
            WidgetId::Vcs,
            WidgetId::CtxPct,
            WidgetId::CacheHit,
            WidgetId::Speed,
        ]])
    }

    /// Preset 4: Metrics Addon bar stacked under AGY's built-in statusline.
    pub fn preset_addon() -> Self {
        let mut cfg = Self::base_with_rows(vec![vec![
            WidgetId::CacheHit,
            WidgetId::CacheRead,
            WidgetId::Speed,
            WidgetId::Cost,
        ]]);
        cfg.style = VisualStyle::Pill;
        cfg.separator = SeparatorStyle::Space;
        cfg.stack_with_default = true;
        cfg
    }

    /// Ensures `rows` always has exactly [`MAX_ROWS`] slots, `bar_length` is clamped,
    /// and `speed_warn_threshold` is finite and non-negative.
    pub fn normalize_rows(&mut self) {
        while self.rows.len() < MAX_ROWS {
            self.rows.push(Vec::new());
        }
        self.rows.truncate(MAX_ROWS);
        self.bar_length = self.bar_length.clamp(4, 40);
        if !self.speed_warn_threshold.is_finite() || self.speed_warn_threshold < 0.0 {
            self.speed_warn_threshold = DEFAULT_SPEED_WARN_THRESHOLD;
        }
    }

    /// Finds which row (`0..3`) currently hosts `widget`, if any.
    pub fn locate_widget(&self, widget: WidgetId) -> Option<(usize, usize)> {
        for (r_idx, row) in self.rows.iter().enumerate() {
            if let Some(c_idx) = row.iter().position(|&w| w == widget) {
                return Some((r_idx, c_idx));
            }
        }
        None
    }

    /// Returns the matching built-in preset index (`0..=3`), ignoring runtime `no_color`.
    pub fn matching_preset_index(&self) -> Option<usize> {
        let mut norm = self.clone();
        norm.normalize_rows();
        norm.no_color = false;
        let presets = [
            Self::preset_full(),
            Self::preset_powerline(),
            Self::preset_minimal(),
            Self::preset_addon(),
        ];
        presets.iter().position(|p| p == &norm)
    }
}

/// Loads [`LayoutConfig`] from `path`, falling back to [`LayoutConfig::default`] if absent or invalid.
pub fn load_layout_config(path: &Path) -> LayoutConfig {
    try_load_layout_config(path).unwrap_or_default()
}

pub(crate) fn try_load_layout_config(path: &Path) -> io::Result<LayoutConfig> {
    let content = fs::read_to_string(path)?;
    let mut cfg: LayoutConfig = serde_json::from_str(&content)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    cfg.normalize_rows();
    Ok(cfg)
}

/// Atomically writes `config` to `path` as pretty-printed JSON.
pub fn save_layout_config(path: &Path, config: &LayoutConfig) -> io::Result<()> {
    let mut json = serde_json::to_string_pretty(config)
        .map_err(|e| io::Error::other(format!("failed to serialize layout config: {e}")))?;
    json.push('\n');
    atomic_write(path, json.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[test]
    fn partial_config_json_deserializes_with_defaults() {
        let old_json = r#"{
            "rows": [["state", "model", "vcs"], ["ctx_bar", "sandbox"], ["cache_hit", "speed"]],
            "separator": "dot",
            "theme": "default",
            "responsive": true,
            "bar_length": 15,
            "speed_warn_threshold": 50.0
        }"#;
        let mut cfg: LayoutConfig = serde_json::from_str(old_json).unwrap();
        cfg.normalize_rows();
        assert_eq!(cfg.style, VisualStyle::Plain);
        assert_eq!(cfg.glyph_mode, GlyphMode::Unicode);
        assert!(!cfg.stack_with_default);
        assert_eq!(
            cfg.rows[0],
            vec![WidgetId::State, WidgetId::Model, WidgetId::Vcs]
        );
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = env::temp_dir().join(format!("agy_layout_rt_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("config.json");
        let cfg = LayoutConfig::preset_powerline();
        save_layout_config(&path, &cfg).unwrap();
        let loaded = load_layout_config(&path);
        assert_eq!(loaded, cfg);
        assert_eq!(loaded.matching_preset_index(), Some(1));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn speed_mode_cycles_with_median_and_serializes() {
        let s0 = SpeedMode::Weighted;
        let s1 = s0.step(1);
        let s2 = s1.step(1);
        let s3 = s2.step(1);
        let s4 = s3.step(1);
        assert_eq!(s1, SpeedMode::Median);
        assert_eq!(s1.name(), "Median");
        assert_eq!(s2, SpeedMode::Instant);
        assert_eq!(s3, SpeedMode::E2e);
        assert_eq!(s4, SpeedMode::Weighted);
        assert_eq!(SpeedMode::Weighted.step(-1), SpeedMode::E2e);
        assert_eq!(SpeedMode::Instant.step(-1), SpeedMode::Median);

        let json = serde_json::to_string(&SpeedMode::Median).unwrap();
        assert_eq!(json, "\"median\"");
        let parsed: SpeedMode = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, SpeedMode::Median);
    }

    #[test]
    fn normalize_rows_resets_invalid_speed_warn_threshold() {
        let mut cfg = LayoutConfig::preset_minimal();
        cfg.speed_warn_threshold = f64::NAN;
        cfg.normalize_rows();
        assert_eq!(cfg.speed_warn_threshold, DEFAULT_SPEED_WARN_THRESHOLD);

        cfg.speed_warn_threshold = f64::INFINITY;
        cfg.normalize_rows();
        assert_eq!(cfg.speed_warn_threshold, DEFAULT_SPEED_WARN_THRESHOLD);

        cfg.speed_warn_threshold = f64::NEG_INFINITY;
        cfg.normalize_rows();
        assert_eq!(cfg.speed_warn_threshold, DEFAULT_SPEED_WARN_THRESHOLD);

        cfg.speed_warn_threshold = -1.0;
        cfg.normalize_rows();
        assert_eq!(cfg.speed_warn_threshold, DEFAULT_SPEED_WARN_THRESHOLD);

        cfg.speed_warn_threshold = 0.0;
        cfg.normalize_rows();
        assert_eq!(cfg.speed_warn_threshold, 0.0);

        cfg.speed_warn_threshold = 35.5;
        cfg.normalize_rows();
        assert_eq!(cfg.speed_warn_threshold, 35.5);
    }
}
