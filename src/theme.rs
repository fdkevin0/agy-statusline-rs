//! Color palettes and glyph sets (`Unicode`, `Nerd Font`, `ASCII`).

use crate::layout::{GlyphMode, ThemeStyle};

/// 256-color ANSI palette indices for statusline segments and surfaces.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub ready: u8,
    pub thinking: u8,
    pub working: u8,
    pub tool: u8,
    pub review: u8,
    pub error: u8,
    pub model: u8,
    pub dir: u8,
    pub vcs_clean: u8,
    pub vcs_dirty: u8,
    pub ctx_ok: u8,
    pub ctx_warn: u8,
    pub ctx_crit: u8,
    pub cache: u8,
    pub speed_ok: u8,
    pub speed_slow: u8,
    pub cost: u8,
    pub muted: u8,
    pub sep: u8,
    pub pill_bg: u8,
    pub pl_bg_a: u8,
    pub pl_bg_b: u8,
    pub pl_accent_fg: u8,
}

const PALETTE_DEFAULT: Palette = Palette {
    ready: 78, thinking: 220, working: 81, tool: 177, review: 141, error: 203,
    model: 183, dir: 117, vcs_clean: 75, vcs_dirty: 203, ctx_ok: 78, ctx_warn: 220,
    ctx_crit: 203, cache: 111, speed_ok: 80, speed_slow: 214, cost: 150, muted: 245,
    sep: 240, pill_bg: 236, pl_bg_a: 237, pl_bg_b: 235, pl_accent_fg: 232,
};

const PALETTE_NORD: Palette = Palette {
    ready: 108, thinking: 222, working: 110, tool: 139, review: 139, error: 174,
    model: 110, dir: 109, vcs_clean: 74, vcs_dirty: 174, ctx_ok: 108, ctx_warn: 222,
    ctx_crit: 174, cache: 110, speed_ok: 109, speed_slow: 215, cost: 151, muted: 244,
    sep: 239, pill_bg: 236, pl_bg_a: 238, pl_bg_b: 235, pl_accent_fg: 233,
};

const PALETTE_CATPPUCCIN: Palette = Palette {
    ready: 114, thinking: 223, working: 117, tool: 183, review: 183, error: 210,
    model: 183, dir: 117, vcs_clean: 111, vcs_dirty: 210, ctx_ok: 114, ctx_warn: 223,
    ctx_crit: 210, cache: 147, speed_ok: 116, speed_slow: 216, cost: 151, muted: 245,
    sep: 240, pill_bg: 237, pl_bg_a: 238, pl_bg_b: 236, pl_accent_fg: 232,
};

const PALETTE_TOKYO_NIGHT: Palette = Palette {
    ready: 114, thinking: 221, working: 111, tool: 141, review: 183, error: 204,
    model: 147, dir: 110, vcs_clean: 111, vcs_dirty: 204, ctx_ok: 114, ctx_warn: 221,
    ctx_crit: 204, cache: 117, speed_ok: 80, speed_slow: 215, cost: 150, muted: 244,
    sep: 239, pill_bg: 235, pl_bg_a: 236, pl_bg_b: 234, pl_accent_fg: 232,
};

const PALETTE_GRUVBOX: Palette = Palette {
    ready: 142, thinking: 214, working: 108, tool: 175, review: 175, error: 167,
    model: 175, dir: 109, vcs_clean: 109, vcs_dirty: 167, ctx_ok: 142, ctx_warn: 214,
    ctx_crit: 167, cache: 108, speed_ok: 142, speed_slow: 208, cost: 142, muted: 245,
    sep: 240, pill_bg: 236, pl_bg_a: 237, pl_bg_b: 235, pl_accent_fg: 234,
};

const PALETTE_LIGHT: Palette = Palette {
    ready: 28, thinking: 130, working: 25, tool: 90, review: 91, error: 160,
    model: 54, dir: 24, vcs_clean: 25, vcs_dirty: 160, ctx_ok: 28, ctx_warn: 130,
    ctx_crit: 160, cache: 31, speed_ok: 29, speed_slow: 166, cost: 28, muted: 242,
    sep: 248, pill_bg: 254, pl_bg_a: 254, pl_bg_b: 252, pl_accent_fg: 255,
};

