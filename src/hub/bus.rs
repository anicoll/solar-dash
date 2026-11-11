//! The hub's event bus and its two own jobs: the midnight clock and a debug
//! log of everything that passes by.

use std::time::Duration;

use chrono::{Days, LocalResult, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use tokio::sync::broadcast;
use tracing::{debug, warn};

use super::Shutdown;
use crate::metric::Metric;
use crate::modbus::Sample;

/// How many events a slow listener may fall behind before it loses the oldest.
const CAPACITY: usize = 256;

/// Something that happened, announced to anyone listening.
#[derive(Debug, Clone)]
pub enum HubEvent {
    /// The Modbus poller read a new sample (about every 15 seconds).
    SampleArrived(Sample),
    /// A source saved readings. `source` is the name they are stored under
    /// (like "isolarcloud:123"); `from` and `to` are the first and last
    /// reading time, unix seconds.
    ReadingsStored {
        source: String,
        metrics: Vec<Metric>,
        from: i64,
        to: i64,
    },
    /// A source saved prices, under this name (like "amber:abc").
    PricesStored { source: String },
    /// A local day has just ended. Sent by the hub itself at local midnight.
    DayClosed(NaiveDate),
}

/// A handle to the event bus. Cloning gives another handle to the same bus.
#[derive(Clone)]
pub struct Hub {
    events: broadcast::Sender<HubEvent>,
}

impl Default for Hub {
    fn default() -> Self {
        Self {
            events: broadcast::channel(CAPACITY).0,
        }
    }
}

impl Hub {
    /// Announces an event. With nobody listening this does nothing, and a
    /// publisher never waits for a listener.
    pub fn publish(&self, event: HubEvent) {
        let _ = self.events.send(event);
    }

    /// Starts listening. A listener that falls more than `CAPACITY` events
    /// behind gets `RecvError::Lagged` once, then carries on with the newest
    /// events, so it must treat that as "I missed some", not as a failure.
    pub fn subscribe(&self) -> broadcast::Receiver<HubEvent> {
        self.events.subscribe()
    }
}

/// Logs each event at debug level, until shutdown.
pub(super) async fn log_events(mut events: broadcast::Receiver<HubEvent>, shutdown: Shutdown) {
    let shutdown = shutdown.wait();
    tokio::pin!(shutdown);
    loop {
        let event = tokio::select! {
            _ = &mut shutdown => return,
            event = events.recv() => event,
        };
        match event {
            Ok(HubEvent::SampleArrived(sample)) => debug!(ts = sample.ts, "event: sample arrived"),
            Ok(HubEvent::ReadingsStored {
                source,
                metrics,
                from,
                to,
            }) => debug!(
                source,
                metrics = metrics.len(),
                from,
                to,
                "event: readings stored"
            ),
            Ok(HubEvent::PricesStored { source }) => debug!(source, "event: prices stored"),
            Ok(HubEvent::DayClosed(date)) => debug!(%date, "event: day closed"),
            // Missing events only matters to a listener that needs all of
            // them; this one just logs.
            Err(broadcast::error::RecvError::Lagged(missed)) => {
                debug!(missed, "event log fell behind")
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// Current time as unix seconds.
pub(super) fn unix_now() -> i64 {
    Utc::now().timestamp()
}

/// Unix time of the first moment of `date` in `tz`. If midnight does not
/// exist (clocks jump over it) the first hour that does is used; if it
/// happens twice, the earlier one.
fn local_midnight(date: NaiveDate, tz: Tz) -> Option<i64> {
    for hour in 0..3 {
        let naive = date.and_hms_opt(hour, 0, 0)?;
        match tz.from_local_datetime(&naive) {
            LocalResult::Single(t) => return Some(t.timestamp()),
            LocalResult::Ambiguous(earliest, _) => return Some(earliest.timestamp()),
            LocalResult::None => continue,
        }
    }
    None
}

/// When local `date` ends, as unix seconds. On a daylight saving change the
/// day is 23 or 25 hours long, which is why this asks the time zone instead
/// of adding 24 hours.
fn day_end(date: NaiveDate, tz: Tz) -> Option<i64> {
    local_midnight(date.checked_add_days(Days::new(1))?, tz)
}

/// Sends [`HubEvent::DayClosed`] at each local midnight until shutdown.
/// `now` is the clock, a parameter so tests can use their own.
pub(super) async fn run_day_clock(hub: Hub, tz: Tz, now: impl Fn() -> i64, shutdown: Shutdown) {
    let shutdown = shutdown.wait();
    tokio::pin!(shutdown);
    // The day we are waiting to see end. Counting days from here, instead of
    // asking the clock "what day is it" after each wake-up, means a wake-up a
    // moment early or late can neither skip nor repeat a day.
    let mut open = Utc
        .timestamp_opt(now(), 0)
        .single()
        .map(|t| t.with_timezone(&tz).date_naive());
    while let Some(date) = open {
        let Some(end) = day_end(date, tz) else {
            warn!(%date, "could not work out when the day ends; no more day-closed events");
            return;
        };
        let wait = end - now();
        if wait > 0 {
            tokio::select! {
                _ = &mut shutdown => return,
                _ = tokio::time::sleep(Duration::from_secs(wait as u64)) => {}
            }
            // Look at the clock again before announcing.
            continue;
        }
        hub.publish(HubEvent::DayClosed(date));
        open = date.checked_add_days(Days::new(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::shutdown_channel;

    const ADELAIDE: Tz = chrono_tz::Australia::Adelaide;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[tokio::test]
    async fn events_reach_every_subscriber() {
        let hub = Hub::default();
        let mut a = hub.subscribe();
        let mut b = hub.subscribe();
        hub.publish(HubEvent::PricesStored {
            source: "amber:x".into(),
        });
        for rx in [&mut a, &mut b] {
            match rx.recv().await.unwrap() {
                HubEvent::PricesStored { source } => assert_eq!(source, "amber:x"),
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    #[test]
    fn publishing_without_listeners_is_fine() {
        Hub::default().publish(HubEvent::DayClosed(date(2026, 1, 1)));
    }

    #[tokio::test]
    async fn a_lagging_listener_is_told_and_then_catches_up() {
        let hub = Hub::default();
        let mut rx = hub.subscribe();
        for day in 0..(CAPACITY as u32 + 10) {
            hub.publish(HubEvent::DayClosed(
                date(2026, 1, 1) + Days::new(day.into()),
            ));
        }
        assert!(matches!(
            rx.recv().await,
            Err(broadcast::error::RecvError::Lagged(_))
        ));
        // Newer events still arrive after the warning.
        assert!(matches!(rx.recv().await, Ok(HubEvent::DayClosed(_))));
    }

    #[test]
    fn days_end_at_local_midnight_across_daylight_saving() {
        let ts = |d: NaiveDate| day_end(d, ADELAIDE).unwrap();
        // Adelaide: clocks go forward on 2026-10-04 (23-hour day) and back
        // on 2026-04-05 (25-hour day).
        assert_eq!(ts(date(2026, 10, 4)) - ts(date(2026, 10, 3)), 23 * 3600);
        assert_eq!(ts(date(2026, 4, 5)) - ts(date(2026, 4, 4)), 25 * 3600);
        assert_eq!(ts(date(2026, 7, 1)) - ts(date(2026, 6, 30)), 24 * 3600);
        // 2026-07-01 00:00 +09:30 is 2026-06-30 14:30 UTC.
        assert_eq!(
            ts(date(2026, 6, 30)),
            Utc.with_ymd_and_hms(2026, 6, 30, 14, 30, 0)
                .unwrap()
                .timestamp()
        );
    }

    /// The clock sends one `DayClosed` per local midnight, on the right
    /// days, including the short day of a daylight saving change.
    #[tokio::test(start_paused = true)]
    async fn the_day_clock_announces_each_local_midnight() {
        // 2026-10-03 23:00 local (+09:30), an hour before a 23-hour day starts.
        let start = Utc
            .with_ymd_and_hms(2026, 10, 3, 13, 30, 0)
            .unwrap()
            .timestamp();
        let began = tokio::time::Instant::now();
        let now = move || start + began.elapsed().as_secs() as i64;

        let hub = Hub::default();
        let mut rx = hub.subscribe();
        let (trigger, shutdown) = shutdown_channel();
        let clock = tokio::spawn(run_day_clock(hub.clone(), ADELAIDE, now, shutdown));

        // Paused time jumps ahead whenever everything is waiting.
        for expected in [date(2026, 10, 3), date(2026, 10, 4), date(2026, 10, 5)] {
            match rx.recv().await.unwrap() {
                HubEvent::DayClosed(day) => assert_eq!(day, expected),
                other => panic!("unexpected {other:?}"),
            }
        }
        trigger.fire();
        clock.await.unwrap();
    }
}
