//! Stack-allocated token count formatting utilities.

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
}
