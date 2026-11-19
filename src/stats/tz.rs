//! Local timezone resolution backed by `jiff` and civil date helpers.

use std::fs;

use jiff::tz::{Offset, TimeZone};
use jiff::Timestamp;

pub use crate::timeutil::parse_rfc3339_unix_secs;
use crate::timeutil::{
    civil_from_days, days_from_civil, days_in_month, parse_ascii_digits,
};

const SECS_PER_DAY: i64 = 86_400;

pub(super) fn format_day(day_idx: i64) -> String {
    let (y, m, d) = civil_from_days(day_idx);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Formats a Unix epoch timestamp (seconds) as a UTC `YYYY-MM-DD` date string.
pub fn unix_secs_to_utc_date(unix_secs: u64) -> String {
    format_day((unix_secs / 86_400) as i64)
}

/// Timezone specification used to map UTC timestamps to local calendar days.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimeZoneSpec {
    inner: TimeZone,
}

impl Default for TimeZoneSpec {
    fn default() -> Self {
        Self::utc()
    }
}

impl TimeZoneSpec {
    /// Returns a UTC (`+00:00`) timezone specification.
    pub fn utc() -> Self {
        Self {
            inner: TimeZone::UTC,
        }
    }

    /// Returns a fixed-offset timezone specification (`offset_secs` east of UTC, e.g. `+28_800` for `UTC+08:00`).
    pub fn fixed_offset(offset_secs: i32) -> Self {
        let clamped = offset_secs.clamp(-93_599, 93_599);
        let offset = Offset::from_seconds(clamped).unwrap_or(Offset::UTC);
        Self {
            inner: TimeZone::fixed(offset),
        }
    }

    /// Parses an RFC 8536 TZif v1/v2/v3/v4 binary blob (e.g. `/etc/localtime` or `/usr/share/zoneinfo/*`).
    pub fn from_tzif(bytes: &[u8]) -> Option<Self> {
        TimeZone::tzif("tzif", bytes)
            .ok()
            .map(|inner| Self { inner })
    }

    /// Loads the host local timezone via `jiff::tz::TimeZone::system()`, falling back to UTC.
    pub fn local() -> Self {
        Self {
            inner: TimeZone::system(),
        }
    }

    /// Resolves a `TZ` environment variable string (zoneinfo path/name, ISO offset, or POSIX TZ string).
    pub fn from_tz_env(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        let val = trimmed.strip_prefix(':').unwrap_or(trimmed);
        if val.is_empty() {
            return None;
        }
        if val.starts_with('/') {
            let bytes = fs::read(val).ok()?;
            return Self::from_tzif(&bytes);
        }
        if let Some(offset) = parse_iso_offset_secs(val) {
            return Some(Self::fixed_offset(offset));
        }
        if let Ok(inner) = TimeZone::get(val) {
            return Some(Self { inner });
        }
        TimeZone::posix(val).ok().map(|inner| Self { inner })
    }

    /// Returns the UTC offset in seconds at `unix_secs` (positive east of UTC).
    pub fn offset_secs_at(&self, unix_secs: i64) -> i32 {
        Timestamp::from_second(unix_secs)
            .map(|ts| self.inner.to_offset(ts).seconds())
            .unwrap_or(0)
    }

    /// Formats the timezone offset at `unix_secs` as `UTC`, `UTC+HH:MM`, or `UTC-HH:MM`.
    pub fn format_offset_at(&self, unix_secs: i64) -> String {
        let offset = self.offset_secs_at(unix_secs);
        if offset == 0 {
            return "UTC".to_string();
        }
        let sign = if offset < 0 { '-' } else { '+' };
        let abs = offset.unsigned_abs();
        let hours = abs / 3_600;
        let mins = (abs % 3_600) / 60;
        let secs = abs % 60;
        if secs == 0 {
            format!("UTC{sign}{hours:02}:{mins:02}")
        } else {
            format!("UTC{sign}{hours:02}:{mins:02}:{secs:02}")
        }
    }

    /// Maps a UTC Unix timestamp (seconds) into `(local_day_index, "YYYY-MM-DD")`.
    pub fn local_day_and_date(&self, unix_secs: i64) -> (i64, String) {
        let local_secs = unix_secs.saturating_add(i64::from(self.offset_secs_at(unix_secs)));
        let day_idx = local_secs.div_euclid(SECS_PER_DAY);
        (day_idx, format_day(day_idx))
    }
}

fn parse_iso_offset_secs(raw: &str) -> Option<i32> {
    if raw.eq_ignore_ascii_case("Z") || raw.eq_ignore_ascii_case("UTC") {
        return Some(0);
    }
    let b = raw.as_bytes();
    match b {
        [sign @ (b'+' | b'-'), h1, h2, b':', m1, m2] | [sign @ (b'+' | b'-'), h1, h2, m1, m2] => {
            let h: i32 = parse_ascii_digits(&[*h1, *h2])?;
            let m: i32 = parse_ascii_digits(&[*m1, *m2])?;
            if !(0..=23).contains(&h) || !(0..=59).contains(&m) {
                return None;
            }
            let mag = h * 3_600 + m * 60;
            Some(if *sign == b'-' { -mag } else { mag })
        }
        _ => None,
    }
}

