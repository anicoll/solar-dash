//! Amber Electric's public API (api.amber.com.au).
//!
//! This is a documented, token-based REST API (docs at
//! https://app.amber.com.au/developers).
//!
//! - [`client`]: the HTTP client and the lenient types its replies parse into.
//! - [`probe`]: the `amber-probe` command, which checks what the API returns
//!   and how much history it will give us.

pub mod client;
pub mod probe;
