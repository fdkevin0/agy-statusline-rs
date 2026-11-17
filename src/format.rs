//! Stack-allocated token count formatting utilities.

/// Formats a non-negative token count into `out` with comma grouping and `K` (`>= 1,000`)
/// or `M` (`>= 1,000,000`) suffix.
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
/// assert_eq!(buf, "1.2M");
///
/// buf.clear();
/// format_tokens(1_048_576, &mut buf);
/// assert_eq!(buf, "1M");
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
    if (1_000_000..10_000_000).contains(&value) {
        let tenths = (value + 50_000) / 100_000;
        if tenths == 100 {
            out.push_str("10M");
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
        out.push('M');
        return;
    }

    let (n, suffix) = if value >= 10_000_000 {
        (value.saturating_add(500_000) / 1_000_000, Some('M'))
    } else if value >= 1_000 {
        (value / 1_000, Some('K'))
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
        format_tokens(512_000, &mut s);
        assert_eq!(s, "512K");

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
    }
}
