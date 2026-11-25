//! XDG-compliant statusline layout configuration, presets, and persistence.
//!
//! Stores user-defined multi-row layouts at
//! `$XDG_CONFIG_HOME/agy-statusline/config.json` (defaulting to
//! `~/.config/agy-statusline/config.json`).

use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::stats::{ConversationSort, StatsMetric, StatsTuiPrefs, TurnSort};
use crate::sys::fs::atomic_write;

/// Maximum number of configurable statusline rows.
pub const MAX_ROWS: usize = 3;

/// Default speed threshold (tokens/s) below which speed highlights in warning amber.
pub const DEFAULT_SPEED_WARN_THRESHOLD: f64 = 35.0;

macro_rules! define_widgets {
    (
        $(
            $(#[$meta:meta])*
            $variant:ident => ($label:literal, $display:literal, $priority:literal)
        ),+ $(,)?
    ) => {
        /// All statusline widgets available for placement on any row.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum WidgetId {
            $(
                $(#[$meta])*
                $variant,
            )+
        }

        impl WidgetId {
            /// Canonical list of all widgets in catalog display order.
            pub const ALL: &'static [WidgetId] = &[$(Self::$variant),+];

            /// Short machine/pill label.
            pub const fn label(self) -> &'static str {
                match self {
                    $(Self::$variant => $label,)+
                }
            }

            /// Descriptive display name shown in the widget catalog.
            pub const fn display_name(self) -> &'static str {
                match self {
                    $(Self::$variant => $display,)+
                }
            }

            /// Responsive fitting priority (`5` = anchor kept last, `1` = auxiliary dropped first).
            pub const fn priority(self) -> u8 {
                match self {
                    $(Self::$variant => $priority,)+
                }
            }
        }
    };
}

macro_rules! impl_cycle_enum {
    ($ty:ident, $all_vis:vis ALL => [$($variant:ident),+ $(,)?]) => {
        impl $ty {
            /// All variants in canonical cycle order.
            $all_vis const ALL: &'static [Self] = &[$(Self::$variant),+];

            /// Cycles forward (`dir = 1`) or backward (`dir = -1`).
            pub const fn step(self, dir: isize) -> Self {
                #[allow(dead_code)]
                enum Order {
                    $($variant),+
                }
                let idx = match self {
                    $(Self::$variant => Order::$variant as isize,)+
                };
                Self::ALL[(idx + dir).rem_euclid(Self::ALL.len() as isize) as usize]
            }
        }
    };
}

