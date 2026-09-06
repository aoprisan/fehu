//! See the README below and `DESIGN.md` in the repository for the model,
//! equations and parameter ranges.
#![doc = include_str!("../README.md")]
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
pub use sim::{LoadError, STATE_VERSION, Simulator, SimulatorRepr, Snapshot, Tick};
pub use time::{MarketHours, Timestamp};
