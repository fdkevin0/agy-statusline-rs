//! Style geometry calculation and ANSI/mono terminal emission (`Plain`, `Pill`, `Powerline`).

use std::io::{self, Write as IoWrite};

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::config::layout::{GlyphMode, LayoutConfig, ThemeStyle, VisualStyle};
use crate::statusline::theme::GlyphSet;

use super::{Renderer, Segment};

#[derive(Debug, Clone, Copy)]
pub(super) struct StyleGeometry {
    pub(super) pad_per_seg: usize,
    pub(super) sep_w: usize,
    pub(super) slice_trail_w: usize,
    pub(super) mono: bool,
    pub(super) sep_str: &'static str,
}

impl StyleGeometry {
    pub(super) fn slice_visible_width(self, segments: &[Segment]) -> usize {
        if segments.is_empty() {
            return 0;
        }
        let body: usize = segments
            .iter()
            .enumerate()
            .map(|(i, s)| s.text.width() + self.pad_per_seg + if i > 0 { self.sep_w } else { 0 })
            .sum();
        body + self.slice_trail_w
    }
}

pub(super) fn compute_geometry(cfg: &LayoutConfig, glyphs: &GlyphSet) -> StyleGeometry {
    let mono = cfg.theme == ThemeStyle::Mono || cfg.no_color;
    let sep_str = if cfg.glyph_mode == GlyphMode::Ascii {
        cfg.separator.as_ascii_str()
    } else {
        cfg.separator.as_str()
    };
    match (mono, cfg.style) {
        (true, _) | (false, VisualStyle::Plain) => StyleGeometry {
            pad_per_seg: 0,
            sep_w: sep_str.width(),
            slice_trail_w: 0,
            mono,
            sep_str,
        },
        (false, VisualStyle::Pill) => {
            let pad = match glyphs.pill_caps {
                Some((l, r)) => l.width() + r.width(),
                None => 2,
            };
            StyleGeometry {
                pad_per_seg: pad,
                sep_w: 1,
                slice_trail_w: 0,
                mono: false,
                sep_str,
            }
        }
        (false, VisualStyle::Powerline) => {
            let arrow_w = glyphs.pl_right.width();
            StyleGeometry {
                pad_per_seg: 2,
                sep_w: arrow_w,
                slice_trail_w: arrow_w,
                mono: false,
                sep_str,
            }
        }
    }
}

