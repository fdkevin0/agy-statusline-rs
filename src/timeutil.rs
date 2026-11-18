//! Shared Gregorian calendar math and overflow-safe RFC3339 timestamp parsing.

const SECS_PER_DAY: i64 = 86_400;

/// Returns `true` if `year` is a proleptic Gregorian leap year.
pub fn is_leap_year(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

/// Returns the number of days in `month` (`1..=12`) of `year`, or `0` for invalid months.
pub fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Converts a proleptic Gregorian civil date to days since `1970-01-01` (Howard Hinnant algorithm).
pub fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = i64::from(month);
    let d = i64::from(day);
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Converts days since `1970-01-01` back into `(year, month, day)`.
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let year = y + i64::from(m <= 2);
    (year, m as u32, d as u32)
}

pub(crate) fn parse_ascii_digits<T: std::str::FromStr>(bytes: &[u8]) -> Option<T> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(bytes).ok()?.parse::<T>().ok()
}

/// Parses an RFC3339 / ISO-8601 timestamp (`YYYY-MM-DDTHH:MM:SS[.fff](Z|+HH:MM|-HH:MM)`)
/// into Unix epoch seconds, clamping leap second `:60` to `:59`.
pub fn parse_rfc3339_unix_secs(raw: &str) -> Option<i64> {
    let b = raw.trim().as_bytes();
    if b.len() < 20
        || b[4] != b'-'
        || b[7] != b'-'
        || !matches!(b[10], b'T' | b't' | b' ')
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }

    let year: i64 = parse_ascii_digits(&b[0..4])?;
    let month: u32 = parse_ascii_digits(&b[5..7])?;
    let day: u32 = parse_ascii_digits(&b[8..10])?;
    let hour: i64 = parse_ascii_digits(&b[11..13])?;
    let minute: i64 = parse_ascii_digits(&b[14..16])?;
    let second: i64 = parse_ascii_digits(&b[17..19])?;

    if !(1..=12).contains(&month)
        || day == 0
        || day > days_in_month(year, month)
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || !(0..=60).contains(&second)
    {
        return None;
    }

    let mut idx = 19;
    if idx < b.len() && b[idx] == b'.' {
        idx += 1;
        let frac_start = idx;
        while idx < b.len() && b[idx].is_ascii_digit() {
            idx += 1;
        }
        if idx == frac_start {
            return None;
        }
    }

    let tz = b.get(idx..)?;
    let offset_secs = match tz {
        [b'Z' | b'z'] => 0i64,
        [sign @ (b'+' | b'-'), h1, h2, b':', m1, m2] | [sign @ (b'+' | b'-'), h1, h2, m1, m2] => {
            let tz_h: i64 = parse_ascii_digits(&[*h1, *h2])?;
            let tz_m: i64 = parse_ascii_digits(&[*m1, *m2])?;
            if !(0..=23).contains(&tz_h) || !(0..=59).contains(&tz_m) {
                return None;
            }
            let mag = tz_h * 3_600 + tz_m * 60;
            if *sign == b'-' {
                -mag
            } else {
                mag
            }
        }
        _ => return None,
    };

    let days = days_from_civil(year, month, day);
    let clamped_sec = second.min(59);
    let local_secs = days
        .checked_mul(SECS_PER_DAY)?
        .checked_add(hour * 3_600 + minute * 60 + clamped_sec)?;
    local_secs.checked_sub(offset_secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendar_roundtrip_and_leap_years() {
        assert!(is_leap_year(2000));
        assert!(is_leap_year(2024));
        assert!(!is_leap_year(1900));
        assert!(!is_leap_year(2026));
        assert_eq!(days_in_month(2024, 2), 29);
        assert_eq!(days_in_month(2026, 2), 28);
        assert_eq!(days_in_month(2026, 13), 0);

        let d0 = days_from_civil(1970, 1, 1);
        assert_eq!(d0, 0);
        assert_eq!(civil_from_days(d0), (1970, 1, 1));

        let d_leap = days_from_civil(2024, 2, 29);
        assert_eq!(civil_from_days(d_leap), (2024, 2, 29));
    }

    #[test]
    fn rfc3339_clamps_leap_second_and_rejects_trailing_garbage() {
        assert_eq!(
            parse_rfc3339_unix_secs("2026-11-18T23:59:60Z"),
            parse_rfc3339_unix_secs("2026-11-18T23:59:59Z")
        );
        assert_eq!(parse_rfc3339_unix_secs("2026-11-18T12:00:00Zextra"), None);
        assert_eq!(parse_rfc3339_unix_secs("2026-11-18T12:00:00.Z"), None);
        assert_eq!(parse_rfc3339_unix_secs("2026-02-29T00:00:00Z"), None);
    }
}
