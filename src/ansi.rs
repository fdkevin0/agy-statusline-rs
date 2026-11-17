//! UTF-8 safe ANSI SGR escape parser for rendering true-color live previews in Ratatui.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

const STANDARD_FG: [Color; 8] = [
    Color::Black,
    Color::Red,
    Color::Green,
    Color::Yellow,
    Color::Blue,
    Color::Magenta,
    Color::Cyan,
    Color::Gray,
];

const BRIGHT_FG: [Color; 8] = [
    Color::DarkGray,
    Color::LightRed,
    Color::LightGreen,
    Color::LightYellow,
    Color::LightBlue,
    Color::LightMagenta,
    Color::LightCyan,
    Color::White,
];

fn apply_sgr(style: &mut Style, params: &str) {
    if params.is_empty() {
        *style = Style::default();
        return;
    }
    let codes: Vec<u16> = params
        .split(';')
        .filter_map(|p| {
            if p.is_empty() {
                Some(0)
            } else {
                p.parse::<u16>().ok()
            }
        })
        .collect();

    let mut i = 0;
    while i < codes.len() {
        match codes[i] {
            0 => *style = Style::default(),
            1 => *style = style.add_modifier(Modifier::BOLD),
            2 => *style = style.add_modifier(Modifier::DIM),
            3 => *style = style.add_modifier(Modifier::ITALIC),
            22 => *style = style.remove_modifier(Modifier::BOLD | Modifier::DIM),
            23 => *style = style.remove_modifier(Modifier::ITALIC),
            30..=37 => *style = style.fg(STANDARD_FG[(codes[i] - 30) as usize]),
            39 => *style = style.fg(Color::Reset),
            40..=47 => *style = style.bg(STANDARD_FG[(codes[i] - 40) as usize]),
            49 => *style = style.bg(Color::Reset),
            90..=97 => *style = style.fg(BRIGHT_FG[(codes[i] - 90) as usize]),
            100..=107 => *style = style.bg(BRIGHT_FG[(codes[i] - 100) as usize]),
            38 if i + 2 < codes.len() && codes[i + 1] == 5 => {
                *style = style.fg(Color::Indexed(codes[i + 2].min(255) as u8));
                i += 2;
            }
            48 if i + 2 < codes.len() && codes[i + 1] == 5 => {
                *style = style.bg(Color::Indexed(codes[i + 2].min(255) as u8));
                i += 2;
            }
            _ => {}
        }
        i += 1;
    }
}

/// Converts ANSI-colored UTF-8 lines into styled [`ratatui::text::Line`] values.
pub fn ansi_to_ratatui_lines(input: &str) -> Vec<Line<'static>> {
    input
        .lines()
        .map(|line| {
            let mut spans = Vec::new();
            let mut style = Style::default();
            let mut pos = 0;
            let bytes = line.as_bytes();

            while pos < bytes.len() {
                if let Some(rel_esc) = line[pos..].find("\x1b[") {
                    if rel_esc > 0 {
                        spans.push(Span::styled(
                            line[pos..pos + rel_esc].to_string(),
                            style,
                        ));
                    }
                    let seq_start = pos + rel_esc + 2;
                    if let Some(rel_end) = line[seq_start..].find(|c: char| c.is_ascii_alphabetic())
                    {
                        let cmd = line.as_bytes()[seq_start + rel_end];
                        if cmd == b'm' {
                            apply_sgr(&mut style, &line[seq_start..seq_start + rel_end]);
                        }
                        pos = seq_start + rel_end + 1;
                    } else {
                        break;
                    }
                } else {
                    spans.push(Span::styled(line[pos..].to_string(), style));
                    break;
                }
            }
            Line::from(spans)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(lines[0].spans[1].style.bg, None);
    }

    #[test]
    fn preserves_plain_utf8_progress_blocks() {
        let raw = "ctx ███████▒······· 50.0%\n";
        let lines = ansi_to_ratatui_lines(raw);
        assert_eq!(lines[0].spans[0].content, "ctx ███████▒······· 50.0%");
    }
}
