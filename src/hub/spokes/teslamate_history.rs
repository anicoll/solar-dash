use crate::ev_cost::CostSetup;
use crate::hub::{Capability, Spoke, SpokeContext, SpokeFuture, SpokeKind, SpokeMeta};
use crate::teslamate_history;
use crate::web::AppState;

pub const META: SpokeMeta = SpokeMeta {
    id: "teslamate-history",
    kind: SpokeKind::Source,
    offers: &[Capability::Car, Capability::History],
};

/// Copies past drives and charges out of TeslaMate's database.
pub struct TeslamateHistory {
    /// Has a password in it: never log it.
    database_url: String,
    costing: CostSetup,
}

impl TeslamateHistory {
    pub fn new(database_url: String, state: &AppState) -> Self {
        Self {
            database_url,
            costing: state.cost_setup(),
        }
    }
}

impl Spoke for TeslamateHistory {
    fn meta(&self) -> &'static SpokeMeta {
        &META
    }

    fn run(self: Box<Self>, ctx: SpokeContext) -> SpokeFuture {
        Box::pin(async move {
            teslamate_history::run(
                self.database_url,
                ctx.pool,
                ctx.tz,
                self.costing,
                ctx.shutdown.wait(),
            )
            .await
        })
    }
}
