use std::sync::Arc;

use tokio::sync::Notify;

use crate::config::Globird;
use crate::globird::sync;
use crate::hub::{Capability, Spoke, SpokeContext, SpokeFuture, SpokeKind, SpokeMeta};
use crate::web::AppState;

pub const META: SpokeMeta = SpokeMeta {
    id: "globird-portal",
    kind: SpokeKind::Source,
    offers: &[Capability::History],
};

/// The daily GloBird usage and cost sync.
pub struct GlobirdPortal {
    config: Globird,
    synced: Arc<Notify>,
}

impl GlobirdPortal {
    pub fn new(config: Globird, state: &AppState) -> Self {
        Self {
            config,
            synced: state.warm_signal(),
        }
    }
}

impl Spoke for GlobirdPortal {
    fn meta(&self) -> &'static SpokeMeta {
        &META
    }

    fn run(self: Box<Self>, ctx: SpokeContext) -> SpokeFuture {
        Box::pin(async move {
            sync::run(
                self.config,
                ctx.pool,
                ctx.tz,
                Some(self.synced),
                ctx.shutdown.wait(),
            )
            .await
        })
    }
}
