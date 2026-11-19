//! Converts ANSI-colored text to Ratatui lines via `ansi-to-tui` with a `strip-ansi-escapes` fallback.

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
