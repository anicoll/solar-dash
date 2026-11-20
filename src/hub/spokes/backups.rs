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
    min_free_mb: u64,
    offsite: Option<Offsite>,
}

impl Backups {
    pub fn new(dir: PathBuf, keep: usize, min_free_mb: u64, offsite: Option<Offsite>) -> Self {
        Self {
            dir,
            keep,
            min_free_mb,
            offsite,
        }
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
                self.min_free_mb,
                ctx.tz,
                self.offsite,
                ctx.shutdown.wait(),
            )
            .await
        })
    }
}
