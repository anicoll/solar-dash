use crate::config::HaSource;
use crate::ha_source::{self, HaView};
use crate::hub::{Capability, Spoke, SpokeContext, SpokeFuture, SpokeKind, SpokeMeta};

pub const META: SpokeMeta = SpokeMeta {
    id: ha_source::SOURCE_NAME,
    kind: SpokeKind::Source,
    offers: &[Capability::Live, Capability::History],
};

/// Reads entity states from Home Assistant over its WebSocket API. Read-only:
/// it never calls a service or writes a state.
pub struct HomeAssistantSource {
    config: HaSource,
    /// Shared with the web API, which shows the entities' last states.
    view: HaView,
}

impl HomeAssistantSource {
    pub fn new(config: HaSource, view: HaView) -> Self {
        Self { config, view }
    }
}

impl Spoke for HomeAssistantSource {
    fn meta(&self) -> &'static SpokeMeta {
        &META
    }

    fn run(self: Box<Self>, ctx: SpokeContext) -> SpokeFuture {
        Box::pin(async move {
            ha_source::run(
                self.config,
                self.view,
                ctx.pool,
                ctx.hub,
                ctx.shutdown.wait(),
            )
            .await
        })
    }
}
