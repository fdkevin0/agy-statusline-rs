//! Token count formatting and ANSI statusline rendering.
//!
//! Formats cache hit rate, cache read token counts, and generation speed
//! into a single pre-allocated buffer with exact visible-width padding.

use std::fmt::Write as FmtWrite;
use std::io::{self, Write as IoWrite};

/// ANSI prefix for normal speed (`>= 50 token/s` or unknown): blue background, bright white text.
pub const COLOR_NORMAL: &str = "\x1b[44;97m";
/// ANSI prefix for slow speed (`< 50 token/s`): yellow background, black text.
pub const COLOR_SLOW: &str = "\x1b[43;30m";
/// ANSI reset suffix with trailing newline.
pub const ANSI_SUFFIX: &str = "\x1b[0m\n";

/// Maximum terminal width clamped during right-padding.
pub const MAX_TERMINAL_WIDTH: usize = 1000;

/// Token usage snapshot from AGY statusline JSON.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    /// Uncached input tokens.
    pub input_tokens: Option<i64>,
    /// Cache read input tokens.
    pub cache_read_input_tokens: Option<i64>,
    /// Cache creation input tokens.
    pub cache_creation_input_tokens: Option<i64>,
}

/// Rendered statusline separated into ANSI prefix and visible payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedStatusline {
    /// Leading ANSI color escape sequence.
    pub prefix: &'static str,
    /// Visible ASCII text payload (without ANSI escapes or newline).
    pub payload: String,
}

impl RenderedStatusline {
    /// Writes the colored statusline, right-padded with spaces to `terminal_width`.
    ///
    /// Widths above [`MAX_TERMINAL_WIDTH`] are clamped to `1000`.
    pub fn write_padded<W: IoWrite>(
        &self,
        writer: &mut W,
        terminal_width: Option<usize>,
    ) -> io::Result<()> {
        let width = terminal_width.unwrap_or(0).min(MAX_TERMINAL_WIDTH);
        let padding = width.saturating_sub(self.payload.len());
        writer.write_all(self.prefix.as_bytes())?;
        writer.write_all(self.payload.as_bytes())?;
        for _ in 0..padding {
            writer.write_all(b" ")?;
        }
        writer.write_all(ANSI_SUFFIX.as_bytes())
    }

    /// Returns the full ANSI-wrapped line (without terminal padding) as a `String`.
    pub fn to_ansi_line(&self) -> String {
        let mut out = String::with_capacity(self.prefix.len() + self.payload.len() + ANSI_SUFFIX.len());
        out.push_str(self.prefix);
        out.push_str(&self.payload);
        out.push_str(ANSI_SUFFIX);
        out
    }
}

/// Formats a non-negative token count into `out` with comma grouping and `K` suffix for `>= 1000`.
///
/// Uses a stack buffer to avoid intermediate heap allocations.
///
/// # Examples
///
/// ```
/// use agy_statusline::format::format_tokens;
///
/// let mut buf = String::new();
/// format_tokens(1_234_567, &mut buf);
/// assert_eq!(buf, "1,234K");
///
/// buf.clear();
/// format_tokens(800, &mut buf);
/// assert_eq!(buf, "800");
/// ```
pub fn format_tokens(value: u64, out: &mut String) {
    let is_k = value >= 1000;
    let n = if is_k { value / 1000 } else { value };

    // Format digits in reverse on the stack, inserting ',' every 3 digits.
    let mut stack = [0u8; 32];
    let mut idx = 0usize;
    let mut rem = n;
    let mut group = 0usize;
    loop {
        if group == 3 {
            stack[idx] = b',';
            idx += 1;
            group = 0;
        }
        stack[idx] = b'0' + (rem % 10) as u8;
        idx += 1;
        rem /= 10;
        group += 1;
        if rem == 0 {
            break;
        }
    }

    out.reserve(idx + usize::from(is_k));
    while idx > 0 {
        idx -= 1;
        out.push(stack[idx] as char);
    }
    if is_k {
        out.push('K');
    }
}

