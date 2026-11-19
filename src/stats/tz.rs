//! Local timezone resolution backed by `jiff` and civil date helpers.

#[cfg(test)]
use jiff::tz::Offset;
use jiff::tz::TimeZone;
use jiff::Timestamp;

use crate::timeutil::{
    civil_date_from_unix_day, unix_day_from_civil_date, SECS_PER_DAY,
};

pub(super) fn format_day(day_idx: i64) -> String {
    civil_date_from_unix_day(day_idx).to_string()
}

/// Formats a Unix epoch timestamp (seconds) as a UTC `YYYY-MM-DD` date string.
pub fn unix_secs_to_utc_date(unix_secs: u64) -> String {
    crate::timeutil::unix_secs_to_utc_date(unix_secs as i64)
}

/// Timezone specification used to map UTC timestamps to local calendar days.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TimeZoneSpec {
    inner: TimeZone,
}

impl TimeZoneSpec {
    /// Loads the host local timezone via `jiff::tz::TimeZone::system()`, falling back to UTC.
    pub(super) fn local() -> Self {
        Self {
            inner: TimeZone::system(),
        }
    }

    /// Returns a UTC (`+00:00`) timezone specification.
    #[cfg(test)]
    pub(super) fn utc() -> Self {
        Self {
            inner: TimeZone::UTC,
        }
    }

    /// Returns a fixed-offset timezone specification (`offset_secs` east of UTC, e.g. `+28_800` for `UTC+08:00`).
    #[cfg(test)]
    pub(super) fn fixed_offset(offset_secs: i32) -> Self {
        let clamped = offset_secs.clamp(-93_599, 93_599);
        let offset = Offset::from_seconds(clamped).unwrap_or(Offset::UTC);
        Self {
            inner: TimeZone::fixed(offset),
        }
    }

    /// Wraps an explicit `jiff::tz::TimeZone`.
    #[cfg(test)]
    pub(super) fn from_jiff(inner: TimeZone) -> Self {
        Self { inner }
    }

    /// Formats the timezone offset at `unix_secs` as `UTC`, `UTC+HH:MM`, or `UTC-HH:MM`.
    pub(super) fn format_offset_at(&self, unix_secs: i64) -> String {
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

    /// Maps a UTC Unix timestamp (seconds) into `(local_day_index, "YYYY-MM-DD")`.
    pub(super) fn local_day_and_date(&self, unix_secs: i64) -> (i64, String) {
        let date = Timestamp::from_second(unix_secs)
            .map(|ts| self.inner.to_datetime(ts).date())
            .unwrap_or_else(|_| civil_date_from_unix_day(unix_secs.div_euclid(SECS_PER_DAY)));
        (unix_day_from_civil_date(date), date.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeutil::parse_rfc3339_unix_secs;

    #[test]
    fn fixed_offset_and_posix_dst_timezone_bucketing() {
        let shanghai = TimeZoneSpec::fixed_offset(8 * 3_600);
        // 2026-11-17T20:00:00Z is 2026-11-18 04:00:00 in UTC+08:00
        let ts_utc_17 = parse_rfc3339_unix_secs("2026-11-17T20:00:00Z").unwrap();
        assert_eq!(shanghai.format_offset_at(ts_utc_17), "UTC+08:00");
        assert_eq!(shanghai.local_day_and_date(ts_utc_17).1, "2026-11-18");

        assert_eq!(TimeZoneSpec::utc().format_offset_at(ts_utc_17), "UTC");
        assert_eq!(
            TimeZoneSpec::utc().local_day_and_date(ts_utc_17).1,
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