impl Renderer<'_> {
    fn pill_colors(&self, seg: &Segment) -> (u8, u8, bool) {
        match seg.accent_bg {
            Some(abg) => (abg, self.pal.pl_accent_fg, true),
            None => (self.pal.pill_bg, seg.fg, seg.bold),
        }
    }

    fn powerline_colors(&self, idx: usize, seg: &Segment) -> (u8, u8, bool) {
        match seg.accent_bg {
            Some(accent) => (accent, self.pal.pl_accent_fg, true),
            None => (
                if idx.is_multiple_of(2) {
                    self.pal.pl_bg_a
                } else {
                    self.pal.pl_bg_b
                },
                seg.fg,
                seg.bold,
            ),
        }
    }

    fn two_group_gap(
        &self,
        left: &[Segment],
        right: &[Segment],
        term_width: Option<usize>,
    ) -> usize {
        let w = self.geom.slice_visible_width(left) + self.geom.slice_visible_width(right);
        term_width.unwrap_or(0).saturating_sub(w).max(1)
    }

    fn three_group_gaps(
        &self,
        left: &[Segment],
        center: &[Segment],
        right: &[Segment],
        term_width: Option<usize>,
    ) -> (usize, usize) {
        let w = self.geom.slice_visible_width(left)
            + self.geom.slice_visible_width(center)
            + self.geom.slice_visible_width(right);
        let rem = term_width.unwrap_or(0).saturating_sub(w);
        let gap1 = (rem / 2).max(1);
        let gap2 = rem.saturating_sub(gap1).max(1);
        (gap1, gap2)
    }

    pub(super) fn write_row_groups<W: IoWrite>(
        &self,
        groups: &[Vec<Segment>],
        term_width: Option<usize>,
        out: &mut W,
    ) -> io::Result<()> {
        match groups {
            [] => Ok(()),
            [only] => {
                self.write_slice(only, out)?;
                writeln!(out)
            }
            [left, right] => {
                let gap = self.two_group_gap(left, right, term_width);
                self.write_slice(left, out)?;
                write!(out, "{}", " ".repeat(gap))?;
                self.write_slice(right, out)?;
                writeln!(out)
            }
            [left, center, right, ..] => {
                let (gap1, gap2) = self.three_group_gaps(left, center, right, term_width);
                self.write_slice(left, out)?;
                write!(out, "{}", " ".repeat(gap1))?;
                self.write_slice(center, out)?;
                write!(out, "{}", " ".repeat(gap2))?;
                self.write_slice(right, out)?;
                writeln!(out)
            }
        }
    }

    fn write_slice<W: IoWrite>(&self, segments: &[Segment], out: &mut W) -> io::Result<()> {
        if self.geom.mono {
            for (i, seg) in segments.iter().enumerate() {
                if i > 0 {
                    write!(out, "{}", self.geom.sep_str)?;
                }
                write!(out, "{}", seg.text)?;
            }
            return Ok(());
        }

        let pal = self.pal;
        let glyphs = self.glyphs;

        match self.cfg.style {
            VisualStyle::Plain => {
                for (i, seg) in segments.iter().enumerate() {
                    if i > 0 {
                        write!(out, "\x1b[38;5;{}m{}\x1b[0m", pal.sep, self.geom.sep_str)?;
                    }
                    let bold = if seg.bold { "\x1b[1m" } else { "" };
                    write!(out, "{bold}\x1b[38;5;{}m{}\x1b[0m", seg.fg, seg.text)?;
                }
                Ok(())
            }
            VisualStyle::Pill => {
                for (i, seg) in segments.iter().enumerate() {
                    if i > 0 {
                        write!(out, " ")?;
                    }
                    let (bg, fg, bold) = self.pill_colors(seg);
                    let bold_s = if bold { "\x1b[1m" } else { "" };
                    if let Some((cap_l, cap_r)) = glyphs.pill_caps {
                        write!(
                            out,
                            "\x1b[38;5;{bg}m{cap_l}\x1b[48;5;{bg}m\x1b[38;5;{fg}m{bold_s}{}\x1b[0m\x1b[38;5;{bg}m{cap_r}\x1b[0m",
                            seg.text
                        )?;
                    } else {
                        write!(
                            out,
                            "\x1b[48;5;{bg}m\x1b[38;5;{fg}m{bold_s} {} \x1b[0m",
                            seg.text
                        )?;
                    }
                }
                Ok(())
            }
            VisualStyle::Powerline => {
                let mut prev_bg: Option<u8> = None;
                for (i, seg) in segments.iter().enumerate() {
                    let (bg, fg, bold) = self.powerline_colors(i, seg);
                    match prev_bg {
                        Some(p_bg) => {
                            write!(out, "\x1b[48;5;{bg}m\x1b[38;5;{p_bg}m{}", glyphs.pl_right)?;
                        }
                        None => write!(out, "\x1b[48;5;{bg}m")?,
                    }
                    let bold_s = if bold { "\x1b[1m" } else { "\x1b[22m" };
                    write!(out, "{bold_s}\x1b[38;5;{fg}m {} ", seg.text)?;
                    prev_bg = Some(bg);
                }
                if let Some(last_bg) = prev_bg {
                    write!(out, "\x1b[0m\x1b[38;5;{last_bg}m{}\x1b[0m", glyphs.pl_right)?;
                }
                Ok(())
            }
        }
    }

    pub(super) fn render_row_groups_line(
        &self,
        groups: &[Vec<Segment>],
        term_width: Option<usize>,
    ) -> Option<Line<'static>> {
        let mut spans = Vec::new();
        match groups {
            [] => return None,
            [only] => self.push_slice_spans(only, &mut spans),
            [left, right] => {
                let gap = self.two_group_gap(left, right, term_width);
                self.push_slice_spans(left, &mut spans);
                spans.push(Span::raw(" ".repeat(gap)));
                self.push_slice_spans(right, &mut spans);
            }
            [left, center, right, ..] => {
                let (gap1, gap2) = self.three_group_gaps(left, center, right, term_width);
                self.push_slice_spans(left, &mut spans);
                spans.push(Span::raw(" ".repeat(gap1)));
                self.push_slice_spans(center, &mut spans);
                spans.push(Span::raw(" ".repeat(gap2)));
                self.push_slice_spans(right, &mut spans);
            }
        }
        Some(Line::from(spans))
    }

    fn push_slice_spans(&self, segments: &[Segment], spans: &mut Vec<Span<'static>>) {
        let styled = |fg: u8, bg: Option<u8>, bold: bool| {
            let mut style = Style::default().fg(Color::Indexed(fg));
            if let Some(b) = bg {
                style = style.bg(Color::Indexed(b));
            }
            if bold {
                style = style.add_modifier(Modifier::BOLD);
            }
            style
        };

        if self.geom.mono {
            for (i, seg) in segments.iter().enumerate() {
                if i > 0 {
                    spans.push(Span::raw(self.geom.sep_str));
                }
                spans.push(Span::raw(seg.text.clone()));
            }
            return;
        }

        let pal = self.pal;
        let glyphs = self.glyphs;

        match self.cfg.style {
            VisualStyle::Plain => {
                for (i, seg) in segments.iter().enumerate() {
                    if i > 0 {
                        spans.push(Span::styled(
                            self.geom.sep_str,
                            Style::default().fg(Color::Indexed(pal.sep)),
                        ));
                    }
                    spans.push(Span::styled(
                        seg.text.clone(),
                        styled(seg.fg, None, seg.bold),
                    ));
                }
            }
            VisualStyle::Pill => {
                for (i, seg) in segments.iter().enumerate() {
                    if i > 0 {
                        spans.push(Span::raw(" "));
                    }
                    let (bg, fg, bold) = self.pill_colors(seg);
                    if let Some((cap_l, cap_r)) = glyphs.pill_caps {
                        spans.push(Span::styled(cap_l, Style::default().fg(Color::Indexed(bg))));
                        spans.push(Span::styled(seg.text.clone(), styled(fg, Some(bg), bold)));
                        spans.push(Span::styled(cap_r, Style::default().fg(Color::Indexed(bg))));
                    } else {
                        spans.push(Span::styled(
                            format!(" {} ", seg.text),
                            styled(fg, Some(bg), bold),
                        ));
                    }
                }
            }
            VisualStyle::Powerline => {
                let mut prev_bg: Option<u8> = None;
                for (i, seg) in segments.iter().enumerate() {
                    let (bg, fg, bold) = self.powerline_colors(i, seg);
                    if let Some(p_bg) = prev_bg {
                        spans.push(Span::styled(
                            glyphs.pl_right,
                            Style::default()
                                .fg(Color::Indexed(p_bg))
                                .bg(Color::Indexed(bg)),
                        ));
                    }
                    spans.push(Span::styled(
                        format!(" {} ", seg.text),
                        styled(fg, Some(bg), bold),
                    ));
                    prev_bg = Some(bg);
                }
                if let Some(last_bg) = prev_bg {
                    spans.push(Span::styled(
                        glyphs.pl_right,
                        Style::default().fg(Color::Indexed(last_bg)),
                    ));
                }
            }
        }
    }
}
