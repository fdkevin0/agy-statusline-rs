//! Stack-allocated token count formatting utilities.

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
    if (1_000_000..10_000_000).contains(&value) {
        push_tenths_with_suffix((value + 50_000) / 100_000, 'M', out);
        return;
    }
    if (1_000..10_000).contains(&value) {
        push_tenths_with_suffix((value + 50) / 100, 'K', out);
        return;
    }

    let (n, suffix) = if value >= 10_000_000 {
        (value.saturating_add(500_000) / 1_000_000, Some('M'))
    } else if value >= 10_000 {
        (((value + 500) / 1_000).min(999), Some('K'))
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
        format_tokens(999_999, &mut s);
        assert_eq!(s, "999K");

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
