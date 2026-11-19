//! Shared `jiff`-backed time, civil date, and RFC3339 timestamp utilities.

use std::time::{SystemTime, UNIX_EPOCH};

use jiff::civil::{self, Date};
use jiff::tz::TimeZone;
use jiff::Timestamp;

pub(crate) const SECS_PER_DAY: i64 = 86_400;

/// Returns the current Unix epoch timestamp in seconds (`0` if system clock is before 1970).
pub(crate) fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Parses an RFC3339 / ISO-8601 timestamp (`YYYY-MM-DDTHH:MM:SS[.fff](Z|+HH:MM|-HH:MM)`)
/// into Unix epoch seconds (clamping leap second `:60` to `:59`).
pub(crate) fn parse_rfc3339_unix_secs(raw: &str) -> Option<i64> {
    raw.trim()
        .parse::<Timestamp>()
        .ok()
        .map(Timestamp::as_second)
}

/// Formats a Unix epoch timestamp (seconds) as a UTC `YYYY-MM-DD` date string.
pub(crate) fn unix_secs_to_utc_date(secs: i64) -> String {
    Timestamp::from_second(secs)
        .map(|ts| ts.to_zoned(TimeZone::UTC).date().to_string())
        .unwrap_or_else(|_| civil_date_from_unix_day(secs.div_euclid(SECS_PER_DAY)).to_string())
}

/// Converts days since `1970-01-01` into a proleptic Gregorian [`jiff::civil::Date`].
pub(crate) fn civil_date_from_unix_day(day_idx: i64) -> Date {
    day_idx
        .checked_mul(SECS_PER_DAY)
        .and_then(|secs| Timestamp::from_second(secs).ok())
        .map(|ts| ts.to_zoned(TimeZone::UTC).date())
        .unwrap_or_else(|| civil::date(1970, 1, 1))
}

/// Converts a proleptic Gregorian [`jiff::civil::Date`] into days since `1970-01-01`.
pub(crate) fn unix_day_from_civil_date(date: Date) -> i64 {
    date.to_zoned(TimeZone::UTC)
        .map(|z| z.timestamp().as_second().div_euclid(SECS_PER_DAY))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_date_roundtrips_unix_days_and_utc_formatting() {
        let epoch = civil::date(1970, 1, 1);
        assert_eq!(unix_day_from_civil_date(epoch), 0);
        assert_eq!(civil_date_from_unix_day(0), epoch);
        assert_eq!(unix_secs_to_utc_date(0), "1970-01-01");

        let leap = civil::date(2024, 2, 29);
        let leap_day = unix_day_from_civil_date(leap);
        assert_eq!(civil_date_from_unix_day(leap_day), leap);
        assert_eq!(unix_secs_to_utc_date(1_709_251_199), "2024-02-29");
    }

    #[test]
    fn rfc3339_parses_offsets_clamps_leap_second_and_rejects_invalid() {
        assert_eq!(parse_rfc3339_unix_secs("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339_unix_secs("  2026-11-18T12:00:00Z  "),
            Some(1_795_003_200)
        );
        assert_eq!(
            parse_rfc3339_unix_secs("2026-11-18t12:00:00.456z"),
            Some(1_795_003_200)
        );
        assert_eq!(
            parse_rfc3339_unix_secs("2026-11-18 20:00:00+08:00"),
            Some(1_795_003_200)
        );
        assert_eq!(
            parse_rfc3339_unix_secs("2026-11-18T07:00:00-05:00"),
            Some(1_795_003_200)
        );
        assert_eq!(
            parse_rfc3339_unix_secs("2024-02-29T23:59:59Z"),
            Some(1_709_251_199)
        );
        assert_eq!(
            parse_rfc3339_unix_secs("2026-11-18T23:59:60Z"),
            parse_rfc3339_unix_secs("2026-11-18T23:59:59Z")
        );
        assert_eq!(parse_rfc3339_unix_secs("2026-02-29T00:00:00Z"), None);
        assert_eq!(parse_rfc3339_unix_secs("2026-13-01T00:00:00Z"), None);
        assert_eq!(parse_rfc3339_unix_secs("2026-11-18T12:00:00"), None);
        assert_eq!(parse_rfc3339_unix_secs("2026-11-18T12:00:00.Z"), None);
        assert_eq!(parse_rfc3339_unix_secs("2026-11-18T12:00:00Zextra"), None);
    }
}
