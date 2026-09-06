//! Sample backend for [`fehu`]: four hardcoded, seeded symbols ticking in
//! wall-clock time, OHLC bars over HTTP, an SSE tick stream, and endpoints
//! through which a game pushes events into the simulation.
//!
//! The crate is a library so the router can be exercised in tests; the binary
//! in `main.rs` wires it to a TCP listener.

pub mod api;
pub mod engine;
pub mod events;
pub mod market;

pub use api::router;
pub use market::{App, Options};