/// Returns the static 256-color palette for `theme`.
///
/// `ThemeStyle::Mono` short-circuits ANSI color emission in the renderer, so it
/// falls back to [`PALETTE_DEFAULT`] rather than duplicating a dummy table.
pub const fn palette_for(theme: ThemeStyle) -> &'static Palette {
    match theme {
        ThemeStyle::Default | ThemeStyle::Mono => &PALETTE_DEFAULT,
        ThemeStyle::Nord => &PALETTE_NORD,
        ThemeStyle::Catppuccin => &PALETTE_CATPPUCCIN,
        ThemeStyle::TokyoNight => &PALETTE_TOKYO_NIGHT,
        ThemeStyle::Gruvbox => &PALETTE_GRUVBOX,
        ThemeStyle::Light => &PALETTE_LIGHT,
    }
}

impl Palette {
    /// Selects the 256-color palette matching `theme`.
    pub const fn for_theme(theme: ThemeStyle) -> Self {
        *palette_for(theme)
    }
}

/// Glyph set selected by [`GlyphMode`].
#[derive(Debug, Clone, Copy)]
pub struct GlyphSet {
    pub ready: &'static str,
    pub thinking: &'static str,
    pub working: &'static str,
    pub tool: &'static str,
    pub review: &'static str,
    pub error: &'static str,
    pub other: &'static str,
    pub dir: &'static str,
    pub branch: &'static str,
    pub cache_label: &'static str,
    pub reset: &'static str,
    pub speed: &'static str,
    pub ttft: &'static str,
    pub mode: &'static str,
    pub ellipsis: &'static str,
    pub bar_chars: [char; 5],
    pub pl_right: &'static str,
    pub pill_caps: Option<(&'static str, &'static str)>,
}

const GLYPHS_UNICODE: GlyphSet = GlyphSet {
    ready: "●",
    thinking: "◆",
    working: "⚙",
    tool: "✦",
    review: "◎",
    error: "✖",
    other: "◌",
    dir: "⌂",
    branch: "⎇",
    cache_label: "◈ cache",
    reset: "↻",
    speed: "⚡",
    ttft: "⏱",
    mode: "◇",
    ellipsis: "…",
    bar_chars: ['█', '▓', '▒', '░', '·'],
    pl_right: "\u{e0b0}",
    pill_caps: None,
};

const GLYPHS_NERD: GlyphSet = GlyphSet {
    ready: "\u{f111}",
    thinking: "\u{f0eb}",
    working: "\u{f085}",
    tool: "\u{f0ad}",
    review: "\u{f06e}",
    error: "\u{f057}",
    other: "\u{f254}",
    dir: "\u{f07c}",
    branch: "\u{e0a0}",
    cache_label: "\u{f1c0} cache",
    reset: "\u{f021}",
    speed: "\u{f0e7}",
    ttft: "\u{f017}",
    mode: "\u{f0e8}",
    ellipsis: "…",
    bar_chars: ['█', '▓', '▒', '░', '·'],
    pl_right: "\u{e0b0}",
    pill_caps: Some(("\u{e0b6}", "\u{e0b4}")),
};

const GLYPHS_ASCII: GlyphSet = GlyphSet {
    ready: "*",
    thinking: "~",
    working: ">",
    tool: "#",
    review: "@",
    error: "!",
    other: ".",
    dir: "dir:",
    branch: "git:",
    cache_label: "cache",
    reset: "r:",
    speed: "spd",
    ttft: "ttft",
    mode: "mode:",
    ellipsis: "..",
    bar_chars: ['#', '=', '-', '.', '.'],
    pl_right: ">",
    pill_caps: None,
};

/// Returns the static glyph table for `mode`.
pub const fn glyphs_for(mode: GlyphMode) -> &'static GlyphSet {
    match mode {
        GlyphMode::Unicode => &GLYPHS_UNICODE,
        GlyphMode::Nerd => &GLYPHS_NERD,
        GlyphMode::Ascii => &GLYPHS_ASCII,
    }
}

impl GlyphSet {
    /// Returns the glyph table for `mode`.
    pub const fn for_mode(mode: GlyphMode) -> Self {
        *glyphs_for(mode)
    }
}
