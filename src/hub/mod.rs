//! The hub and its spokes.
//!
//! The dashboard talks to a handful of outside systems: the inverter over
//! Modbus, iSolarCloud, the GloBird portal, TeslaMate, Amber, and the backup
//! folders. Each of those is a **spoke**: a background job that is switched on
//! by its own config section, does one thing and only logs its own errors.
//! The **hub** is the small piece in the middle that
//!
//! - starts the enabled spokes and keeps them running side by side
//!   ([`run_spokes`]), so one failing spoke never stops the others, and
//! - gives them a shared event bus ([`Hub`], [`HubEvent`]) to announce what
//!   they have just stored, so that other parts can react without the spokes
//!   knowing about them.
//!
//! A spoke is a [`Spoke`]: it says what it is (`id`, `kind`, `offers`) and has
//! a `run` that lasts until shutdown. The spokes themselves live in
//! [`spokes`] and are thin adapters around the existing `run` functions.
//!
//! Nothing listens to the bus yet except a debug log line. Phase 4's MQTT
//! sink (publishing to Home Assistant) will be the first real consumer.

mod bus;
pub mod spokes;

use std::future::Future;
use std::pin::Pin;
use std::task::Poll;

use chrono_tz::Tz;
use serde::Serialize;
use sqlx::SqlitePool;
use tokio::sync::watch;
use tracing::{debug, error};

pub use bus::{Hub, HubEvent};

/// What a spoke can give the dashboard. A spoke may offer several.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// Power and state of charge right now.
    Live,
    /// Past readings and day totals.
    History,
    /// The inverter's lifetime grid energy counters.
    Meter,
    /// Electricity prices.
    Price,
    /// The car's charge state, sessions and odometer.
    Car,
    /// The inverter's control settings.
    Settings,
    /// Copies of the database.
    Backup,
}

/// Whether a spoke brings data in or sends it out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SpokeKind {
    Source,
    Sink,
}

/// What a spoke is, apart from the code that runs it. Kept in a `const` so
/// the list of all spokes (for `/api/spokes`) needs no running spoke.
#[derive(Debug, Clone, Copy)]
pub struct SpokeMeta {
    /// Stable kebab-case name, used in logs, the API and (later) the config.
    pub id: &'static str,
    pub kind: SpokeKind,
    pub offers: &'static [Capability],
}

/// A background job, started by [`run_spokes`].
pub trait Spoke {
    fn meta(&self) -> &'static SpokeMeta;

    fn id(&self) -> &'static str {
        self.meta().id
    }

    fn kind(&self) -> SpokeKind {
        self.meta().kind
    }

    fn offers(&self) -> &'static [Capability] {
        self.meta().offers
    }

    /// Runs until `ctx.shutdown` fires. An `Err` is logged by [`run_spokes`]
    /// and ends only this spoke.
    ///
    /// The future is boxed by hand because traits cannot have `async fn`
    /// that is usable as `dyn Spoke` without help; this avoids adding the
    /// `async-trait` crate. `self: Box<Self>` lets the spoke move its own
    /// settings into the future.
    fn run(self: Box<Self>, ctx: SpokeContext) -> SpokeFuture;
}

/// What [`Spoke::run`] returns.
pub type SpokeFuture = Pin<Box<dyn Future<Output = anyhow::Result<()>>>>;

/// What every spoke is given when it starts.
#[derive(Clone)]
pub struct SpokeContext {
    pub pool: SqlitePool,
    /// The plant's time zone, for "today" and local midnight.
    pub tz: Tz,
    pub shutdown: Shutdown,
    pub hub: Hub,
}

/// The sending half of a shutdown signal. See [`shutdown_channel`].
pub struct ShutdownTrigger(watch::Sender<bool>);

impl ShutdownTrigger {
    pub fn fire(&self) {
        // `send_replace` works even when nobody is listening yet.
        self.0.send_replace(true);
    }
}

/// A shutdown signal that many spokes can wait on. Cloning gives another
/// listener.
#[derive(Clone)]
pub struct Shutdown(watch::Receiver<bool>);

/// Makes a trigger and the signal it fires.
pub fn shutdown_channel() -> (ShutdownTrigger, Shutdown) {
    let (tx, rx) = watch::channel(false);
    (ShutdownTrigger(tx), Shutdown(rx))
}

impl Shutdown {
    /// Completes once the signal has fired (also if it fired earlier, or if
    /// the trigger was dropped, because then it can never fire).
    pub async fn wait(mut self) {
        // `wait_for` returns at once if the value already matches.
        let _ = self.0.wait_for(|fired| *fired).await;
    }
}