/// Parses an RFC3339 timestamp into `(utc_day_index, "YYYY-MM-DD")`.
pub fn parse_rfc3339_utc_day(raw: &str) -> Option<(i64, String)> {
    let utc_secs = parse_rfc3339_unix_secs(raw)?;
    let utc_day = utc_secs.div_euclid(SECS_PER_DAY);
    Some((utc_day, format_day(utc_day)))
}

pub(super) fn parse_civil_date_day(raw: &str) -> Option<i64> {
    let b = raw.trim().as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let year: i64 = parse_ascii_digits(&b[0..4])?;
    let month: u32 = parse_ascii_digits(&b[5..7])?;
    let day: u32 = parse_ascii_digits(&b[8..10])?;
    if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
        return None;
    }
    Some(days_from_civil(year, month, day))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_tzif_v2(offset_secs: i32, footer: &str) -> Vec<u8> {
        let mut out = Vec::new();
        // v1 header: timecnt=0, typecnt=1, charcnt=4
        out.extend_from_slice(b"TZif2");
        out.extend_from_slice(&[0u8; 15]);
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(&1u32.to_be_bytes());
        out.extend_from_slice(&4u32.to_be_bytes());
        // v1 ttinfo (6 bytes) + abbr (4 bytes)
        out.extend_from_slice(&offset_secs.to_be_bytes());
        out.push(0);
        out.push(0);
        out.extend_from_slice(b"CST\0");
        // v2 header: timecnt=1, typecnt=1, charcnt=4
        out.extend_from_slice(b"TZif2");
        out.extend_from_slice(&[0u8; 15]);
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(&1u32.to_be_bytes());
        out.extend_from_slice(&1u32.to_be_bytes());
        out.extend_from_slice(&4u32.to_be_bytes());
        // v2 transition time (8 bytes), type_idx (1 byte), ttinfo (6 bytes), abbr (4 bytes)
        out.extend_from_slice(&0i64.to_be_bytes());
        out.push(0);
        out.extend_from_slice(&offset_secs.to_be_bytes());
        out.push(0);
        out.push(0);
        out.extend_from_slice(b"CST\0");
        out.push(b'\n');
        out.extend_from_slice(footer.as_bytes());
        out.push(b'\n');
        out
    }

    #[test]
    fn rfc3339_utc_day_handles_z_and_timezone_rollover() {
        let (d1, s1) = parse_rfc3339_utc_day("2026-11-18T03:05:35Z").unwrap();
        assert_eq!(s1, "2026-11-18");

        let (d2, s2) = parse_rfc3339_utc_day("2026-11-17T23:30:00-05:00").unwrap();
        assert_eq!(s2, "2026-11-18");
        assert_eq!(d1, d2);

        let (d3, s3) = parse_rfc3339_utc_day("2026-11-18T01:00:00+08:00").unwrap();
        assert_eq!(s3, "2026-11-17");
        assert_eq!(d3, d1 - 1);

        assert_eq!(parse_rfc3339_utc_day("not-a-timestamp"), None);
        assert_eq!(parse_rfc3339_utc_day("2026-02-29T00:00:00Z"), None);
    }

    #[test]
    fn tzif_and_posix_dst_timezone_bucketing() {
        let shanghai_bytes = build_tzif_v2(8 * 3_600, "CST-8");
        let shanghai = TimeZoneSpec::from_tzif(&shanghai_bytes).unwrap();
        // 2026-11-17T20:00:00Z is 2026-11-18 04:00:00 in UTC+08:00
        let ts_utc_17 = parse_rfc3339_unix_secs("2026-11-17T20:00:00Z").unwrap();
        assert_eq!(shanghai.format_offset_at(ts_utc_17), "UTC+08:00");
        assert_eq!(shanghai.local_day_and_date(ts_utc_17).1, "2026-11-18");

        // US Eastern POSIX rule with DST: EST5EDT,M3.2.0,M11.1.0
        let ny = TimeZoneSpec::from_tz_env("EST5EDT,M3.2.0,M11.1.0").unwrap();
        let summer = parse_rfc3339_unix_secs("2026-07-01T12:00:00Z").unwrap();
        let winter = parse_rfc3339_unix_secs("2026-11-17T12:00:00Z").unwrap();
        assert_eq!(ny.format_offset_at(summer), "UTC-04:00");
        assert_eq!(ny.format_offset_at(winter), "UTC-05:00");
        assert_eq!(TimeZoneSpec::from_tzif(b"bad"), None);
    }
}
