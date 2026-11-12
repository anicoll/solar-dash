use crate::config::HomeAssistant;
use crate::home_assistant;
use crate::hub::{Spoke, SpokeContext, SpokeFuture, SpokeKind, SpokeMeta};

pub const META: SpokeMeta = SpokeMeta {
    id: "home-assistant-mqtt",
    kind: SpokeKind::Sink,
    // A sink brings no data in, so it offers no capability.
    offers: &[],
};

/// Publishes live readings to Home Assistant over MQTT discovery. Publish
/// only: it never subscribes to a command topic and never controls anything.
pub struct HomeAssistantMqtt {
    config: HomeAssistant,
}

impl HomeAssistantMqtt {
    pub fn new(config: HomeAssistant) -> Self {
        Self { config }
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
        Box::pin(async move { home_assistant::run(self.config, events, ctx.shutdown.wait()).await })
    }
}
