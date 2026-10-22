//! GloBird Energy customer portal (myaccount.globirdenergy.com.au).
//!
//! This is an unofficial, reverse-engineered API, the one the web portal's own
//! pages use (learned from the `bolagnaise/globird-ha` Home Assistant
//! integration). It can change or break without notice.
//!
//! - [`client`]: the HTTP client: login and generic JSON calls.
//! - [`probe`]: the `globird-probe` command, which dumps what each endpoint
//!   returns so we can see the real response shapes.
//! - [`service`]: picking the power service and its meter, shared by the probe
//!   and the sync.
//! - [`data`]: cost and usage rows, and grouping them into one record per day.
//! - [`sync`]: the background task in `serve` that keeps the database up to
//!   date with the portal.

pub mod client;
pub mod data;
pub mod probe;
pub mod service;
pub mod sync;
