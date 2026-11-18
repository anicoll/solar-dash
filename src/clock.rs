//! Site time: the one place that turns local dates and clock times into unix
//! seconds and back, using [jiff].
//!
//! Every day boundary in solar-dash is a *local* day in the site's time zone
//! (`[site] timezone`, Adelaide by default). Daylight saving makes some local
//! times happen twice (clocks go back) and some never happen (clocks go
//! forward), so the rules for those cases live here instead of being copied
//! into each module:
//!
//! - a local time that never happened gives `None`;
//! - a local time that happened twice gives the earlier of the two.
//!
//! The `*_to_jiff` / `*_to_chrono` functions at the bottom are temporary
//! bridges while the code moves off chrono one module at a time. They go away
//! with chrono itself.

// Most helpers get their callers in the following migration PRs; the last one
// removes this allow.
#![allow(dead_code)]

use anyhow::{Context, Result};
use jiff::civil::{Date, DateTime, Time};
use jiff::tz::{AmbiguousOffset, TimeZone};
use jiff::{Timestamp, ToSpan};

/// Looks up an IANA time zone such as `Australia/Adelaide`.
pub fn site_tz(name: &str) -> Result<TimeZone> {
    TimeZone::get(name).with_context(|| format!("unknown timezone {name:?}"))
}

/// Current time as unix seconds.
pub fn unix_now() -> i64 {
    Timestamp::now().as_second()
}

/// Unix time of the local `datetime` in `tz`. `None` if that moment never
/// happened (a daylight saving gap); the earlier one if it happened twice.
pub fn local_ts(datetime: DateTime, tz: &TimeZone) -> Option<i64> {
    let ambiguous = tz.to_ambiguous_timestamp(datetime);
    if matches!(ambiguous.offset(), AmbiguousOffset::Gap { .. }) {
        return None;
    }
    ambiguous.earlier().ok().map(|t| t.as_second())
}

/// Unix time of `minutes` after local midnight on `date`, or `None` if that
/// moment never happened. The minutes are counted on the wall clock, so 600
/// is always 10:00 even on a day the clocks change.
pub fn local_minutes(date: Date, minutes: u32, tz: &TimeZone) -> Option<i64> {
    let at = date
        .to_datetime(Time::midnight())
        .checked_add(i64::from(minutes).minutes())
        .ok()?;
    local_ts(at, tz)
}

/// Unix time of the first moment of `date` in `tz`.
///
/// Midnight normally exists exactly once. In a zone whose clocks jump over
/// midnight the first whole hour that does exist is used; if clocks go back
/// over midnight, the earlier of the two.
pub fn day_start(date: Date, tz: &TimeZone) -> Option<i64> {
    (0..3).find_map(|hour| local_ts(date.to_datetime(Time::constant(hour, 0, 0, 0)), tz))
}

/// When local `date` ends (the start of the next day), as unix seconds. A
/// day with a clock change is 23 or 25 hours long, which is why this asks the
/// time zone instead of adding 24 hours.
pub fn day_end(date: Date, tz: &TimeZone) -> Option<i64> {
    day_start(date.tomorrow().ok()?, tz)
}

/// The local day `date` as `[start, end)` in unix seconds.
pub fn day_bounds(date: Date, tz: &TimeZone) -> Option<(i64, i64)> {
    Some((day_start(date, tz)?, day_end(date, tz)?))
}

/// The local date of unix time `ts` in `tz`. Panics only for timestamps
/// outside jiff's range (years before -9999 or after 9999).
pub fn local_date(ts: i64, tz: &TimeZone) -> Date {
    local_datetime(ts, tz).date()
}

/// The local wall-clock time of unix time `ts` in `tz`.
pub fn local_datetime(ts: i64, tz: &TimeZone) -> DateTime {
    tz.to_datetime(Timestamp::from_second(ts).expect("unix time within jiff's range"))
}

/// Today's local date in `tz`.
pub fn today(tz: &TimeZone) -> Date {
    local_date(unix_now(), tz)
}

// ---------------------------------------------------------------------------
// Temporary bridges to chrono, removed with chrono itself
// ---------------------------------------------------------------------------

/// A chrono-tz zone as a jiff zone (looked up by its IANA name).
pub fn tz_to_jiff(tz: chrono_tz::Tz) -> TimeZone {
    TimeZone::get(tz.name()).expect("chrono-tz zones are all in the tz database")
}

/// A jiff zone as a chrono-tz zone. Zones that did not come from a name
/// (never the case for the site zone) fall back to UTC.
pub fn tz_to_chrono(tz: &TimeZone) -> chrono_tz::Tz {
    tz.iana_name()
        .and_then(|name| name.parse().ok())
        .unwrap_or(chrono_tz::UTC)
}

