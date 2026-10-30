//! The fuel API: the manual cars (the ones TeslaMate does not track) with
//! their petrol fills and what `fuel_log` works out from them. The fills are
//! edited through the admin routes (see `admin.rs`).

use std::collections::HashMap;

use axum::Json;
use axum::extract::State;
use serde_json::{Value, json};

use super::AppState;
use super::api::ApiResult;
use crate::db;
use crate::fuel_log;

/// `GET /api/fuel/cars`: every manual car, oldest first, even with no fills:
/// `{"cars":[{"car_id","source":"manual","name","model","kind","label",
/// "owned_since","totals":{..},"l_per_100km","cost_per_km","months":[..],
/// "fills":[..]}]}`. `label` is the model, else the name. `owned_since` is the
/// day the car was bought (YYYY-MM-DD), or null. `fills` has the newest first, each
/// with the price paid, the fuel economy of the full-to-full stretch that
/// ends there, the km since the last odometer reading and the month's
/// average petrol price (`avg_cents_per_litre`).
pub async fn cars(State(state): State<AppState>) -> ApiResult {
    let data = state.admin.get();
    let fills = db::fuel_fills(&state.pool).await?;
    let mut cars = Vec::new();
    for car in db::manual_cars(&state.pool).await? {
        let own: Vec<db::FuelFill> = fills
            .iter()
            .filter(|f| f.car_id == car.id)
            .cloned()
            .collect();
        let summary = fuel_log::summarise(&own, &data.petrol);
        let by_id: HashMap<i64, &db::FuelFill> = own.iter().map(|f| (f.id, f)).collect();
        // `summary.fills` is oldest first (date, odometer, id).
        let list: Vec<Value> = summary
            .fills
            .iter()
            .rev()
            .map(|detail| {
                let fill = by_id[&detail.id];
                json!({
                    "id": fill.id,
                    "date": fill.date,
                    "litres": fill.litres,
                    "cost_dollars": fill.cost_dollars,
                    "odometer_km": fill.odometer_km,
                    "full_tank": fill.full_tank,
                    "note": fill.note,
                    "cents_per_litre": detail.cents_per_litre,
                    "l_per_100km": detail.l_per_100km,
                    "km_since_last": detail.km_since_last,
                    "avg_cents_per_litre": detail.avg_cents_per_litre,
                })
            })
            .collect();
        let label = car
            .model
            .clone()
            .filter(|m| !m.trim().is_empty())
            .unwrap_or_else(|| car.name.clone());
        cars.push(json!({
            "car_id": car.id,
            "source": "manual",
            "name": car.name,
            "model": car.model,
            "kind": car.kind,
            "label": label,
            "owned_since": car.owned_since,
            "totals": summary.totals,
            "l_per_100km": summary.l_per_100km,
            "cost_per_km": summary.cost_per_km,
            "months": summary.months,
            "fills": list,
        }));
    }
    Ok(Json(json!({ "cars": cars })))
}
