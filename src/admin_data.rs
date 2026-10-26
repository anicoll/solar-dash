//! The data the admin page edits (tariffs, petrol prices, purchases, bills and
//! a few preferences), kept in one snapshot that can be swapped while the
//! server runs. Handlers take the current snapshot once per request.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use anyhow::Result;

use crate::bills::PastBill;
use crate::config::Config;
use crate::cost::TariffSchedule;
use crate::ev_cost::EvPrices;
use crate::petrol_prices::PetrolPrices;
use crate::web::{Purchase, purchases_from_config};

/// The data the admin page edits, as the rest of the app uses it.
#[derive(Debug, Clone)]
pub struct AdminData {
    /// The plans over time; `None` turns the cost features off.
    pub tariff: Option<Arc<TariffSchedule>>,
    /// The petrol and away-from-home prices.
    pub ev_prices: EvPrices,
    /// What petrol cost on each day.
    pub petrol: PetrolPrices,
    /// The petrol car each EV model is compared with.
    pub compare_cars: BTreeMap<String, String>,
    pub petrol_grade: String,
    /// Oldest first; empty turns the payback API off.
    pub purchases: Arc<[Purchase]>,
    /// Assumed yearly electricity price rise (percent) in the payback projection.
    pub price_rise_percent: f64,
    /// The past bills, oldest first; empty when there are none.
    pub bills: Arc<[PastBill]>,
}

impl Default for AdminData {
    /// Nothing configured: cost features off, with the usual defaults.
    fn default() -> Self {
        Self {
            tariff: None,
            ev_prices: EvPrices::default(),
            petrol: PetrolPrices::default(),
            compare_cars: BTreeMap::new(),
            petrol_grade: "Unleaded 91".to_string(),
            purchases: Arc::from([]),
            price_rise_percent: 3.0,
            bills: Arc::from([]),
        }
    }
}

impl AdminData {
    /// Builds the data from `config.toml`, with clear errors for bad files.
    pub fn from_config(config: &Config) -> Result<Self> {
        let tariff = config
            .tariff
            .as_ref()
            .map(|current| TariffSchedule::from_config(current, &config.previous_tariff))
            .transpose()?
            .map(Arc::new);
        let purchases = purchases_from_config(&config.system.purchase)?;
        // A bad bills file stops `serve` here, with the line in the message.
        let bills = match &config.bills {
            Some(section) => crate::bills::load(std::path::Path::new(&section.file))?,
            None => Vec::new(),
        };
        let ev_prices = config
            .teslamate
            .as_ref()
            .map(|t| EvPrices {
                petrol_cents_per_litre: t.petrol_cents_per_litre,
                petrol_litres_per_100km: t.petrol_litres_per_100km,
                away_cents_per_kwh: t.away_cents_per_kwh,
            })
            .unwrap_or_default();
        // A bad petrol prices file stops `serve` here, with the line in the message.
        let petrol = match &config.teslamate {
            Some(t) => match &t.petrol_prices_file {
                Some(file) => {
                    PetrolPrices::open(t.petrol_cents_per_litre, std::path::Path::new(file))?
                }
                None => {
                    let changes = t
                        .petrol_price
                        .iter()
                        .map(|p| Ok((crate::config::parse_date(&p.from)?, p.cents_per_litre)))
                        .collect::<Result<Vec<_>>>()?;
                    PetrolPrices::fixed(t.petrol_cents_per_litre, changes)
                }
            },
            None => PetrolPrices::default(),
        };
        let mut data = Self {
            tariff,
            ev_prices,
            petrol,
            purchases: purchases.into(),
            bills: bills.into(),
            price_rise_percent: config.system.price_rise_percent,
            ..Self::default()
        };
        if let Some(t) = &config.teslamate {
            data.compare_cars = t.compare_cars.clone();
            data.petrol_grade = t.petrol_grade.clone();
        }
        Ok(data)
    }
}

/// A shared, swappable handle to the current [`AdminData`]. Cloning shares
/// the same slot.
#[derive(Clone)]
pub struct SharedAdminData(Arc<RwLock<Arc<AdminData>>>);

impl SharedAdminData {
    pub fn new(data: AdminData) -> Self {
        Self(Arc::new(RwLock::new(Arc::new(data))))
    }

    /// The current data. Cheap: the lock is only held to clone the `Arc`, and
    /// the snapshot stays the same for as long as the caller keeps it.
    pub fn get(&self) -> Arc<AdminData> {
        // A poisoned lock still holds a whole `Arc`, so keep using it.
        self.0.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Swaps in new data; requests already running keep the old snapshot.
    pub fn set(&self, data: AdminData) {
        *self.0.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(data);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_swaps_for_every_clone_but_not_for_old_snapshots() {
        let shared = SharedAdminData::new(AdminData::default());
        let other = shared.clone();
        let before = shared.get();
        other.set(AdminData {
            price_rise_percent: 5.0,
            ..AdminData::default()
        });
        assert_eq!(shared.get().price_rise_percent, 5.0);
        assert_eq!(before.price_rise_percent, 3.0);
    }

    #[test]
    fn config_without_optional_sections_gives_the_defaults() {
        let config: Config = toml::from_str(
            "[isolarcloud]\nappkey = \"k\"\naccess_key = \"s\"\nrsa_public_key = \"r\"\n\
             username = \"u\"\npassword = \"p\"\n",
        )
        .unwrap();
        let data = AdminData::from_config(&config).unwrap();
        assert!(data.tariff.is_none());
        assert_eq!(data.petrol_grade, "Unleaded 91");
        assert!(data.purchases.is_empty() && data.bills.is_empty());
    }
}