/// Renders the statusline for optional `usage` and `speed` into a [`RenderedStatusline`].
///
/// Displays `--` for cache hit rate when usage is missing, any token count is negative,
/// `cache_creation_input_tokens != 0`, or total input + cache read tokens is zero.
///
/// # Examples
///
/// ```
/// use agy_statusline::format::{render_statusline, TokenUsage};
///
/// let usage = TokenUsage {
///     input_tokens: Some(200),
///     cache_read_input_tokens: Some(800),
///     cache_creation_input_tokens: Some(0),
/// };
/// let rendered = render_statusline(Some(usage), Some(12.6));
/// assert_eq!(
///     rendered.to_ansi_line(),
///     "\x1b[43;30mCache hit: 80.0% | Cache read: 800 | Last avg speed: 13 token/s\x1b[0m\n"
/// );
/// ```
pub fn render_statusline(usage: Option<TokenUsage>, speed: Option<f64>) -> RenderedStatusline {
    let prefix = match speed {
        Some(s) if s < 50.0 => COLOR_SLOW,
        _ => COLOR_NORMAL,
    };

    let mut payload = String::with_capacity(80);
    if let Some(u) = usage {
        if let (Some(input), Some(read), Some(0)) = (
            u.input_tokens,
            u.cache_read_input_tokens,
            u.cache_creation_input_tokens,
        ) {
            if input >= 0 && read >= 0 {
                let total = (input as f64) + (read as f64);
                if total > 0.0 {
                    let hit_pct = 100.0 * (read as f64) / total;
                    let _ = write!(payload, "Cache hit: {hit_pct:.1}% | Cache read: ");
                    format_tokens(read as u64, &mut payload);
                    payload.push_str(" | ");
                    write_speed_text(speed, &mut payload);
                    return RenderedStatusline { prefix, payload };
                }
            }
        }
    }

    payload.push_str("Cache hit: -- | ");
    write_speed_text(speed, &mut payload);
    RenderedStatusline { prefix, payload }
}

fn write_speed_text(speed: Option<f64>, out: &mut String) {
    match speed {
        Some(s) => {
            let _ = write!(out, "Last avg speed: {s:.0} token/s");
        }
        None => out.push_str("Last avg speed: --"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_tokens_cases() {
        let mut s = String::new();
        format_tokens(0, &mut s);
        assert_eq!(s, "0");

        s.clear();
        format_tokens(999, &mut s);
        assert_eq!(s, "999");

        s.clear();
        format_tokens(1000, &mut s);
        assert_eq!(s, "1K");

        s.clear();
        format_tokens(1_234_567, &mut s);
        assert_eq!(s, "1,234K");
    }

    #[test]
    fn render_missing_and_nonzero_creation() {
        let r1 = render_statusline(None, None);
        assert_eq!(
            r1.to_ansi_line(),
            "\x1b[44;97mCache hit: -- | Last avg speed: --\x1b[0m\n"
        );

        let r2 = render_statusline(
            Some(TokenUsage {
                input_tokens: Some(100),
                cache_read_input_tokens: Some(200),
                cache_creation_input_tokens: Some(10),
            }),
            Some(75.2),
        );
        assert_eq!(
            r2.to_ansi_line(),
            "\x1b[44;97mCache hit: -- | Last avg speed: 75 token/s\x1b[0m\n"
        );
    }

    #[test]
    fn write_padded_respects_width_and_clamp() {
        let r = render_statusline(None, None);
        let mut out = Vec::new();
        r.write_padded(&mut out, Some(40)).unwrap();
        let text = String::from_utf8(out).unwrap();
        // payload is "Cache hit: -- | Last avg speed: --" (34 chars) -> 6 spaces
        assert!(text.ends_with("      \x1b[0m\n"));
    }
}
