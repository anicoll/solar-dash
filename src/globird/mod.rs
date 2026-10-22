//! GloBird Energy customer portal (myaccount.globirdenergy.com.au).
//!
//! This is an unofficial, reverse-engineered API, the one the web portal's own
//! pages use (learned from the `bolagnaise/globird-ha` Home Assistant
//! integration). It can change or break without notice.
//!
//! - [`client`]: the HTTP client: login and generic JSON calls.
//! - [`probe`]: the `globird-probe` command, which dumps what each endpoint
//!   returns so we can see the real response shapes.

pub mod client;
pub mod probe;
