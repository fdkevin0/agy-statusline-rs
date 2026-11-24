//! Shared `jiff`-backed time, timezone, civil date, and RFC3339 timestamp utilities.

use jiff::civil::{self, Date};
use jiff::tz::{Offset, TimeZone};
use jiff::Timestamp;

pub(crate) const SECS_PER_HOUR: u64 = 3_600;
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
pub fn unix_secs_to_utc_date(secs: i64) -> String {
    let ts = Timestamp::from_second(secs).unwrap_or(Timestamp::UNIX_EPOCH);
    Offset::UTC.to_datetime(ts).date().to_string()
}

/// Formats days since `1970-01-01` as a `YYYY-MM-DD` string.
pub(crate) fn format_day(day_idx: i64) -> String {
    civil_date_from_unix_day(day_idx).to_string()
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

/// Timezone specification used to map UTC timestamps to local calendar days and datetimes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TimeZoneSpec {
    inner: TimeZone,
}

impl TimeZoneSpec {
    /// Loads the host local timezone via `jiff::tz::TimeZone::system()`, falling back to UTC.
    pub(crate) fn local() -> Self {
        Self {
            inner: TimeZone::system(),
        }
    }

    #[cfg(test)]
    pub(crate) fn utc() -> Self {
        Self {
            inner: TimeZone::UTC,
        }
    }

    #[cfg(test)]
    pub(crate) fn fixed_offset(offset_secs: i32) -> Self {
        let clamped = offset_secs.clamp(-93_599, 93_599);
        let offset = Offset::from_seconds(clamped).unwrap_or(Offset::UTC);
        Self {
            inner: TimeZone::fixed(offset),
        }
    }

    #[cfg(test)]
    pub(crate) fn from_jiff(inner: TimeZone) -> Self {
        Self { inner }
    }

    /// Formats the timezone offset at `unix_secs` as `UTC`, `UTC+HH:MM`, or `UTC-HH:MM`.
    pub(crate) fn format_offset_at(&self, unix_secs: i64) -> String {
        let secs = Timestamp::from_second(unix_secs)
            .map(|ts| self.inner.to_offset(ts).seconds())
            .unwrap_or(0);
        if secs == 0 {
            return "UTC".to_string();
        }
        let sign = if secs < 0 { '-' } else { '+' };
        let abs = secs.unsigned_abs();
        let (hours, mins, rem_secs) = (abs / 3_600, (abs % 3_600) / 60, abs % 60);
        if rem_secs == 0 {
            format!("UTC{sign}{hours:02}:{mins:02}")
        } else {
            format!("UTC{sign}{hours:02}:{mins:02}:{rem_secs:02}")
        }
    }

    /// Maps a UTC Unix timestamp (seconds) into a local day index since `1970-01-01`.
    pub(crate) fn local_day(&self, unix_secs: i64) -> i64 {
        let date = Timestamp::from_second(unix_secs)
            .map(|ts| self.inner.to_datetime(ts).date())
            .unwrap_or_else(|_| civil_date_from_unix_day(unix_secs.div_euclid(SECS_PER_DAY)));
        unix_day_from_civil_date(date)
    }

    /// Formats a UTC Unix timestamp (seconds) as a local `YYYY-MM-DD HH:MM:SS` timestamp.
    pub(crate) fn local_datetime_string(&self, unix_secs: i64) -> String {
        Timestamp::from_second(unix_secs)
            .map(|ts| {
                let dt = self.inner.to_datetime(ts);
                format!(
                    "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                    dt.year(),
                    dt.month(),
                    dt.day(),
                    dt.hour(),
                    dt.minute(),
                    dt.second()
                )
            })
            .unwrap_or_else(|_| {
                format!(
                    "{} 00:00:00",
                    format_day(unix_secs.div_euclid(SECS_PER_DAY))
                )
            })
    }
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

    #[test]
    fn fixed_offset_and_posix_dst_timezone_bucketing() {
        let shanghai = TimeZoneSpec::fixed_offset(8 * 3_600);
        // 2026-11-17T20:00:00Z is 2026-11-18 04:00:00 in UTC+08:00
        let ts_utc_17 = parse_rfc3339_unix_secs("2026-11-17T20:00:00Z").unwrap();
        assert_eq!(shanghai.format_offset_at(ts_utc_17), "UTC+08:00");
        assert_eq!(format_day(shanghai.local_day(ts_utc_17)), "2026-11-18");

        assert_eq!(TimeZoneSpec::utc().format_offset_at(ts_utc_17), "UTC");
        assert_eq!(
            format_day(TimeZoneSpec::utc().local_day(ts_utc_17)),
            "2026-11-17"
        );

        // US Eastern POSIX rule with DST: EST5EDT,M3.2.0,M11.1.0
        let ny = TimeZoneSpec::from_jiff(TimeZone::posix("EST5EDT,M3.2.0,M11.1.0").unwrap());
        let summer = parse_rfc3339_unix_secs("2026-07-01T12:00:00Z").unwrap();
        let winter = parse_rfc3339_unix_secs("2026-11-17T12:00:00Z").unwrap();
        assert_eq!(ny.format_offset_at(summer), "UTC-04:00");
        assert_eq!(ny.format_offset_at(winter), "UTC-05:00");
    }
}