define_widgets! {
    /// Agent execution state (`● READY`, `◆ THINKING`, etc.).
    State => ("state", "Agent State", 5),
    /// Active model display name and reasoning effort.
    Model => ("model", "Model + Effort", 4),
    /// Active workspace/project directory basename.
    Directory => ("directory", "Workspace Dir", 2),
    /// Git branch, dirty marker, and optional diff stats.
    Vcs => ("vcs", "Git Branch + Diff", 3),
    /// Linked `git worktree` name (`⎇ wt:feat-x`).
    Worktree => ("worktree", "Git Worktree", 2),
    /// Unicode context usage bar + percentage.
    CtxBar => ("ctx_bar", "Context Progress Bar", 4),
    /// Context usage/remaining percentage only (`ctx 42.5%`).
    CtxPct => ("ctx_pct", "Context % Only", 4),
    /// Current/limit or cumulative session tokens.
    CtxTokens => ("ctx_tokens", "Session Tokens", 2),
    /// Cache hit percentage (`◈ cache 80.0%`).
    CacheHit => ("cache_hit", "Cache Hit %", 3),
    /// Formatted cache read token count (`↻ 800`).
    CacheRead => ("cache_read", "Cache Read Tokens", 1),
    /// Output generation speed (`⚡ 64 tok/s`).
    Speed => ("speed", "Output Speed (tok/s)", 3),
    /// Time to first token (`⏱ 1.8s`).
    Ttft => ("ttft", "First Token (TTFT)", 1),
    /// Thinking output tokens (`◆ think 4.2K`).
    Thinking => ("thinking", "Thinking Tokens", 1),
    /// Estimated session cost (`$0.14`).
    Cost => ("cost", "Session Cost ($)", 3),
    /// Active cycle mode (`◇ PLAN`, `◇ AUTO`, etc.) and `>200K` indicator.
    Mode => ("mode", "Cycle Mode (Plan/Auto)", 3),
    /// Rolling 5-hour quota remaining % + reset timer + pace warning.
    #[serde(rename = "quota_5h")]
    Quota5h => ("quota_5h", "5h Quota + Pace", 4),
    /// Weekly (or multi-day) quota remaining % + reset timer + pace warning.
    QuotaWeekly => ("quota_weekly", "Weekly Quota", 3),
    /// Artifact count (`art 2`).
    Artifacts => ("artifacts", "Artifact Count", 1),
    /// Active subagent count (`sub 2`).
    Subagents => ("subagents", "Active Subagents", 1),
    /// Background task count (`tasks 3`).
    Tasks => ("tasks", "Background Tasks", 1),
    /// Queued user inputs (`queue 1`).
    Queue => ("queue", "Queued Inputs", 2),
    /// Vim mode (`vim NORMAL`).
    Vim => ("vim", "Vim Mode", 2),
    /// Battle mode (`battle ACTIVE #1`).
    Battle => ("battle", "Battle Mode", 1),
    /// Sandbox status (`sandbox ON` / `sandbox ON+net` / `sandbox off`).
    Sandbox => ("sandbox", "Sandbox Status", 1),
    /// AGY CLI version (`v1.5.16`).
    Version => ("version", "AGY Version", 1),
    /// Account subscription tier (`Pro`, `Ultra`, etc.).
    Plan => ("plan", "Plan Tier", 1),
    /// Authenticated account email (`alice@example.com`).
    Account => ("account", "Account Email", 1),
    /// Active conversation title (truncated to fit).
    Title => ("title", "Conversation Title", 2),
    /// Active agent name (`agent researcher`).
    Agent => ("agent", "Active Agent", 2),
    /// Product name (`Antigravity`).
    Product => ("product", "Product Name", 1),
    /// Short active session identifier (`sid:8d0371e1`).
    Session => ("session", "Session ID", 1),
    /// Active transcript filename (`log:transcript.jsonl`).
    Transcript => ("transcript", "Transcript File", 1),
    /// Elastic left/center/right alignment spacer.
    Spacer => ("spacer", "Flex Spacer (L|R)", 5),
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

impl_cycle_enum!(VisualStyle, ALL => [Plain, Pill, Powerline]);

impl VisualStyle {
    /// Human-readable style name.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Plain => "Plain",
            Self::Pill => "Pill",
            Self::Powerline => "Powerline",
        }
    }
}

/// Separator inserted between widgets in [`VisualStyle::Plain`], [`ThemeStyle::Mono`], and `NO_COLOR` mode.
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

impl_cycle_enum!(SeparatorStyle, ALL => [Dot, Pipe, Slash, Space]);

impl SeparatorStyle {
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
    /// Nerd Font icons and rounded [`VisualStyle::Pill`] capsule caps.
    Nerd,
    /// Pure 7-bit ASCII symbols for basic terminals.
    Ascii,
}

impl_cycle_enum!(GlyphMode, ALL => [Unicode, Nerd, Ascii]);

