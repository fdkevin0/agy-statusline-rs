//! Shared `jiff`-backed time, civil date, and RFC3339 timestamp utilities.

use jiff::civil::{self, Date};
use jiff::tz::Offset;
use jiff::Timestamp;

pub(crate) const SECS_PER_DAY: i64 = 86_400;

/// Returns the current Unix epoch timestamp in seconds (`0` if system clock is before 1970).
pub fn unix_now_secs() -> u64 {
    Timestamp::now().as_second().max(0) as u64
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
    let ts = Timestamp::from_second(secs).unwrap_or(Timestamp::UNIX_EPOCH);
    Offset::UTC.to_datetime(ts).date().to_string()
}

/// Converts days since `1970-01-01` into a proleptic Gregorian [`jiff::civil::Date`].
pub(crate) fn civil_date_from_unix_day(day_idx: i64) -> Date {
    day_idx
        .checked_mul(SECS_PER_DAY)
        .and_then(|secs| Timestamp::from_second(secs).ok())
        .map(|ts| Offset::UTC.to_datetime(ts).date())
        .unwrap_or_else(|| civil::date(1970, 1, 1))
}

/// Converts a proleptic Gregorian [`jiff::civil::Date`] into days since `1970-01-01`.
pub(crate) fn unix_day_from_civil_date(date: Date) -> i64 {
    Offset::UTC
        .to_timestamp(date.at(0, 0, 0, 0))
        .map(|ts| ts.as_second().div_euclid(SECS_PER_DAY))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_date_roundtrips_and_out_of_range_falls_back_to_epoch() {
        let epoch = civil::date(1970, 1, 1);
        assert_eq!(unix_day_from_civil_date(epoch), 0);
        assert_eq!(civil_date_from_unix_day(0), epoch);
        assert_eq!(unix_secs_to_utc_date(0), "1970-01-01");

        let pre_epoch = civil::date(1969, 12, 31);
        assert_eq!(unix_day_from_civil_date(pre_epoch), -1);
        assert_eq!(civil_date_from_unix_day(-1), pre_epoch);

        let leap = civil::date(2024, 2, 29);
        let leap_day = unix_day_from_civil_date(leap);
        assert_eq!(civil_date_from_unix_day(leap_day), leap);
        assert_eq!(unix_secs_to_utc_date(1_709_251_199), "2024-02-29");

        assert_eq!(civil_date_from_unix_day(i64::MAX), epoch);
        assert_eq!(civil_date_from_unix_day(i64::MIN), epoch);
        assert_eq!(unix_secs_to_utc_date(i64::MAX), "1970-01-01");
        assert_eq!(unix_secs_to_utc_date(i64::MIN), "1970-01-01");
    }
}
