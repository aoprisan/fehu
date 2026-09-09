//! The economy server built on [`fehu`]: seeded symbols ticking in
//! wall-clock time, OHLC bars over HTTP, an SSE tick stream, endpoints
//! through which a game pushes events into the simulation, and trading:
//! users open accounts, pay money into them in integer cents, and their
//! traders send orders that each account is validated against and settled
//! through. On top of that sits the economy: a conserved currency, goods,
//! recipes and jobs, rewards, and a command journal that makes every
//! acknowledged mutation survive a restart.
//!
//! Where [`fehu`] is the portable, `no_std` library — the price process, the
//! book and the ledger, with no clock and no I/O — this crate is the process
//! that runs one: tokio actors, HTTP, and state on disk.
//!
//! The crate is a library so the router can be exercised in tests; the binary
//! in `main.rs` wires it to a TCP listener.

pub mod account;
pub mod actor;
pub mod api;
pub mod auth;
pub mod catalog;
pub mod engine;
pub mod events;
pub mod jobs;
pub mod journal;
pub mod limit;
pub mod market;
pub mod metrics;
pub mod npc;
pub mod outbox;
pub mod reconcile;
pub mod rewards;
pub mod save;
pub mod service;
pub mod symbol;
pub mod symbols;
pub mod trading;
pub mod world;

pub use actor::Actor;
pub use api::router;
pub use market::{App, Market, Options};