/// What `/api/spokes` says about one spoke.
#[derive(Debug, Clone, Serialize)]
pub struct SpokeInfo {
    pub id: &'static str,
    pub kind: SpokeKind,
    #[serde(rename = "capabilities")]
    pub offers: &'static [Capability],
    pub enabled: bool,
}

/// Every spoke this build has, in a fixed order, with `enabled` set for the
/// ids in `enabled`.
pub fn spoke_infos(enabled: &[&str]) -> Vec<SpokeInfo> {
    spokes::ALL
        .iter()
        .map(|meta| SpokeInfo {
            id: meta.id,
            kind: meta.kind,
            offers: meta.offers,
            enabled: enabled.contains(&meta.id),
        })
        .collect()
}

/// Runs the spokes side by side until shutdown, plus the hub's own jobs (the
/// midnight clock and the debug log of events).
///
/// Everything runs on the calling task, polled together like `try_join!`
/// does, so a panic still surfaces as before. A spoke that returns an error is
/// logged with its id and then simply finished: the others, and the web
/// server, keep going.
pub async fn run_spokes(spokes: Vec<Box<dyn Spoke>>, ctx: SpokeContext) {
    let mut jobs: Vec<Option<Pin<Box<dyn Future<Output = ()>>>>> = Vec::new();
    for spoke in spokes {
        let id = spoke.id();
        debug!(spoke = id, kind = ?spoke.kind(), offers = ?spoke.offers(), "starting spoke");
        let run = spoke.run(ctx.clone());
        jobs.push(Some(Box::pin(async move {
            if let Err(err) = run.await {
                error!(
                    spoke = id,
                    "{id} stopped: {err:#}; the dashboard keeps running"
                );
            }
        })));
    }
    // Subscribe before anything runs, so the logger misses no event.
    jobs.push(Some(Box::pin(bus::log_events(
        ctx.hub.subscribe(),
        ctx.shutdown.clone(),
    ))));
    jobs.push(Some(Box::pin(bus::run_day_clock(
        ctx.hub.clone(),
        ctx.tz,
        bus::unix_now,
        ctx.shutdown.clone(),
    ))));

    // Poll every unfinished job each time something wakes us.
    std::future::poll_fn(|cx| {
        let mut pending = false;
        for slot in jobs.iter_mut() {
            if let Some(job) = slot {
                if job.as_mut().poll(cx).is_ready() {
                    *slot = None;
                } else {
                    pending = true;
                }
            }
        }
        if pending {
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    })
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    const TEST_META: SpokeMeta = SpokeMeta {
        id: "test-spoke",
        kind: SpokeKind::Source,
        offers: &[Capability::Live],
    };

    /// A spoke that either fails at once or runs until shutdown.
    struct Fake {
        fail: bool,
        stopped: Arc<AtomicBool>,
    }

    impl Spoke for Fake {
        fn meta(&self) -> &'static SpokeMeta {
            &TEST_META
        }

        fn run(self: Box<Self>, ctx: SpokeContext) -> SpokeFuture {
            Box::pin(async move {
                if self.fail {
                    anyhow::bail!("boom");
                }
                ctx.shutdown.wait().await;
                self.stopped.store(true, Ordering::SeqCst);
                Ok(())
            })
        }
    }

    async fn test_ctx() -> (SpokeContext, ShutdownTrigger) {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        let (trigger, shutdown) = shutdown_channel();
        let ctx = SpokeContext {
            pool,
            tz: chrono_tz::Australia::Adelaide,
            shutdown,
            hub: Hub::default(),
        };
        (ctx, trigger)
    }

    #[tokio::test]
    async fn spokes_run_until_shutdown_and_a_failing_one_does_not_stop_the_others() {
        let (ctx, trigger) = test_ctx().await;
        let stopped = Arc::new(AtomicBool::new(false));
        let spokes: Vec<Box<dyn Spoke>> = vec![
            Box::new(Fake {
                fail: true,
                stopped: stopped.clone(),
            }),
            Box::new(Fake {
                fail: false,
                stopped: stopped.clone(),
            }),
        ];
        let driver = async {
            // By now the failing spoke has finished; the other must still run.
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert!(!stopped.load(Ordering::SeqCst));
            trigger.fire();
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(run_spokes(spokes, ctx), driver)
        })
        .await
        .expect("run_spokes did not stop on shutdown");
        assert!(stopped.load(Ordering::SeqCst));
    }

    #[test]
    fn spoke_list_marks_the_enabled_ones() {
        let infos = spoke_infos(&["amber"]);
        assert_eq!(infos.len(), spokes::ALL.len());
        let enabled: Vec<_> = infos.iter().filter(|i| i.enabled).map(|i| i.id).collect();
        assert_eq!(enabled, ["amber"]);
    }
}
