//! The hub's event bus and its two own jobs: the midnight clock and a debug
//! log of everything that passes by.

use std::time::Duration;

use jiff::civil::Date;
use jiff::tz::TimeZone;
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
    DayClosed(Date),
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

/// Sends [`HubEvent::DayClosed`] at each local midnight until shutdown.
/// `now` is the clock, a parameter so tests can use their own.
pub(super) async fn run_day_clock(
    hub: Hub,
    tz: TimeZone,
    now: impl Fn() -> i64,
    shutdown: Shutdown,
) {
    let shutdown = shutdown.wait();
    tokio::pin!(shutdown);
    // The day we are waiting to see end. Counting days from here, instead of
    // asking the clock "what day is it" after each wake-up, means a wake-up a
    // moment early or late can neither skip nor repeat a day.
    let mut open = jiff::Timestamp::from_second(now())
        .ok()
        .map(|t| tz.to_datetime(t).date());
    while let Some(date) = open {
        let Some(end) = crate::clock::day_end(date, &tz) else {
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
        open = date.tomorrow().ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::shutdown_channel;
    use jiff::ToSpan;

    fn adelaide() -> TimeZone {
        crate::clock::site_tz("Australia/Adelaide").unwrap()
    }

    fn date(y: i16, m: i8, d: i8) -> Date {
        jiff::civil::date(y, m, d)
    }

    /// Unix time of a UTC date and time.
    fn utc_ts(y: i16, m: i8, d: i8, h: i8, min: i8, s: i8) -> i64 {
        TimeZone::UTC
            .to_timestamp(jiff::civil::datetime(y, m, d, h, min, s, 0))
            .unwrap()
            .as_second()
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
        for day in 0..(CAPACITY as i32 + 10) {
            hub.publish(HubEvent::DayClosed(
                date(2026, 1, 1).checked_add(day.days()).unwrap(),
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
        let tz = adelaide();
        let ts = |d: Date| crate::clock::day_end(d, &tz).unwrap();
        // Adelaide: clocks go forward on 2026-10-04 (23-hour day) and back
        // on 2026-04-05 (25-hour day).
        assert_eq!(ts(date(2026, 10, 4)) - ts(date(2026, 10, 3)), 23 * 3600);
        assert_eq!(ts(date(2026, 4, 5)) - ts(date(2026, 4, 4)), 25 * 3600);
        assert_eq!(ts(date(2026, 7, 1)) - ts(date(2026, 6, 30)), 24 * 3600);
        // 2026-07-01 00:00 +09:30 is 2026-06-30 14:30 UTC.
        assert_eq!(ts(date(2026, 6, 30)), utc_ts(2026, 6, 30, 14, 30, 0));
    }

    /// The clock sends one `DayClosed` per local midnight, on the right
    /// days, including the short day of a daylight saving change.
    #[tokio::test(start_paused = true)]
    async fn the_day_clock_announces_each_local_midnight() {
        // 2026-10-03 23:00 local (+09:30), an hour before a 23-hour day starts.
        let start = utc_ts(2026, 10, 3, 13, 30, 0);
        let began = tokio::time::Instant::now();
        let now = move || start + began.elapsed().as_secs() as i64;

        let hub = Hub::default();
        let mut rx = hub.subscribe();
        let (trigger, shutdown) = shutdown_channel();
        let clock = tokio::spawn(run_day_clock(hub.clone(), adelaide(), now, shutdown));

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
