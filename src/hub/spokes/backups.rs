use std::path::PathBuf;

use crate::backup;
use crate::hub::{Capability, Spoke, SpokeContext, SpokeFuture, SpokeKind, SpokeMeta};
use crate::offsite::Offsite;

pub const META: SpokeMeta = SpokeMeta {
    id: "backups",
    kind: SpokeKind::Sink,
    offers: &[Capability::Backup],
};

/// Nightly database backups, with an optional offsite copy.
pub struct Backups {
    dir: PathBuf,
    keep: usize,
    offsite: Option<Offsite>,
}

impl Backups {
    pub fn new(dir: PathBuf, keep: usize, offsite: Option<Offsite>) -> Self {
        Self { dir, keep, offsite }
    }
}

impl Spoke for Backups {
    fn meta(&self) -> &'static SpokeMeta {
        &META
    }

    fn run(self: Box<Self>, ctx: SpokeContext) -> SpokeFuture {
        Box::pin(async move {
            backup::run(
                ctx.pool,
                self.dir,
                self.keep,
                ctx.tz,
                self.offsite,
                ctx.shutdown.wait(),
            )
            .await
        })
    }
}