/// A chrono date as a jiff date.
pub fn date_to_jiff(date: chrono::NaiveDate) -> Date {
    use chrono::Datelike;
    // Every date solar-dash handles is far inside both crates' ranges.
    Date::new(date.year() as i16, date.month() as i8, date.day() as i8)
        .expect("chrono date within jiff's range")
}

/// A jiff date as a chrono date.
pub fn date_to_chrono(date: Date) -> chrono::NaiveDate {
    chrono::NaiveDate::from_ymd_opt(
        i32::from(date.year()),
        date.month() as u32,
        date.day() as u32,
    )
    .expect("jiff date within chrono's range")
}

/// A jiff time of day as a chrono time (whole seconds).
pub fn time_to_chrono(time: Time) -> chrono::NaiveTime {
    chrono::NaiveTime::from_hms_opt(
        time.hour() as u32,
        time.minute() as u32,
        time.second() as u32,
    )
    .expect("jiff time is a valid time of day")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{LocalResult, TimeZone as _};
    use jiff::civil::date;

    fn adelaide() -> TimeZone {
        site_tz("Australia/Adelaide").unwrap()
    }

    /// The rule the old chrono code used everywhere: gap = None, overlap = earlier.
    fn chrono_local(tz: chrono_tz::Tz, date: chrono::NaiveDate, h: u32, m: u32) -> Option<i64> {
        match tz.from_local_datetime(&date.and_hms_opt(h, m, 0).unwrap()) {
            LocalResult::Single(t) => Some(t.timestamp()),
            LocalResult::Ambiguous(earliest, _) => Some(earliest.timestamp()),
            LocalResult::None => None,
        }
    }

    #[test]
    fn matches_chrono_on_every_half_hour_2024_to_2027() {
        let ctz = chrono_tz::Australia::Adelaide;
        let tz = adelaide();
        let mut day = date(2024, 1, 1);
        let mut checked = 0;
        while day < date(2028, 1, 1) {
            let cday = date_to_chrono(day);
            for minutes in (0..24 * 60).step_by(30) {
                assert_eq!(
                    local_minutes(day, minutes, &tz),
                    chrono_local(ctz, cday, minutes / 60, minutes % 60),
                    "{day} +{minutes} min"
                );
                checked += 1;
            }
            // Month arithmetic clamps to the end of the month in both crates.
            assert_eq!(
                date_to_chrono(day.checked_add(1.month()).unwrap()),
                cday.checked_add_months(chrono::Months::new(1)).unwrap()
            );
            assert_eq!(date_to_jiff(cday), day);
            day = day.tomorrow().unwrap();
        }
        assert_eq!(checked, 70_128);
    }

    #[test]
    fn clock_change_days_are_23_and_25_hours() {
        let tz = adelaide();
        // Clocks go forward at 02:00 on 4 Oct 2026 and back at 03:00 on 5 Apr 2026.
        let (start, end) = day_bounds(date(2026, 10, 4), &tz).unwrap();
        assert_eq!(end - start, 23 * 3600);
        let (start, end) = day_bounds(date(2026, 4, 5), &tz).unwrap();
        assert_eq!(end - start, 25 * 3600);
        // 02:30 on the forward day never happened.
        assert_eq!(local_minutes(date(2026, 10, 4), 150, &tz), None);
    }

    #[test]
    fn local_date_round_trips_day_start() {
        let tz = adelaide();
        let day = date(2027, 1, 15);
        let start = day_start(day, &tz).unwrap();
        assert_eq!(local_date(start, &tz), day);
        assert_eq!(local_date(start - 1, &tz), date(2027, 1, 14));
        // Adelaide is UTC+10:30 in January: the day starts 13:30 UTC the day before.
        // 1_800_000_000 is 2027-01-15 08:00 UTC, 18:30 in Adelaide.
        assert_eq!(start, 1_800_000_000 - (18 * 3600 + 30 * 60));
    }

    #[test]
    fn zone_names_survive_both_bridges() {
        let tz = adelaide();
        assert_eq!(tz.iana_name(), Some("Australia/Adelaide"));
        assert_eq!(tz_to_chrono(&tz), chrono_tz::Australia::Adelaide);
        assert_eq!(
            tz_to_jiff(chrono_tz::Australia::Perth).iana_name(),
            Some("Australia/Perth")
        );
        assert!(site_tz("Mars/Olympus").is_err());
    }

    #[test]
    fn dates_serialise_like_chrono() {
        let day = date(2026, 11, 18);
        assert_eq!(
            serde_json::to_string(&day).unwrap(),
            serde_json::to_string(&date_to_chrono(day)).unwrap()
        );
        let time: Time = "11:00".parse().unwrap();
        assert_eq!(time_to_chrono(time).to_string(), "11:00:00");
    }
}
