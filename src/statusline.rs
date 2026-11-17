//! Top-level statusline JSON parsing and pipeline orchestration.

use std::io::{self, Write};

use serde::Deserialize;

use crate::db::SpeedReader;
use crate::format::{render_statusline, TokenUsage};

/// Raw token usage payload nested under `context_window.current_usage`.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct JsonUsage {
    /// Uncached input tokens.
    pub input_tokens: Option<i64>,
    /// Cache read input tokens.
    pub cache_read_input_tokens: Option<i64>,
    /// Cache creation input tokens.
    pub cache_creation_input_tokens: Option<i64>,
}

impl From<JsonUsage> for TokenUsage {
    fn from(u: JsonUsage) -> Self {
        Self {
            input_tokens: u.input_tokens,
            cache_read_input_tokens: u.cache_read_input_tokens,
            cache_creation_input_tokens: u.cache_creation_input_tokens,
        }
    }
}

/// Raw `context_window` section of AGY statusline JSON.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct ContextWindow {
    /// Usage metrics for the most recent request.
    pub current_usage: Option<JsonUsage>,
}

/// Top-level AGY statusline JSON input.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct StatuslineState {
    /// Terminal column width for right-padding.
    pub terminal_width: Option<usize>,
    /// Active conversation UUID.
    #[serde(default)]
    pub conversation_id: String,
    /// Context window usage container.
    #[serde(default)]
    pub context_window: ContextWindow,
}

/// Parses `input` JSON (falling back gracefully on malformed/empty input),
/// queries generation speed via `reader`, and writes the padded ANSI line to `out`.
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
/// let json = r#"{"context_window":{"current_usage":{"input_tokens":200,"cache_read_input_tokens":800,"cache_creation_input_tokens":0}}}"#;
/// let mut out = Vec::new();
/// process_statusline(json, &FixedSpeed(Some(60.0)), &mut out).unwrap();
/// assert_eq!(
///     String::from_utf8(out).unwrap(),
///     "\x1b[44;97mCache hit: 80.0% | Cache read: 800 | Last avg speed: 60 token/s\x1b[0m\n"
/// );
/// ```
pub fn process_statusline<W: Write>(
    input: &str,
    reader: &dyn SpeedReader,
    out: &mut W,
) -> io::Result<()> {
    let state: Option<StatuslineState> = serde_json::from_str(input).ok();
    let usage = state
        .as_ref()
        .and_then(|s| s.context_window.current_usage)
        .map(TokenUsage::from);
    let speed = state
        .as_ref()
        .and_then(|s| reader.last_speed(&s.conversation_id));
    let width = state.as_ref().and_then(|s| s.terminal_width);
    let rendered = render_statusline(usage, speed);
    rendered.write_padded(out, width)
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
    fn empty_and_invalid_json_degrades_gracefully() {
        let mut out = Vec::new();
        process_statusline("", &StubReader(None), &mut out).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "\x1b[44;97mCache hit: -- | Last avg speed: --\x1b[0m\n"
        );

        let mut out2 = Vec::new();
        process_statusline("{bad json", &StubReader(Some(100.0)), &mut out2).unwrap();
        assert_eq!(
            String::from_utf8(out2).unwrap(),
            "\x1b[44;97mCache hit: -- | Last avg speed: --\x1b[0m\n"
        );
    }
}
