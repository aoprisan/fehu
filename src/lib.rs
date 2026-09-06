//! `fehu` — a deterministic synthetic stock price simulator for games.
//!
//! A slow-moving *fundamental value* is driven only by external inputs; the
//! *price* mean-reverts toward it (Ornstein–Uhlenbeck on the log-spread) with
//! GARCH(1,1) volatility clustering and Poisson jumps. Everything is `no_std +
//! alloc`, uses `libm` for transcendental math and a caller-seeded
//! `xoshiro256++` RNG, so the same seed and events give bit-identical output on
//! every platform.
//!
//! See `DESIGN.md` in the repository for the model, equations and parameter
//! ranges.
//!
//! ```
//! use fehu::{Config, Simulator};
//!
//! let mut sim = Simulator::new(Config::default(), 42).unwrap();
//! let tick = sim.step();
//! assert!(tick.price_cents > 0);
//! ```
#![no_std]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

extern crate alloc;
#[cfg(feature = "std")]
extern crate std;

mod candles;
mod config;
mod event;
mod math;
mod sim;
mod time;

pub use candles::{Candle, Candles, Interval};
pub use config::{Config, ConfigError, GarchParams, JumpParams, VolumeParams};
pub use event::{Event, EventError, EventKind};
pub use sim::{STATE_VERSION, Simulator, Snapshot, Tick};
pub use time::{MarketHours, Timestamp};
