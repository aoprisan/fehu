//! Sample backend for [`fehu`]: seeded symbols ticking in
//! wall-clock time, OHLC bars over HTTP, an SSE tick stream, endpoints
//! through which a game pushes events into the simulation, and trading:
//! users open accounts, pay money into them in integer cents, and their
//! traders send orders that each account is validated against and settled
//! through.
//!
//! The crate is a library so the router can be exercised in tests; the binary
//! in `main.rs` wires it to a TCP listener.

pub mod account;
pub mod actor;
pub mod api;
pub mod auth;
pub mod engine;
pub mod events;
pub mod limit;
pub mod market;
pub mod metrics;
pub mod reconcile;
pub mod save;
pub mod symbol;
pub mod symbols;
pub mod trading;

pub use actor::Actor;
pub use api::router;
pub use market::{App, Market, Options};
