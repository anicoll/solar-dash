use crate::config::Modbus;
use crate::hub::{Capability, Spoke, SpokeContext, SpokeFuture, SpokeKind, SpokeMeta};
use crate::modbus::{self, Latest, LatestSettings};
use crate::web::AppState;

pub const META: SpokeMeta = SpokeMeta {
    id: "sungrow-modbus",
    kind: SpokeKind::Source,
    offers: &[Capability::Live, Capability::Meter, Capability::Settings],
};

/// Reads the inverter over Modbus TCP.
pub struct SungrowModbus {
    config: Modbus,
    latest: Latest,
    latest_settings: LatestSettings,
}

impl SungrowModbus {
    pub fn new(config: Modbus, state: &AppState) -> Self {
        Self {
            config,
            latest: state.latest(),
            latest_settings: state.latest_settings(),
        }
    }
}

impl Spoke for SungrowModbus {
    fn meta(&self) -> &'static SpokeMeta {
        &META
    }

    fn run(self: Box<Self>, ctx: SpokeContext) -> SpokeFuture {
        Box::pin(async move {
            modbus::run_poll(
                self.config,
                &ctx.pool,
                self.latest,
                self.latest_settings,
                &ctx.hub,
                ctx.shutdown.wait(),
            )
            .await
        })
    }
}
