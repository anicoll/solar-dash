//! Amber Electric (api.amber.com.au): the official, documented price API.
//!
//! - [`models`]: the JSON shapes the API returns (sites, prices, usage).
//! - [`client`]: the HTTP client, read-only (it only ever sends GET requests).
//! - [`probe`]: the `amber-probe` command, which prints what the API returns
//!   so we can check it against our own meter before building on it.
//!
//! Nothing here runs inside `serve` yet; later changes will use the client to
//! keep prices in the database. Until then some fields are never read, so the
//! dead-code warning is silenced for the whole module.
#![allow(dead_code)]

pub mod client;
pub mod models;
pub mod probe;
