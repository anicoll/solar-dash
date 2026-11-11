//! Amber Electric (api.amber.com.au): the official, documented price API.
//!
//! - [`models`]: the JSON shapes the API returns (sites, prices, usage).
//! - [`client`]: the HTTP client, read-only (it only ever sends GET requests).
//! - [`probe`]: the `amber-probe` command, which prints what the API returns
//!   so we can check it against our own meter before building on it.
//!
//! - [`store`]: saves prices in the neutral `price_interval` table.
//! - [`task`]: the task inside `serve` that fetches prices every 5 minutes.
//!
//! Some model fields are never read yet, so the dead-code warning is silenced
//! for the whole module.
#![allow(dead_code)]

pub mod client;
pub mod models;
pub mod probe;
pub mod store;
pub mod task;
