use crate::amber::task;
use crate::config::Amber;
use crate::hub::{Capability, Spoke, SpokeContext, SpokeFuture, SpokeKind, SpokeMeta};

pub const META: SpokeMeta = SpokeMeta {
    id: "amber",
    kind: SpokeKind::Source,
    offers: &[Capability::Price],
};

/// Fetches Amber's electricity prices. Read-only: it never changes anything
/// at Amber.
pub struct AmberPrices {
    config: Amber,
}

impl AmberPrices {
    pub fn new(config: Amber) -> Self {
        Self { config }
    }
}

impl Spoke for AmberPrices {
    fn meta(&self) -> &'static SpokeMeta {
        &META
    }

    fn run(self: Box<Self>, ctx: SpokeContext) -> SpokeFuture {
        Box::pin(
            async move { task::run(self.config, ctx.pool, &ctx.hub, ctx.shutdown.wait()).await },
        )
    }
}
