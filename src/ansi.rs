//! UTF-8 safe ANSI SGR escape parser for rendering true-color live previews in Ratatui.

use ansi_to_tui::IntoText;
use ratatui::text::Line;

/// Converts ANSI-colored UTF-8 lines into styled [`ratatui::text::Line`] values.
pub fn ansi_to_ratatui_lines(input: &str) -> Vec<Line<'static>> {
    match input.into_text() {
        Ok(text) => text.lines,
        Err(_) => strip_ansi_escapes::strip_str(input)
            .lines()
            .map(|line| Line::from(line.to_owned()))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Modifier};

    #[test]
    fn parses_256_colors_bold_reset_and_utf8_glyphs() {
        let raw = "\x1b[48;5;78m\x1b[38;5;232m\x1b[1m ● READY \x1b[0m\x1b[38;5;78m\u{e0b0}\x1b[0m\n";
        let lines = ansi_to_ratatui_lines(raw);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans.len(), 2);
        assert_eq!(lines[0].spans[0].content, " ● READY ");
        assert_eq!(lines[0].spans[0].style.bg, Some(Color::Indexed(78)));
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Indexed(232)));
        assert!(lines[0].spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(lines[0].spans[1].content, "\u{e0b0}");
        assert_eq!(lines[0].spans[1].style.fg, Some(Color::Indexed(78)));
        assert_eq!(lines[0].spans[1].style.bg, Some(Color::Reset));
    }

    #[test]
    fn preserves_plain_utf8_progress_blocks() {
        let raw = "ctx ███████▒······· 50.0%\n";
        let lines = ansi_to_ratatui_lines(raw);
        assert_eq!(lines[0].spans[0].content, "ctx ███████▒······· 50.0%");
    }

    #[test]
    fn parses_truecolor_24bit_rgb_sgr() {
        let raw = "\x1b[38;2;12;34;56m\x1b[48;2;200;150;100mHi\x1b[0m\n";
        let lines = ansi_to_ratatui_lines(raw);
        assert_eq!(lines[0].spans[0].content, "Hi");
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Rgb(12, 34, 56)));
        assert_eq!(lines[0].spans[0].style.bg, Some(Color::Rgb(200, 150, 100)));
    }

    #[test]
    fn preserves_multiline_sgr_and_empty_lines() {
        let raw = "\x1b[32mline1\x1b[0m\n\n\x1b[33mline3\x1b[0m\n";
        let lines = ansi_to_ratatui_lines(raw);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].spans[0].content, "line1");
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Green));
        assert!(lines[1].spans.is_empty());
        assert_eq!(lines[2].spans[0].content, "line3");
        assert_eq!(lines[2].spans[0].style.fg, Some(Color::Yellow));
    }

    #[test]
    fn skips_non_sgr_csi_sequences_cleanly() {
        let raw = "\x1b[2K\x1b[32mok\x1b[2 q\x1b[0m done";
        let lines = ansi_to_ratatui_lines(raw);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans.len(), 2);
        assert_eq!(lines[0].spans[0].content, "ok");
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Green));
        assert_eq!(lines[0].spans[1].content, " done");
        assert_eq!(lines[0].spans[1].style.fg, Some(Color::Reset));
    }

    #[test]
    fn handles_malformed_or_truncated_csi_gracefully() {
        let lines = ansi_to_ratatui_lines("pre\x1b[31\x1b[32mok\x1b[0m");
        assert_eq!(lines.len(), 1);
        let combined: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(combined, "preok");

        let lines_utf8 = ansi_to_ratatui_lines("\x1b[31● hello");
        assert_eq!(lines_utf8.len(), 1);
        let combined_utf8: String = lines_utf8[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(!combined_utf8.contains('\x1b'));
        assert!(!combined_utf8.is_empty());

        let lines_tab = ansi_to_ratatui_lines("\x1b[31\thello");
        assert_eq!(lines_tab.len(), 1);
        let combined_tab: String = lines_tab[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(!combined_tab.contains('\x1b'));
        assert!(!combined_tab.is_empty());

        let lines_trunc = ansi_to_ratatui_lines("hi\x1b[31");
        assert_eq!(lines_trunc.len(), 1);
        let combined_trunc: String = lines_trunc[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(combined_trunc, "hi");
    }
}
