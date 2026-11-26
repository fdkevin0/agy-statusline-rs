//! Shared low-level formatting, width truncation, duration, and single-line text sanitization.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::time::{SECS_PER_DAY, SECS_PER_HOUR};

fn push_tenths_with_suffix(tenths: u64, suffix: char, out: &mut String) {
    if tenths == 100 {
        out.reserve(3);
        out.push_str("10");
        out.push(suffix);
        return;
    }
    let whole = (tenths / 10) as u8;
    let frac = (tenths % 10) as u8;
    out.reserve(4);
    out.push((b'0' + whole) as char);
    if frac != 0 {
        out.push('.');
        out.push((b'0' + frac) as char);
    }
    out.push(suffix);
}

/// Formats a non-negative token count into `out` with comma grouping and `K` (`1,000..999,500`),
/// `M` (`999,500..999,500,000`), or `B` (`>= 999,500,000`) suffix using symmetric boundary
/// promotion (`999_500 -> 1M`, `999_500_000 -> 1B`).
///
/// Uses a stack buffer to avoid intermediate heap allocations.
///
/// # Examples
///
/// ```
/// use agy_statusline::statusline::format::format_tokens;
///
/// let mut buf = String::new();
/// format_tokens(1_338_000_000, &mut buf);
/// assert_eq!(buf, "1.3B");
///
/// buf.clear();
/// format_tokens(1_234_567, &mut buf);
/// assert_eq!(buf, "1.2M");
///
/// buf.clear();
/// format_tokens(1_048_576, &mut buf);
/// assert_eq!(buf, "1M");
///
/// buf.clear();
/// format_tokens(1_500, &mut buf);
/// assert_eq!(buf, "1.5K");
///
/// buf.clear();
/// format_tokens(512_000, &mut buf);
/// assert_eq!(buf, "512K");
///
/// buf.clear();
/// format_tokens(800, &mut buf);
/// assert_eq!(buf, "800");
/// ```
pub fn format_tokens(value: u64, out: &mut String) {
    if (999_500_000..10_000_000_000).contains(&value) {
        push_tenths_with_suffix((value + 50_000_000) / 100_000_000, 'B', out);
        return;
    }
    if (999_500..10_000_000).contains(&value) {
        push_tenths_with_suffix((value + 50_000) / 100_000, 'M', out);
        return;
    }
    if (1_000..10_000).contains(&value) {
        push_tenths_with_suffix((value + 50) / 100, 'K', out);
        return;
    }

    let (n, suffix) = if value >= 10_000_000_000 {
        (value.saturating_add(500_000_000) / 1_000_000_000, Some('B'))
    } else if value >= 10_000_000 {
        (value.saturating_add(500_000) / 1_000_000, Some('M'))
    } else if value >= 10_000 {
        ((value + 500) / 1_000, Some('K'))
    } else {
        (value, None)
    };

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

    out.reserve(idx + usize::from(suffix.is_some()));
    while idx > 0 {
        idx -= 1;
        out.push(stack[idx] as char);
    }
    if let Some(ch) = suffix {
        out.push(ch);
    }
}

/// Formats a token count into a newly allocated compact string.
pub(crate) fn format_tokens_string(value: u64) -> String {
    let mut buf = String::with_capacity(8);
    format_tokens(value, &mut buf);
    buf
}

/// Formats a duration in seconds into a compact human-readable string (`45s`, `12m`, `3h10m`, `5d12h`).
pub(crate) fn format_duration_compact(secs: u64) -> String {
    let day = SECS_PER_DAY as u64;
    if secs < 60 {
        return format!("{secs}s");
    }
    if secs < SECS_PER_HOUR {
        return format!("{}m", secs / 60);
    }
    if secs < day {
        let h = secs / SECS_PER_HOUR;
        let m = (secs % SECS_PER_HOUR) / 60;
        return if m == 0 {
            format!("{h}h")
        } else {
            format!("{h}h{m}m")
        };
    }
    let d = secs / day;
    let h = (secs % day) / SECS_PER_HOUR;
    if h == 0 {
        format!("{d}d")
    } else {
        format!("{d}d{h}h")
    }
}

/// Truncates `s` to at most `max_cols` terminal display columns, appending `ellipsis` when truncated.
pub(crate) fn truncate_to_width(s: &str, max_cols: usize, ellipsis: &str) -> String {
    if s.width() <= max_cols {
        return s.to_string();
    }
    let ell_w = ellipsis.width();
    if max_cols == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0usize;
    let budget = if max_cols > ell_w {
        max_cols - ell_w
    } else {
        max_cols
    };
    for ch in s.chars() {
        let w = ch.width().unwrap_or(0);
        if used + w > budget {
            break;
        }
        out.push(ch);
        used += w;
    }
    if max_cols > ell_w {
        out.push_str(ellipsis);
    }
    out
}

