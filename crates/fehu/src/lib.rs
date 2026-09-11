//! See the README below, and the module docs of `sim` and `exchange` for the
//! model, equations and parameter ranges.
#![doc = include_str!("../README.md")]
#![no_std]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

extern crate alloc;
#[cfg(feature = "std")]
extern crate std;

mod book;
mod candles;
mod config;
mod event;
mod exchange;
pub mod ledger;
mod math;
mod sim;
mod time;

pub use book::{
    CancelError, Level, MAX_ORDER_QTY, MAX_PRICE_CENTS, MarketRules, Order, OrderBook, OrderError,
    OrderId, OrderKind, OrderStatus, Owner, Party, Placement, Preview, Resting, Side, TimeInForce,
    Trade, TraderId,
};
pub use candles::{Candle, Candles, Interval};
pub use config::{Config, ConfigError, GarchParams, JumpParams, VolumeParams};
pub use event::{Event, EventError, EventKind};
pub use exchange::{
    EXCHANGE_VERSION, Exchange, ExchangeRepr, FlowParams, ImpactParams, LiquidityParams, Position,
    StepReport, TradingParams,
};
// `ledger` is the one module exported whole rather than flattened: its
// fifteen types read as one namespace (`ledger::Draft`, `ledger::Reason`),
// and names that general belong behind it rather than beside `Order` and
// `Tick`. The two a caller needs before they need the rest are here.
pub use ledger::{Ledger, MAX_WALLET_CENTS};
pub use sim::{LoadError, STATE_VERSION, Simulator, SimulatorRepr, Snapshot, Tick};
pub use time::{MarketHours, Timestamp};
