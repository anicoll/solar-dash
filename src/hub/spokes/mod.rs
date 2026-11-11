//! The spokes: one thin adapter per existing background job. Each adapter
//! only moves the job's settings into a [`Spoke`](super::Spoke) and calls the
//! job's own `run` function; nothing about how the jobs work lives here.

mod amber;
mod backups;
mod globird_portal;
mod isolarcloud;
mod sungrow_modbus;
mod teslamate_history;
mod teslamate_mqtt;

use super::{Spoke, SpokeMeta};
use crate::config::Config;
use crate::isolar::client::Client;
use crate::offsite::Offsite;
use crate::web::AppState;

/// Every spoke this build has, in the order they are listed at startup.
pub const ALL: [SpokeMeta; 7] = [
    sungrow_modbus::META,
    isolarcloud::META,
    globird_portal::META,
    teslamate_mqtt::META,
    teslamate_history::META,
    amber::META,
    backups::META,
];

/// The spokes this config turns on. A spoke with no config section is left
/// out, so it never starts. The iSolarCloud sync and the backups are always
/// on, as before (backups end at once when `backup_keep` is 0).
///
/// `client` and `offsite` are made by the caller because making them can
/// fail, and `serve` checks those settings at a fixed point at startup.
pub fn build(
    config: &Config,
    state: &AppState,
    client: Client,
    offsite: Option<Offsite>,
) -> Vec<Box<dyn Spoke>> {
    let mut spokes: Vec<Box<dyn Spoke>> = Vec::new();
    if let Some(modbus) = config.modbus.clone() {
        spokes.push(Box::new(sungrow_modbus::SungrowModbus::new(modbus, state)));
    }
    spokes.push(Box::new(isolarcloud::IsolarCloudSpoke::new(
        client,
        config.isolarcloud.clone(),
    )));
    if let Some(globird) = config.globird.clone() {
        spokes.push(Box::new(globird_portal::GlobirdPortal::new(globird)));
    }
    if let Some(teslamate) = config.teslamate.clone() {
        spokes.push(Box::new(teslamate_mqtt::TeslamateMqtt::new(
            teslamate, state,
        )));
    }
    // The history sync needs `database_url` in [teslamate]. The URL has a
    // password, so it is never logged.
    if let Some(url) = config
        .teslamate
        .as_ref()
        .and_then(|teslamate| teslamate.database_url.clone())
    {
        spokes.push(Box::new(teslamate_history::TeslamateHistory::new(
            url, state,
        )));
    }
    if let Some(amber) = config.amber.clone() {
        spokes.push(Box::new(amber::AmberPrices::new(amber)));
    }
    spokes.push(Box::new(backups::Backups::new(
        config.database.backup_dir(),
        config.database.backup_keep,
        offsite,
    )));
    spokes
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn ids_are_unique_stable_kebab_case() {
        let ids: Vec<_> = ALL.iter().map(|meta| meta.id).collect();
        assert_eq!(
            ids,
            [
                "sungrow-modbus",
                "isolarcloud",
                "globird-portal",
                "teslamate-mqtt",
                "teslamate-history",
                "amber",
                "backups"
            ]
        );
        assert_eq!(ids.iter().collect::<HashSet<_>>().len(), ids.len());
    }
}