/// Returns `raw` trimmed when non-empty.
pub(crate) fn trimmed_non_empty(raw: Option<&str>) -> Option<&str> {
    raw.map(str::trim).filter(|s| !s.is_empty())
}

/// Returns `true` when both optional strings are non-empty after trimming and differ ASCII case-insensitively.
pub(crate) fn ascii_values_conflict(a: Option<&str>, b: Option<&str>) -> bool {
    matches!(
        (trimmed_non_empty(a), trimmed_non_empty(b)),
        (Some(lhs), Some(rhs)) if !lhs.eq_ignore_ascii_case(rhs)
    )
}

/// Splits `raw` on non-ASCII-alphanumeric boundaries into lowercase non-empty tokens.
pub(crate) fn tokenize_alnum_lower(raw: &str) -> Vec<String> {
    raw.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|tok| !tok.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

/// Strips ANSI escapes and control characters, collapsing runs of whitespace into single spaces.
pub(crate) fn sanitize_inline_text(raw: &str) -> String {
    // Normalize non-CR/LF whitespace (such as `\t` and NEL) before `strip_ansi_escapes`,
    // whose VTE parser drops C0/C1 control characters other than `\n` and `\r`.
    let normalized: String = raw
        .chars()
        .map(|ch| {
            if ch.is_whitespace() && !matches!(ch, ' ' | '\n' | '\r') {
                ' '
            } else {
                ch
            }
        })
        .collect();
    let stripped = strip_ansi_escapes::strip_str(&normalized);
    let mut out = String::with_capacity(stripped.len());
    let mut pending_space = false;

    for ch in stripped.chars() {
        if ch.is_whitespace() {
            if !out.is_empty() {
                pending_space = true;
            }
        } else if !ch.is_control() {
            if pending_space {
                out.push(' ');
                pending_space = false;
            }
            out.push(ch);
        }
    }

    out
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
        format_tokens(1_000, &mut s);
        assert_eq!(s, "1K");

        s.clear();
        format_tokens(1_500, &mut s);
        assert_eq!(s, "1.5K");

        s.clear();
        format_tokens(1_950, &mut s);
        assert_eq!(s, "2K");

        s.clear();
        format_tokens(9_950, &mut s);
        assert_eq!(s, "10K");

        s.clear();
        format_tokens(10_499, &mut s);
        assert_eq!(s, "10K");

        s.clear();
        format_tokens(10_500, &mut s);
        assert_eq!(s, "11K");

        s.clear();
        format_tokens(512_000, &mut s);
        assert_eq!(s, "512K");

        s.clear();
        format_tokens(999_499, &mut s);
        assert_eq!(s, "999K");

        s.clear();
        format_tokens(999_500, &mut s);
        assert_eq!(s, "1M");

        s.clear();
        format_tokens(999_999, &mut s);
        assert_eq!(s, "1M");

        s.clear();
        format_tokens(1_000_000, &mut s);
        assert_eq!(s, "1M");

        s.clear();
        format_tokens(1_048_576, &mut s);
        assert_eq!(s, "1M");

        s.clear();
        format_tokens(1_234_567, &mut s);
        assert_eq!(s, "1.2M");

        s.clear();
        format_tokens(9_950_000, &mut s);
        assert_eq!(s, "10M");

        s.clear();
        format_tokens(2_000_000, &mut s);
        assert_eq!(s, "2M");

        s.clear();
        format_tokens(12_345_678, &mut s);
        assert_eq!(s, "12M");

        s.clear();
        format_tokens(999_499_999, &mut s);
        assert_eq!(s, "999M");

        s.clear();
        format_tokens(999_500_000, &mut s);
        assert_eq!(s, "1B");

        s.clear();
        format_tokens(999_999_999, &mut s);
        assert_eq!(s, "1B");

        s.clear();
        format_tokens(1_000_000_000, &mut s);
        assert_eq!(s, "1B");

        s.clear();
        format_tokens(1_338_000_000, &mut s);
        assert_eq!(s, "1.3B");

        s.clear();
        format_tokens(2_550_000_000, &mut s);
        assert_eq!(s, "2.6B");

        s.clear();
        format_tokens(9_950_000_000, &mut s);
        assert_eq!(s, "10B");

        s.clear();
        format_tokens(10_000_000_000, &mut s);
        assert_eq!(s, "10B");

        s.clear();
        format_tokens(1_234_500_000_000, &mut s);
        assert_eq!(s, "1,235B");
    }
}
