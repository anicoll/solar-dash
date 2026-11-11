use crate::config::IsolarCloud;
use crate::hub::{Capability, Spoke, SpokeContext, SpokeFuture, SpokeKind, SpokeMeta};
use crate::ingest;
use crate::isolar::client::Client;

pub const META: SpokeMeta = SpokeMeta {
    id: "isolarcloud",
    kind: SpokeKind::Source,
    offers: &[Capability::Live, Capability::History],
};

/// The iSolarCloud live sync and background backfill. It is not optional:
/// `[isolarcloud]` is required config.
pub struct IsolarCloudSpoke {
    client: Client,
    config: IsolarCloud,
}

impl IsolarCloudSpoke {
    pub fn new(client: Client, config: IsolarCloud) -> Self {
        Self { client, config }
    }
}

impl Spoke for IsolarCloudSpoke {
    fn meta(&self) -> &'static SpokeMeta {
        &META
    }

    fn run(self: Box<Self>, ctx: SpokeContext) -> SpokeFuture {
        Box::pin(async move {
            ingest::run_live_sync(
                self.client,
                &ctx.pool,
                &self.config,
                &ctx.hub,
                ctx.shutdown.wait(),
            )
            .await
        })
    }
}
