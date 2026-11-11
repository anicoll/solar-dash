use crate::config::Globird;
use crate::globird::sync;
use crate::hub::{Capability, Spoke, SpokeContext, SpokeFuture, SpokeKind, SpokeMeta};

pub const META: SpokeMeta = SpokeMeta {
    id: "globird-portal",
    kind: SpokeKind::Source,
    offers: &[Capability::History],
};

/// The daily GloBird usage and cost sync.
pub struct GlobirdPortal {
    config: Globird,
}

impl GlobirdPortal {
    pub fn new(config: Globird) -> Self {
        Self { config }
    }
}

impl Spoke for GlobirdPortal {
    fn meta(&self) -> &'static SpokeMeta {
        &META
    }

    fn run(self: Box<Self>, ctx: SpokeContext) -> SpokeFuture {
        Box::pin(async move { sync::run(self.config, ctx.pool, ctx.tz, ctx.shutdown.wait()).await })
    }
}
