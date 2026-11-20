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

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::civil::date;

    fn adelaide() -> TimeZone {
        site_tz("Australia/Adelaide").unwrap()
    }

    #[test]
    fn clock_change_days_are_23_and_25_hours() {
        let tz = adelaide();
        // Clocks go forward at 02:00 on 4 Oct 2026 and back at 03:00 on 5 Apr 2026.
        let (start, end) = day_bounds(date(2026, 10, 4), &tz).unwrap();
        assert_eq!(end - start, 23 * 3600);
        let (start, end) = day_bounds(date(2026, 4, 5), &tz).unwrap();
        assert_eq!(end - start, 25 * 3600);
        // An ordinary day.
        let (start, end) = day_bounds(date(2026, 11, 18), &tz).unwrap();
        assert_eq!(end - start, 24 * 3600);
    }

    #[test]
    fn skipped_hour_is_none_and_repeated_hour_is_the_earlier() {
        let tz = adelaide();
        // 02:30 on 4 Oct 2026 never happened.
        assert_eq!(local_minutes(date(2026, 10, 4), 150, &tz), None);
        // 02:30 on 5 Apr 2026 happened twice: first at +10:30 (16:00 UTC the
        // day before), then an hour later at +09:30.
        let first = local_minutes(date(2026, 4, 5), 150, &tz).unwrap();
        assert_eq!(
            first,
            local_ts(date(2026, 4, 5).at(1, 30, 0, 0), &tz).unwrap() + 3600
        );
        assert_eq!(local_datetime(first, &tz), date(2026, 4, 5).at(2, 30, 0, 0));
        assert_eq!(
            local_datetime(first + 3600, &tz),
            date(2026, 4, 5).at(2, 30, 0, 0)
        );
    }

    #[test]
    fn local_date_round_trips_day_start() {
        let tz = adelaide();
        let day = date(2027, 1, 15);
        let start = day_start(day, &tz).unwrap();
        assert_eq!(local_date(start, &tz), day);
        assert_eq!(local_date(start - 1, &tz), date(2027, 1, 14));
        // 1_800_000_000 is 2027-01-15 08:00 UTC, 18:30 in Adelaide.
        assert_eq!(start, 1_800_000_000 - (18 * 3600 + 30 * 60));
    }

    #[test]
    fn month_arithmetic_clamps_to_the_end_of_the_month() {
        assert_eq!(
            date(2026, 1, 31).checked_add(1.month()).unwrap(),
            date(2026, 2, 28)
        );
        assert_eq!(
            date(2028, 1, 31).checked_add(1.month()).unwrap(),
            date(2028, 2, 29)
        );
    }

    #[test]
    fn zone_names_and_date_text() {
        assert_eq!(adelaide().iana_name(), Some("Australia/Adelaide"));
        assert!(site_tz("Mars/Olympus").is_err());
        assert_eq!(
            serde_json::to_string(&date(2026, 11, 18)).unwrap(),
            "\"2026-11-18\""
        );
        let time: Time = "11:00".parse().unwrap();
        assert_eq!(time, Time::constant(11, 0, 0, 0));
    }
}