impl GlyphMode {
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

impl_cycle_enum!(ThemeStyle, ALL => [Default, Nord, Catppuccin, TokyoNight, Gruvbox, Light, Mono]);

impl ThemeStyle {
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

impl_cycle_enum!(SpeedMode, ALL => [Weighted, Median, Instant, E2e]);

impl SpeedMode {
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

/// Built-in statusline layout presets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Preset {
    /// Two-line Full preset (replaces default AGY bar).
    Full,
    /// Two-line Powerline preset with Catppuccin palette.
    Powerline,
    /// Single-line Minimal preset.
    Minimal,
    /// Compact Addon bar stacked under AGY's built-in statusline.
    Addon,
}

impl_cycle_enum!(Preset, pub ALL => [Full, Powerline, Minimal, Addon]);

impl Preset {
    /// Canonical CLI preset name.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Powerline => "powerline",
            Self::Minimal => "minimal",
            Self::Addon => "addon",
        }
    }

    /// Human-readable display name shown in the TUI settings pane.
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Full => "Full",
            Self::Powerline => "Powerline",
            Self::Minimal => "Minimal",
            Self::Addon => "Addon",
        }
    }

    /// Builds the [`LayoutConfig`] corresponding to this preset.
    pub fn to_layout(self) -> LayoutConfig {
        match self {
            Self::Full => LayoutConfig::preset_full(),
            Self::Powerline => LayoutConfig::preset_powerline(),
            Self::Minimal => LayoutConfig::preset_minimal(),
            Self::Addon => LayoutConfig::preset_addon(),
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
    /// Separator between widgets in `Plain` style, `Mono` theme, and `NO_COLOR` mode.
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
    /// Automatically hide zero, absent, or inactive widgets (`worktree`, `ttft`, `thinking`,
    /// `quota_5h`, `quota_weekly`, `artifacts`, `subagents`, `tasks`, `queue`, `vim`, `battle`, `sandbox`).
    pub smart_hide: bool,
    /// Show `+ins -del` working-tree diff stats alongside `vcs` branch.
    pub show_git_diff: bool,
    /// Last selected `agy-statusline stats` aggregation metric (omitted when unset).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_from_str"
    )]
    pub stats_metric: Option<StatsMetric>,
    /// Last selected single-conversation Turns table sort order (omitted when unset).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_from_str"
    )]
    pub conv_turn_sort: Option<TurnSort>,
    /// Last selected historical conversations list sort order (omitted when unset).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_from_str"
    )]
    pub conv_list_sort: Option<ConversationSort>,
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
            bar_length: 8,
            speed_warn_threshold: DEFAULT_SPEED_WARN_THRESHOLD,
            stack_with_default: false,
            speed_mode: SpeedMode::Weighted,
            ctx_remaining: false,
            smart_hide: true,
            show_git_diff: true,
            stats_metric: None,
            conv_turn_sort: None,
            conv_list_sort: None,
            no_color: false,
        };
        cfg.normalize_rows();
        cfg
    }

    /// Preset 1 (`Full`): Two-line layout (replaces default AGY bar).
    pub fn preset_full() -> Self {
        Self::base_with_rows(vec![
            vec![
                WidgetId::State,
                WidgetId::Mode,
                WidgetId::Model,
                WidgetId::Directory,
                WidgetId::Vcs,
                WidgetId::Worktree,
                WidgetId::Queue,
                WidgetId::Vim,
                WidgetId::Battle,
                WidgetId::Version,
            ],
            vec![
                WidgetId::CtxBar,
                WidgetId::CacheHit,
                WidgetId::Speed,
                WidgetId::Cost,
                WidgetId::Quota5h,
                WidgetId::QuotaWeekly,
                WidgetId::Subagents,
            ],
        ])
    }

    /// Preset 2 (`Powerline`): Two-line Powerline layout with Catppuccin palette.
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

    /// Preset 3 (`Minimal`): Single-line minimal layout.
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

    /// Preset 4 (`Addon`): Compact metrics bar stacked under AGY's built-in statusline.
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

    /// Returns the matching built-in [`Preset`], ignoring runtime `no_color`.
    pub fn matching_preset(&self) -> Option<Preset> {
        let Self {
            rows,
            style,
            theme,
            separator,
            glyph_mode,
            responsive,
            bar_length,
            speed_warn_threshold,
            stack_with_default,
            speed_mode,
            ctx_remaining,
            smart_hide,
            show_git_diff,
            stats_metric: _,
            conv_turn_sort: _,
            conv_list_sort: _,
            no_color: _,
        } = self;
        Preset::ALL.iter().copied().find(|p| {
            let target = p.to_layout();
            style == &target.style
                && theme == &target.theme
                && separator == &target.separator
                && glyph_mode == &target.glyph_mode
                && responsive == &target.responsive
                && bar_length == &target.bar_length
                && speed_warn_threshold.to_bits() == target.speed_warn_threshold.to_bits()
                && stack_with_default == &target.stack_with_default
                && speed_mode == &target.speed_mode
                && ctx_remaining == &target.ctx_remaining
                && smart_hide == &target.smart_hide
                && show_git_diff == &target.show_git_diff
                && rows == &target.rows
        })
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

fn deserialize_optional_from_str<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: std::str::FromStr,
{
    let raw = Option::<String>::deserialize(deserializer)?;
    // Ignore unknown/legacy values instead of invalidating config.json.
    Ok(raw.and_then(|s| s.parse::<T>().ok()))
}

/// Loads all persisted `stats`/`conversations` TUI preferences from `config.json`.
pub fn load_saved_stats_tui_prefs(path: &Path) -> StatsTuiPrefs {
    try_load_layout_config(path)
        .ok()
        .map_or_else(StatsTuiPrefs::default, |cfg| StatsTuiPrefs {
            metric: cfg.stats_metric.unwrap_or_default(),
            turn_sort: cfg.conv_turn_sort.unwrap_or_default(),
            conv_sort: cfg.conv_list_sort.unwrap_or_default(),
        })
}

/// Persists changed `stats`/`conversations` TUI preferences into `config.json`
/// without disturbing layout settings or clobbering corrupt files.
pub fn save_stats_tui_prefs(
    path: &Path,
    metric: Option<StatsMetric>,
    turn_sort: Option<TurnSort>,
    conv_sort: Option<ConversationSort>,
) -> io::Result<()> {
    let mut cfg = match try_load_layout_config(path) {
        Ok(cfg) => cfg,
        Err(err) if err.kind() == io::ErrorKind::NotFound => LayoutConfig::default(),
        Err(_) => return Ok(()),
    };
    let mut changed = false;
    if let Some(m) = metric {
        if cfg.stats_metric != Some(m) {
            cfg.stats_metric = Some(m);
            changed = true;
        }
    }
    if let Some(ts) = turn_sort {
        if cfg.conv_turn_sort != Some(ts) {
            cfg.conv_turn_sort = Some(ts);
            changed = true;
        }
    }
    if let Some(cs) = conv_sort {
        if cfg.conv_list_sort != Some(cs) {
            cfg.conv_list_sort = Some(cs);
            changed = true;
        }
    }
    if !changed {
        return Ok(());
    }
    save_layout_config(path, &cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let cfg = LayoutConfig::preset_powerline();
        save_layout_config(&path, &cfg).unwrap();
        let loaded = load_layout_config(&path);
        assert_eq!(loaded, cfg);
        assert_eq!(loaded.matching_preset(), Some(Preset::Powerline));
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

    #[test]
    fn quota_5h_roundtrips_canonically() {
        assert_eq!(
            serde_json::to_string(&WidgetId::Quota5h).unwrap(),
            "\"quota_5h\""
        );
        assert_eq!(
            serde_json::from_str::<WidgetId>("\"quota_5h\"").unwrap(),
            WidgetId::Quota5h
        );
    }

    #[test]
    fn preset_full_uses_tuned_two_row_defaults() {
        let cfg = LayoutConfig::preset_full();
        assert_eq!(cfg.bar_length, 8);
        assert_eq!(cfg.speed_warn_threshold, 35.0);
        assert_eq!(cfg.rows.len(), MAX_ROWS);
        assert!(!cfg.rows[0].is_empty());
        assert_eq!(cfg.rows[0].last(), Some(&WidgetId::Version));
        assert!(!cfg.rows[1].is_empty());
        assert!(cfg.rows[2].is_empty());
        assert_eq!(WidgetId::CacheHit.priority(), 3);
    }

    #[test]
    fn stats_tui_prefs_persist_without_breaking_presets_custom_layout_or_corrupt_files() {
        use crate::stats::{ConversationSortField, SortDir, TurnSortField};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");

        // Missing file -> defaults (total metric, turn ascending, modified descending).
        let initial = load_saved_stats_tui_prefs(&path);
        assert_eq!(initial.metric, StatsMetric::Total);
        assert_eq!(
            initial.turn_sort,
            TurnSort {
                field: TurnSortField::Turn,
                dir: SortDir::Asc,
            }
        );
        assert_eq!(
            initial.conv_sort,
            ConversationSort {
                field: ConversationSortField::Modified,
                dir: SortDir::Desc,
            }
        );

        let cost_desc = TurnSort {
            field: TurnSortField::Cost,
            dir: SortDir::Desc,
        };
        let steps_asc = ConversationSort {
            field: ConversationSortField::Steps,
            dir: SortDir::Asc,
        };
        save_stats_tui_prefs(
            &path,
            Some(StatsMetric::Cost),
            Some(cost_desc),
            Some(steps_asc),
        )
        .unwrap();
        let loaded = load_saved_stats_tui_prefs(&path);
        assert_eq!(loaded.metric, StatsMetric::Cost);
        assert_eq!(loaded.turn_sort, cost_desc);
        assert_eq!(loaded.conv_sort, steps_asc);
        assert_eq!(
            load_layout_config(&path).matching_preset(),
            Some(Preset::Full)
        );

        // Preserves custom layout settings when updating preferences.
        let mut custom = LayoutConfig::preset_powerline();
        custom.bar_length = 12;
        save_layout_config(&path, &custom).unwrap();
        save_stats_tui_prefs(
            &path,
            Some(StatsMetric::InputOutput),
            Some(cost_desc),
            Some(steps_asc),
        )
        .unwrap();
        let reloaded = load_layout_config(&path);
        assert_eq!(reloaded.style, VisualStyle::Powerline);
        assert_eq!(reloaded.bar_length, 12);
        assert_eq!(reloaded.stats_metric, Some(StatsMetric::InputOutput));
        assert_eq!(reloaded.conv_turn_sort, Some(cost_desc));
        assert_eq!(reloaded.conv_list_sort, Some(steps_asc));

        // Legacy/unknown persisted values degrade safely to defaults without discarding layout settings.
        let raw = fs::read_to_string(&path)
            .unwrap()
            .replace("\"io\"", "\"input+output+cache_write\"")
            .replace("\"cost:desc\"", "\"nonexistent:desc\"")
            .replace("\"steps:asc\"", "\"badfield:asc\"");
        fs::write(&path, raw).unwrap();
        let degraded_cfg = load_layout_config(&path);
        assert_eq!(degraded_cfg.style, VisualStyle::Powerline);
        assert_eq!(degraded_cfg.bar_length, 12);
        assert_eq!(degraded_cfg.stats_metric, None);
        assert_eq!(degraded_cfg.conv_turn_sort, None);
        assert_eq!(degraded_cfg.conv_list_sort, None);
        let degraded = load_saved_stats_tui_prefs(&path);
        assert_eq!(degraded, StatsTuiPrefs::default());

        // Corrupt config file is left untouched rather than clobbered.
        fs::write(&path, "{broken").unwrap();
        save_stats_tui_prefs(&path, Some(StatsMetric::Turns), Some(cost_desc), None).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "{broken");
    }
}
