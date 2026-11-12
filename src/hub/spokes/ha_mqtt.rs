use crate::amber::store;
use crate::config::HomeAssistant;
use crate::home_assistant::{self, AmberNow, Offer, TodaySnapshot};
use crate::hub::{Spoke, SpokeContext, SpokeFuture, SpokeKind, SpokeMeta};
use crate::web::{self, AppState};

pub const META: SpokeMeta = SpokeMeta {
    id: "home-assistant-mqtt",
    kind: SpokeKind::Sink,
    // A sink brings no data in, so it offers no capability.
    offers: &[],
};

/// Publishes live readings, today's totals and (with Amber) prices to Home
/// Assistant over MQTT discovery. Publish only: it never subscribes to a
/// command topic and never controls anything.
pub struct HomeAssistantMqtt {
    config: HomeAssistant,
    state: AppState,
    /// Whether `[amber]` is configured.
    amber: bool,
}

impl HomeAssistantMqtt {
    pub fn new(config: HomeAssistant, state: &AppState, amber: bool) -> Self {
        Self {
            config,
            state: state.clone(),
            amber,
        }
    }
}

impl Spoke for HomeAssistantMqtt {
    fn meta(&self) -> &'static SpokeMeta {
        &META
    }

    fn run(self: Box<Self>, ctx: SpokeContext) -> SpokeFuture {
        // Subscribe here, before the spoke starts running, so no sample
        // sent in between is missed.
        let events = ctx.hub.subscribe();
        Box::pin(async move {
            let offer = Offer {
                amber: self.amber,
                costs: self.state.has_tariff(),
            };
            let state = self.state;
            let today = || snapshot(&state, &ctx.pool, ctx.tz, offer);
            home_assistant::run(self.config, offer, events, today, ctx.shutdown.wait()).await
        })
    }
}

/// Today's figures, from the functions behind the Today page: the day totals
/// of `/api/daily`, the cost of `/api/costs` and the stored Amber prices.
async fn snapshot(
    state: &AppState,
    pool: &sqlx::SqlitePool,
    tz: chrono_tz::Tz,
    offer: Offer,
) -> anyhow::Result<TodaySnapshot> {
    let now = chrono::Utc::now().timestamp();
    let date = home_assistant::local_date(now, tz);
    let energy = web::day_energy(state, date).await?;
    let money = if offer.costs {
        web::day_money(state, date).await?
    } else {
        None
    };
    let amber = if offer.amber {
        let rows = store::prices_at(pool, None, now).await?;
        // The newest row per channel comes first (see `prices_at`).
        let find = |channel: &str| rows.iter().find(|row| row.channel == channel);
        let general = find("general");
        Some(AmberNow {
            general_cents: general.map(|row| row.cents),
            feed_in_raw_cents: find("feed_in").map(|row| row.cents),
            descriptor: general.and_then(|row| row.descriptor.clone()),
        })
    } else {
        None
    };
    Ok(TodaySnapshot {
        date,
        energy,
        money,
        amber,
    })
}
