use crate::config::Teslamate;
use crate::hub::{Capability, Spoke, SpokeContext, SpokeFuture, SpokeKind, SpokeMeta};
use crate::teslamate::{self, Car};
use crate::web::AppState;

pub const META: SpokeMeta = SpokeMeta {
    id: "teslamate-mqtt",
    kind: SpokeKind::Source,
    offers: &[Capability::Car],
};

/// Follows the car's charge state over TeslaMate's MQTT broker, and saves
/// charging sessions and the odometer.
pub struct TeslamateMqtt {
    config: Teslamate,
    car: Car,
}

impl TeslamateMqtt {
    pub fn new(config: Teslamate, state: &AppState) -> Self {
        Self {
            config,
            car: state.car(),
        }
    }
}

impl Spoke for TeslamateMqtt {
    fn meta(&self) -> &'static SpokeMeta {
        &META
    }

    fn run(self: Box<Self>, ctx: SpokeContext) -> SpokeFuture {
        Box::pin(async move {
            teslamate::run(self.config, self.car, ctx.pool, ctx.tz, ctx.shutdown.wait()).await
        })
    }
}
