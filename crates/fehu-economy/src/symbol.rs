//! One listed symbol: its metadata, its exchange (simulator plus order
//! book), its bars, its tape, its halt and its stops.
//!
//! A [`SymbolState`] is owned by one actor task ([`crate::actor::Actor`])
//! and touched only through jobs sent to it. Everything here is synchronous
//! and self-contained: a symbol never waits on anything, and it never sees
//! another symbol or the accounts. Reads — quotes, bars, the book, the tape
//! — may be sent by anyone; anything that changes the book is sent only by
//! the market actor ([`crate::market::Market`]), which is what keeps the
//! books and the money in step. See [`crate::market`] for the rules.

use std::collections::VecDeque;
use std::time::Duration;

use fehu::{
    Candle, Candles, Config, Exchange, Interval, JumpParams, LiquidityParams, MarketHours, Order,
    OrderKind, Placement, Resting, Side, Snapshot, Tick, Timestamp, Trade, TraderId, TradingParams,
    VolumeParams,
};
use serde::{Deserialize, Serialize};

use crate::account::notional_cents;
use crate::market::{HaltPolicy, MAX_STREAM_TRADES, STREAM_BOOK_DEPTH, StreamMessage};
use crate::save::{Symbol, SymbolSave};
use crate::trading::{BookDto, StopOrder, StopRequest, TradeDto};

/// Milliseconds in one day.
pub const DAY_MS: i64 = 86_400_000;

/// The longest unit name a good may be measured in.
pub const MAX_UNIT_LEN: usize = 16;

/// What a listing *is*, and where its units come from.
///
/// Both kinds are traded through the same book, held in the same
/// [`Trader::positions`](crate::trading::Trader::positions) and reserved the
/// same way; the kind says only how many units exist and what may be done to
/// them besides trading. Every rule that follows from it is a refusal
/// somewhere: a good has no dividend and no buyout, a stock is not bought
/// from a catalogue and not consumed.
///
/// The audit is the same sentence for both — what traders hold plus what
/// resting bids speak for may not exceed the units in existence — over two
/// different counts of what exists.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AssetKind {
    /// A company. Its shares are fixed at the flotation: nothing creates or
    /// destroys them, so what the traders hold plus what is still out in the
    /// market adds up to `shares_outstanding` for as long as it is listed.
    /// It pays dividends and can be delisted with a buyout.
    Stock {
        /// Shares in existence.
        shares_outstanding: u64,
    },
    /// A thing. Its units are produced and consumed, so its supply is
    /// `issued − consumed` and moves — but only ever through a command that
    /// says which: nothing else creates a unit of a good, which is why a
    /// good is quoted without synthetic liquidity
    /// ([`TradingParams::synthetic`](fehu::TradingParams)).
    Good {
        /// Units brought into the world since it was listed.
        issued: u64,
        /// Units destroyed by consuming them.
        consumed: u64,
        /// What one unit is, for display: `kg`, `crate`, `ingot`.
        unit: String,
    },
}

impl AssetKind {
    /// A stock with `shares` shares.
    #[must_use]
    pub fn stock(shares: u64) -> Self {
        Self::Stock {
            shares_outstanding: shares,
        }
    }

    /// A good measured in `unit`, with nothing issued yet.
    #[must_use]
    pub fn good(unit: impl Into<String>) -> Self {
        Self::Good {
            issued: 0,
            consumed: 0,
            unit: unit.into(),
        }
    }

    /// A short, stable name for JSON and for logs.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Stock { .. } => "stock",
            Self::Good { .. } => "good",
        }
    }

    /// Units in existence: the shares of a stock, or what has been issued
    /// less what has been consumed of a good.
    #[must_use]
    pub fn units_outstanding(&self) -> u64 {
        match self {
            Self::Stock { shares_outstanding } => *shares_outstanding,
            Self::Good {
                issued, consumed, ..
            } => issued.saturating_sub(*consumed),
        }
    }

    /// What one unit is called, for a good.
    #[must_use]
    pub fn unit(&self) -> Option<&str> {
        match self {
            Self::Stock { .. } => None,
            Self::Good { unit, .. } => Some(unit),
        }
    }

    /// This is a good: it is produced and consumed rather than floated.
    #[must_use]
    pub fn is_good(&self) -> bool {
        matches!(self, Self::Good { .. })
    }
}

/// What a listing is, apart from its price process: who it claims to be, what
/// kind of thing it is, and how many units of it exist.
///
/// This travels in the save file. It used to come from the build — the four
/// literals below — but a symbol listed while the server runs has no build to
/// come from, so the file carries the listing itself and the build only
/// supplies the ones a fresh market starts with.
#[derive(Clone, Debug, Serialize, Deserialize)]
// `symbol` is registered on the way in rather than borrowed from the input,
// so the derive needs no `'de: 'static`.
#[serde(bound(deserialize = ""))]
pub struct SymbolInfo {
    /// Ticker, e.g. `ACME`. Spelled as [`Symbol`] rather than `&'static str`
    /// so `serde` does not read it as data borrowed from the input.
    #[serde(with = "crate::save::symbol")]
    pub symbol: Symbol,
    /// Company name, or what the good is called.
    pub name: String,
    /// Sector label.
    pub sector: String,
    /// One-line flavour text.
    pub description: String,
    /// A company's shares, or a good's issued and consumed units. What the
    /// traders hold plus what is still out in the market adds up to
    /// [`AssetKind::units_outstanding`], so a buy cannot ask for more than
    /// is left.
    pub asset: AssetKind,
    /// RNG seed. Same seed + same events ⇒ same prices, every run.
    pub seed: u64,
}

impl SymbolInfo {
    /// Units in existence. See [`AssetKind::units_outstanding`].
    #[must_use]
    pub fn units_outstanding(&self) -> u64 {
        self.asset.units_outstanding()
    }

    /// This listing is a good.
    #[must_use]
    pub fn is_good(&self) -> bool {
        self.asset.is_good()
    }

    /// Bring `qty` units of a good into the world. Refuses a stock, whose
    /// shares are fixed, and refuses to overflow the count.
    ///
    /// # Errors
    /// A message naming why nothing was issued.
    pub fn issue(&mut self, qty: u64) -> Result<u64, String> {
        match &mut self.asset {
            AssetKind::Stock { .. } => Err(format!(
                "{} is a stock: its shares are fixed at the flotation",
                self.symbol
            )),
            AssetKind::Good {
                issued, consumed, ..
            } => {
                let next = issued
                    .checked_add(qty)
                    .ok_or_else(|| format!("{} cannot issue that many units", self.symbol))?;
                *issued = next;
                Ok(next.saturating_sub(*consumed))
            }
        }
    }

    /// Destroy `qty` units of a good. The caller has already taken them off
    /// a holder; this is the world's count of what is left.
    ///
    /// # Errors
    /// A message naming why nothing was consumed.
    pub fn consume(&mut self, qty: u64) -> Result<u64, String> {
        match &mut self.asset {
            AssetKind::Stock { .. } => Err(format!(
                "{} is a stock: shares are sold, not consumed",
                self.symbol
            )),
            AssetKind::Good {
                issued, consumed, ..
            } => {
                let next = consumed
                    .checked_add(qty)
                    .filter(|next| *next <= *issued)
                    .ok_or_else(|| {
                        format!(
                            "{} has only {} units to consume",
                            self.symbol,
                            issued.saturating_sub(*consumed)
                        )
                    })?;
                *consumed = next;
                Ok(issued.saturating_sub(next))
            }
        }
    }
}

/// A symbol's metadata plus the simulator config and trading parameters it
/// is created with. This is what both a seeded symbol and one listed at
/// runtime are built from.
pub struct SymbolSpec {
    pub info: SymbolInfo,
    pub config: Config,
    pub trading: TradingParams,
}

/// The tickers a fresh market starts with, in listing order.
///
/// These are the *seeded* symbols, not the symbol set: listings are added and
/// removed while the server runs (`POST /api/symbols`,
/// `DELETE /api/symbols/{symbol}`), and a restored market lists whatever its
/// save file lists rather than whatever this array says.
pub const TICKERS: [&str; 4] = ["ACME", "NBLA", "HLIO", "PXCO"];

/// `ticker` as the `&'static str` the server uses for it, matched
/// case-insensitively, or `None` if this process has never registered it.
///
/// A lookup, never a registration: see [`crate::symbols`].
pub fn intern(ticker: &str) -> Option<&'static str> {
    crate::symbols::lookup(ticker)
}

/// Register `ticker` and give back the one string the whole server will use
/// for it. Panics only if the ticker is malformed, which the literals below
/// are not.
fn seeded(ticker: &str) -> &'static str {
    crate::symbols::register(ticker).expect("seeded tickers are well formed")
}

/// The four symbols a fresh market is seeded with. `start_ts` is the first
/// tick's timestamp; the rest of the config is per symbol and deliberately
/// varied so the charts look different from one another.
pub fn seeded_symbols(start_ts: Timestamp) -> Vec<SymbolSpec> {
    let base = |start_price_cents: i64, drift: f64, volatility: f64| Config {
        start_price_cents,
        drift,
        volatility,
        start_ts,
        ..Config::default()
    };
    vec![
        SymbolSpec {
            info: SymbolInfo {
                symbol: seeded("ACME"),
                name: "Acme Industrial".into(),
                sector: "Industrials".into(),
                description: "Century-old conglomerate. Low volatility, steady drift, rare jumps."
                    .into(),
                asset: AssetKind::stock(240_000_000),
                seed: 0xACE,
            },
            config: Config {
                jumps: JumpParams {
                    intensity: 40.0,
                    mean: -0.004,
                    std: 0.02,
                },
                volume: VolumeParams {
                    base_per_day: 2_500_000.0,
                    ..VolumeParams::default()
                },
                ..base(8_420, 0.04, 0.22)
            },
            trading: TradingParams::default(),
        },
        SymbolSpec {
            info: SymbolInfo {
                symbol: seeded("NBLA"),
                name: "Nebula Robotics".into(),
                sector: "Technology".into(),
                description:
                    "Pre-profit robotics darling. High volatility, big drift, frequent jumps."
                        .into(),
                asset: AssetKind::stock(85_000_000),
                seed: 0x4E42,
            },
            config: Config {
                jumps: JumpParams {
                    intensity: 200.0,
                    mean: -0.006,
                    std: 0.05,
                },
                volume: VolumeParams {
                    base_per_day: 900_000.0,
                    return_sensitivity: 3.0,
                    ..VolumeParams::default()
                },
                ..base(31_255, 0.15, 0.60)
            },
            // Thin book, wide spread: a market order moves it.
            trading: TradingParams {
                liquidity: LiquidityParams {
                    half_spread: 0.0010,
                    level_step: 0.0010,
                    touch_depth: 0.0005,
                    ..LiquidityParams::default()
                },
                ..TradingParams::default()
            },
        },
        SymbolSpec {
            info: SymbolInfo {
                symbol: seeded("HLIO"),
                name: "Helio Energy".into(),
                sector: "Energy".into(),
                description: "Solar and storage utility. Commodity-driven, moderate volatility."
                    .into(),
                asset: AssetKind::stock(610_000_000),
                seed: 0x4845,
            },
            config: Config {
                jumps: JumpParams {
                    intensity: 80.0,
                    mean: -0.01,
                    std: 0.035,
                },
                volume: VolumeParams {
                    base_per_day: 4_000_000.0,
                    ..VolumeParams::default()
                },
                ..base(2_310, 0.02, 0.42)
            },
            trading: TradingParams::default(),
        },
        SymbolSpec {
            info: SymbolInfo {
                symbol: seeded("PXCO"),
                name: "Pax Consumer Co".into(),
                sector: "Consumer Staples".into(),
                description: "Household brands. Defensive: low volatility, shocks fade slowly."
                    .into(),
                asset: AssetKind::stock(150_000_000),
                seed: 0x5058,
            },
            config: Config {
                mean_reversion_speed: 20.0,
                jumps: JumpParams {
                    intensity: 25.0,
                    mean: -0.003,
                    std: 0.015,
                },
                volume: VolumeParams {
                    base_per_day: 1_500_000.0,
                    ..VolumeParams::default()
                },
                ..base(5_780, 0.05, 0.16)
            },
            // Deep book: hard to move.
            trading: TradingParams {
                liquidity: LiquidityParams {
                    touch_depth: 0.003,
                    depth_growth: 1.5,
                    ..LiquidityParams::default()
                },
                ..TradingParams::default()
            },
        },
    ]
}

/// Why trading in a symbol stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HaltReason {
    /// The price left the band the day opened with: a limit move.
    LimitMove,
    /// The game master stopped it, and only they can start it again.
    Manual,
}

impl HaltReason {
    pub fn label(self) -> &'static str {
        match self {
            Self::LimitMove => "limit move",
            Self::Manual => "halted by the game master",
        }
    }
}

/// Trading in one symbol, stopped.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Halt {
    pub reason: HaltReason,
    /// Simulated time trading stopped.
    pub since_ms: i64,
    /// When an automatic halt lifts by itself. A manual one has no end: it
    /// stands until the game master resumes the symbol.
    pub until_ms: Option<i64>,
    /// The band the price left, and the price that left it.
    pub band_cents: i64,
    pub price_cents: i64,
    /// How far the price had moved from the band, as a fraction.
    pub move_pct: f64,
}

/// Why an order cannot be sent for a symbol right now.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Closed {
    /// Outside the trading session.
    Session {
        /// When the next session opens.
        next_open_ms: i64,
    },
    /// Trading is halted.
    Halted(Halt),
}

impl std::fmt::Display for Closed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Session { next_open_ms } => write!(
                f,
                "the market is closed; the next session opens at {next_open_ms}                  (milliseconds since the Unix epoch, simulated time)"
            ),
            Self::Halted(halt) => {
                write!(f, "trading is halted ({})", halt.reason.label())?;
                if let Some(until) = halt.until_ms {
                    write!(f, " until {until}")?;
                }
                Ok(())
            }
        }
    }
}

/// Whether a symbol can be traded right now, and if not, why not.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct SymbolStatus {
    pub symbol: &'static str,
    /// Simulated time this was asked.
    pub ts_ms: i64,
    /// A session is running (always, with no trading calendar).
    pub market_open: bool,
    pub halted: bool,
    /// Orders are accepted: open, and not halted.
    pub tradable: bool,
    pub halt: Option<Halt>,
    /// When the next session opens; `null` with no trading calendar.
    pub next_open_ms: Option<i64>,
    /// When the running session closes; `null` if none is running.
    pub next_close_ms: Option<i64>,
    /// The price the limit band is measured from, and how far the price has
    /// moved from it.
    pub band_cents: i64,
    pub move_pct: f64,
    /// The move that stops trading; `0` when automatic halts are off.
    pub limit_pct: f64,
}

/// What one call to [`SymbolState::advance_to`] produced.
#[derive(Clone, Debug, Default)]
pub struct Advanced {
    /// Last tick emitted, if any.
    pub last: Option<Tick>,
    /// Number of ticks emitted.
    pub ticks: u64,
    /// Per interval in [`Interval::ALL`] order: did at least one bar close?
    pub closed: [bool; 4],
    /// Trades of the last tick, for the stream.
    pub last_trades: Vec<Trade>,
    /// Every trade a trader took part in, in order.
    pub trader_trades: Vec<Trade>,
}

impl Advanced {
    /// Intervals that closed at least one bar.
    pub fn closed_intervals(&self) -> Vec<Interval> {
        Interval::ALL
            .iter()
            .zip(self.closed)
            .filter(|(_, closed)| *closed)
            .map(|(iv, _)| *iv)
            .collect()
    }
}

/// Live quote for the symbol list and the SSE hello message.
#[derive(Clone, Debug, Serialize)]
pub struct Quote {
    pub symbol: &'static str,
    pub name: String,
    pub sector: String,
    /// Timestamp of the last tick.
    pub ts_ms: i64,
    pub price_cents: i64,
    /// Close of the previous daily bar, if there is one.
    pub prev_close_cents: Option<i64>,
    /// `price / prev_close − 1`, in percent.
    pub change_pct: Option<f64>,
    pub day_open_cents: Option<i64>,
    pub day_high_cents: Option<i64>,
    pub day_low_cents: Option<i64>,
    pub day_volume: u64,
    pub fundamental_cents: i64,
    pub annual_vol: f64,
    pub pending_events: usize,
    pub bid_cents: Option<i64>,
    pub ask_cents: Option<i64>,
    /// `stock` or `good`: what this listing is.
    pub asset_kind: &'static str,
    /// What one unit of a good is called; `null` for a stock, whose unit is
    /// a share.
    pub unit: Option<String>,
    /// Units in existence: a stock's shares, or a good's issued units less
    /// its consumed ones.
    pub shares_outstanding: u64,
    /// `price × shares_outstanding`.
    pub market_cap_cents: i64,
    /// A session is running.
    pub market_open: bool,
    /// Trading is stopped.
    pub halted: bool,
}

/// Serialisable view of [`fehu::Snapshot`].
#[derive(Clone, Debug, Serialize)]
pub struct SnapshotDto {
    pub ts_ms: i64,
    pub price_cents: i64,
    pub fundamental_cents: i64,
    pub log_spread: f64,
    pub annual_vol: f64,
    pub drift_effect: f64,
    pub vol_effect: f64,
    pub pending_events: usize,
}

impl From<Snapshot> for SnapshotDto {
    fn from(s: Snapshot) -> Self {
        Self {
            ts_ms: s.ts.0,
            price_cents: s.price_cents,
            fundamental_cents: s.fundamental_cents,
            log_spread: s.log_spread,
            annual_vol: s.annual_vol,
            drift_effect: s.drift_effect,
            vol_effect: s.vol_effect,
            pending_events: s.pending_events,
        }
    }
}

/// What [`SymbolState::reconfigure`] may change. Every field is optional
/// and an absent one is left as it is.
#[derive(Clone, Debug, Default)]
pub struct Patch {
    pub name: Option<String>,
    pub sector: Option<String>,
    pub description: Option<String>,
    pub drift: Option<f64>,
    pub volatility: Option<f64>,
    pub base_volume_per_day: Option<f64>,
    pub half_spread: Option<f64>,
    pub synthetic: Option<bool>,
}

impl Patch {
    /// The fields the patch sets, named, for an audit line.
    #[must_use]
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if self.name.is_some() {
            parts.push("name".to_string());
        }
        if self.sector.is_some() {
            parts.push("sector".to_string());
        }
        if self.description.is_some() {
            parts.push("description".to_string());
        }
        if let Some(v) = self.drift {
            parts.push(format!("drift {v}"));
        }
        if let Some(v) = self.volatility {
            parts.push(format!("volatility {v}"));
        }
        if let Some(v) = self.base_volume_per_day {
            parts.push(format!("volume {v}/day"));
        }
        if let Some(v) = self.half_spread {
            parts.push(format!("half spread {v}"));
        }
        if let Some(v) = self.synthetic {
            parts.push(format!("synthetic {v}"));
        }
        if parts.is_empty() {
            "nothing".to_string()
        } else {
            parts.join(", ")
        }
    }
}

/// One symbol: its exchange (simulator plus order book), the bars
/// aggregated from its ticks, the coarse daily bars generated as
/// pre-history at start-up, and the tape.
#[derive(Clone)]
pub struct SymbolState {
    pub info: SymbolInfo,
    pub exchange: Exchange,
    /// 1 m / 5 m / 1 h / 1 d bars aggregated from fine ticks.
    pub candles: Candles,
    /// Daily bars from coarse mode, before the fine-tick history starts.
    pub coarse_daily: Vec<Candle>,
    pub last_tick: Option<Tick>,
    /// Fine ticks emitted since start-up (warm-up included).
    pub ticks_total: u64,
    /// Most recent trades, oldest first.
    pub tape: VecDeque<Trade>,
    tape_cap: usize,
    /// Trades since start-up (warm-up included).
    pub trades_total: u64,
    /// Set while trading is stopped.
    pub halt: Option<Halt>,
    /// The price the current band is measured from: where the day opened, or
    /// where trading resumed. A move of more than `price_limit_pct` away from
    /// it halts the symbol.
    pub band_cents: i64,
    /// Stops waiting for the price to reach them, oldest first. They are not
    /// orders and the book does not know about them; see [`StopOrder`].
    pub stops: Vec<StopOrder>,
    /// Set by the market as the symbol leaves the table. A job that reached
    /// this actor through a handle taken before that finds it set, and is
    /// answered as if the symbol were not there — because it is not.
    pub delisted: bool,
}

impl SymbolState {
    pub fn new(spec: SymbolSpec, max_bars: usize, tape_cap: usize) -> Self {
        Self::create(spec, max_bars, tape_cap).expect("seeded configs are valid")
    }

    /// Build a symbol from a spec that has not been vetted. The seeded ones
    /// have been, and go through [`SymbolState::new`]; a listing asked for
    /// over HTTP has not, and its config is checked here rather than
    /// panicking on the request thread.
    ///
    /// # Errors
    /// The first [`fehu::ConfigError`] in the simulator config or the trading
    /// parameters.
    pub fn create(
        spec: SymbolSpec,
        max_bars: usize,
        tape_cap: usize,
    ) -> Result<Self, fehu::ConfigError> {
        let exchange = Exchange::new(spec.config, spec.trading, spec.info.seed)?;
        Ok(Self {
            info: spec.info,
            exchange,
            candles: Candles::new(max_bars),
            coarse_daily: Vec::new(),
            last_tick: None,
            ticks_total: 0,
            tape: VecDeque::new(),
            tape_cap: tape_cap.max(1),
            trades_total: 0,
            halt: None,
            band_cents: 0,
            stops: Vec::new(),
            delisted: false,
        })
    }

    /// The reference price process.
    pub fn sim(&self) -> &fehu::Simulator {
        self.exchange.simulator()
    }

    /// Change what this symbol is called and how its price behaves, in
    /// place. The name, sector and description are the listing's; drift,
    /// volatility and daily volume go to the simulator through
    /// [`Exchange::set_config`], the half spread and the synthetic switch to
    /// the exchange through [`Exchange::set_params`], each of which takes
    /// effect from the next step and draws nothing. A good is never quoted
    /// synthetically, whatever the patch says — its units are counted, and
    /// a print would be one nobody issued.
    ///
    /// # Errors
    /// The first [`fehu::ConfigError`] a new value trips; nothing is
    /// changed on a refusal, the listing's text included.
    pub fn reconfigure(&mut self, patch: &Patch) -> Result<(), fehu::ConfigError> {
        let mut config = self.sim().config().clone();
        if let Some(drift) = patch.drift {
            config.drift = drift;
        }
        if let Some(volatility) = patch.volatility {
            config.volatility = volatility;
        }
        if let Some(base) = patch.base_volume_per_day {
            config.volume.base_per_day = base;
        }
        let mut params = *self.exchange.params();
        if let Some(half_spread) = patch.half_spread {
            params.liquidity.half_spread = half_spread;
        }
        if let Some(synthetic) = patch.synthetic {
            params.synthetic = synthetic && !self.info.is_good();
        }
        // Both validated before either is applied, so a bad spread does not
        // leave a new volatility behind it.
        config.validate()?;
        params.validate()?;
        self.exchange.set_config(config)?;
        self.exchange.set_params(params)?;
        if let Some(name) = &patch.name {
            self.info.name = name.clone();
        }
        if let Some(sector) = &patch.sector {
            self.info.sector = sector.clone();
        }
        if let Some(description) = &patch.description {
            self.info.description = description.clone();
        }
        Ok(())
    }

    /// Generate `coarse_days` daily bars in coarse mode, then tick finely up
    /// to `until` so the intraday intervals have history too. Both run on
    /// the bare simulator (no traders exist yet, so the ticks are the ones
    /// the exchange would have produced) and the book is synced at the end.
    pub fn warm_up(&mut self, coarse_days: usize, until: Timestamp) {
        let sim = self.exchange.simulator_mut();
        self.coarse_daily = sim.coarse_candles(Interval::D1).take(coarse_days).collect();
        let dur = until - sim.clock();
        if dur > 0 {
            for tick in sim.advance(Duration::from_millis(dur as u64)) {
                self.candles.push(&tick);
                self.last_tick = Some(tick);
                self.ticks_total += 1;
            }
        }
        self.exchange.resync();
        self.band_cents = self.price_cents();
    }

    /// Advance the exchange's wall clock to `target` (no-op if it is not in
    /// the future), aggregating every tick into the bars and every trade
    /// into the tape.
    pub fn advance_to(&mut self, target: Timestamp) -> Advanced {
        let mut out = Advanced::default();
        let dur = target - self.exchange.clock();
        if dur <= 0 {
            return out;
        }
        let Self {
            exchange,
            candles,
            tape,
            tape_cap,
            ..
        } = self;
        let dur = Duration::from_millis(dur as u64);
        let reports: Box<dyn Iterator<Item = fehu::StepReport> + '_> = if self.halt.is_some() {
            Box::new(exchange.advance_without_matching(dur))
        } else {
            Box::new(exchange.advance(dur))
        };
        for report in reports {
            let closed = candles.push(&report.tick);
            for (flag, c) in out.closed.iter_mut().zip(closed) {
                *flag |= c.is_some();
            }
            out.last = Some(report.tick);
            out.ticks += 1;
            out.trader_trades.extend(
                report.trades.iter().filter(|t| {
                    t.taker.owner.trader().is_some() || t.maker.owner.trader().is_some()
                }),
            );
            self.trades_total += report.trades.len() as u64;
            for t in &report.trades {
                if tape.len() >= *tape_cap {
                    tape.pop_front();
                }
                tape.push_back(*t);
            }
            out.last_trades = report.trades;
        }
        if out.last.is_some() {
            self.last_tick = out.last;
            self.ticks_total += out.ticks;
        }
        out
    }

    /// The symbol's trading calendar, if it has one.
    pub fn market_hours(&self) -> Option<&MarketHours> {
        self.sim().config().market_hours.as_ref()
    }

    /// A session is running at `now` (always, with no calendar).
    pub fn is_open(&self, now: Timestamp) -> bool {
        self.market_hours().is_none_or(|mh| mh.contains(now))
    }

    /// When the next session opens, with a calendar.
    pub fn next_open_ms(&self, now: Timestamp) -> Option<i64> {
        self.market_hours().map(|mh| {
            if mh.contains(now) {
                mh.next_open(now).0
            } else {
                mh.align(now).0
            }
        })
    }

    /// When the running session closes, if one is running.
    pub fn next_close_ms(&self, now: Timestamp) -> Option<i64> {
        self.market_hours()
            .filter(|mh| mh.contains(now))
            .map(|mh| mh.session_close(now).0)
    }

    /// Why an order cannot be sent right now, if it cannot: outside the
    /// session, or halted. Cancelling is always allowed — a player must be
    /// able to pull an order out of a market that has stopped.
    pub fn closed(&self, now: Timestamp) -> Option<Closed> {
        if let Some(halt) = self.halt {
            return Some(Closed::Halted(halt));
        }
        if self.is_open(now) {
            return None;
        }
        Some(Closed::Session {
            next_open_ms: self.next_open_ms(now).unwrap_or(now.0),
        })
    }

    /// Measure the band from where the price is now: a new day, or trading
    /// starting again after a halt.
    pub fn reband(&mut self) {
        self.band_cents = self.price_cents();
    }

    /// Stop trading in this symbol.
    pub fn halt(&mut self, reason: HaltReason, until_ms: Option<i64>, now_ms: i64) -> Halt {
        let price_cents = self.price_cents();
        let band_cents = if self.band_cents > 0 {
            self.band_cents
        } else {
            price_cents
        };
        let halt = Halt {
            reason,
            since_ms: now_ms,
            until_ms,
            band_cents,
            price_cents,
            move_pct: band_move(band_cents, price_cents),
        };
        self.halt = Some(halt);
        halt
    }

    /// Start trading again, measuring a fresh band from where the price got
    /// to while it was stopped.
    pub fn resume(&mut self) -> Option<Halt> {
        let was = self.halt.take();
        if was.is_some() {
            self.reband();
        }
        was
    }

    /// Record trades executed between ticks (a trader's order) on the tape.
    pub fn record_trades(&mut self, trades: &[Trade]) {
        self.trades_total += trades.len() as u64;
        for t in trades {
            if self.tape.len() >= self.tape_cap {
                self.tape.pop_front();
            }
            self.tape.push_back(*t);
        }
    }

    /// Top `depth` levels of each side.
    pub fn book(&self, depth: usize) -> BookDto {
        let b = self.exchange.book();
        BookDto {
            bids: b.depth(fehu::Side::Buy, depth),
            asks: b.depth(fehu::Side::Sell, depth),
        }
    }

    /// The most recent `limit` bars of `iv`, oldest first, including the
    /// in-progress bar. Daily bars include the coarse pre-history.
    pub fn bars(&self, iv: Interval, limit: usize) -> Vec<Candle> {
        self.bars_before(iv, limit, None)
    }

    /// [`Self::bars`], read further back: the last `limit` bars that opened
    /// strictly before `before_ms`, so a client walks the history one page
    /// at a time by passing the `open_ts` of the oldest bar it has.
    pub fn bars_before(&self, iv: Interval, limit: usize, before_ms: Option<i64>) -> Vec<Candle> {
        let mut v: Vec<Candle> = Vec::new();
        if iv == Interval::D1 {
            v.extend(self.coarse_daily.iter().copied());
        }
        v.extend(self.candles.completed(iv).copied());
        v.extend(self.candles.current(iv).copied());
        if let Some(before) = before_ms {
            v.retain(|c| c.open_ts.0 < before);
        }
        if v.len() > limit {
            v.drain(..v.len() - limit);
        }
        v
    }

    /// Everything about this symbol a restart needs: the exchange (simulator,
    /// book and pending flow), the bars, and the tape.
    pub fn to_save(&self) -> SymbolSave {
        SymbolSave {
            symbol: self.info.symbol.to_string(),
            info: Some(self.info.clone()),
            exchange: self.exchange.clone(),
            candles: self.candles.clone(),
            coarse_daily: self.coarse_daily.clone(),
            last_tick: self.last_tick,
            ticks_total: self.ticks_total,
            tape: self.tape.iter().copied().collect(),
            trades_total: self.trades_total,
            halt: self.halt,
            band_cents: self.band_cents,
            stops: self.stops.clone(),
        }
    }

    /// Rebuild a symbol from a save. Its listing comes out of the file with
    /// it: which symbols a restored market has, and what they are, is what
    /// was saved rather than what this build seeds.
    pub fn from_save(info: SymbolInfo, save: SymbolSave, tape_cap: usize) -> Self {
        let tape_cap = tape_cap.max(1);
        let mut tape: VecDeque<Trade> = save.tape.into_iter().collect();
        while tape.len() > tape_cap {
            tape.pop_front();
        }
        Self {
            info,
            exchange: save.exchange,
            candles: save.candles,
            coarse_daily: save.coarse_daily,
            last_tick: save.last_tick,
            ticks_total: save.ticks_total,
            tape,
            tape_cap,
            trades_total: save.trades_total,
            halt: save.halt,
            band_cents: save.band_cents,
            stops: save.stops,
            delisted: false,
        }
    }

    /// The last traded price, or the simulator's reference before the first
    /// tick.
    pub fn price_cents(&self) -> i64 {
        self.last_tick
            .map_or_else(|| self.sim().snapshot().price_cents, |t| t.price_cents)
    }

    /// The whole company at the last traded price.
    pub fn market_cap_cents(&self) -> i64 {
        notional_cents(self.price_cents(), self.info.units_outstanding())
    }

    /// Current quote.
    pub fn quote(&self) -> Quote {
        let snap = self.sim().snapshot();
        let daily = self.bars(Interval::D1, 2);
        let today = self.candles.current(Interval::D1);
        let prev_close = match (today, daily.len()) {
            (Some(_), n) if n >= 2 => Some(daily[n - 2].close),
            (None, n) if n >= 1 => Some(daily[n - 1].close),
            _ => None,
        };
        let price = self.last_tick.map_or(snap.price_cents, |t| t.price_cents);
        Quote {
            symbol: self.info.symbol,
            name: self.info.name.clone(),
            sector: self.info.sector.clone(),
            ts_ms: self.last_tick.map_or(snap.ts.0, |t| t.ts.0),
            price_cents: price,
            prev_close_cents: prev_close,
            change_pct: prev_close
                .filter(|&p| p > 0)
                .map(|p| (price as f64 / p as f64 - 1.0) * 100.0),
            day_open_cents: today.map(|c| c.open),
            day_high_cents: today.map(|c| c.high),
            day_low_cents: today.map(|c| c.low),
            day_volume: today.map_or(0, |c| c.volume),
            fundamental_cents: snap.fundamental_cents,
            annual_vol: snap.annual_vol,
            pending_events: snap.pending_events,
            bid_cents: self.exchange.book().best_bid(),
            ask_cents: self.exchange.book().best_ask(),
            asset_kind: self.info.asset.label(),
            unit: self.info.asset.unit().map(str::to_owned),
            shares_outstanding: self.info.units_outstanding(),
            market_cap_cents: notional_cents(price, self.info.units_outstanding()),
            market_open: self.is_open(Timestamp(self.last_tick.map_or(snap.ts.0, |t| t.ts.0))),
            halted: self.halt.is_some(),
        }
    }
}

/// What one engine step did to one symbol. The symbol's own actor produces
/// it; the market actor settles it — books the trades, sweeps the expired
/// orders, places the stops that fired — and publishes it.
#[derive(Clone, Debug, Default)]
pub struct Stepped {
    /// Ticks emitted.
    pub ticks: u64,
    /// The `tick` message for the stream, if a tick was emitted.
    pub tick: Option<StreamMessage>,
    /// Every trade of the step a trader took part in, in order, still to be
    /// booked.
    pub trades: Vec<Trade>,
    /// The symbol halted or resumed during the step.
    pub status: Option<SymbolStatus>,
    /// Trades of the requote that follows an automatic resume, to be booked
    /// after `trades`.
    pub resumed: Vec<Trade>,
    /// Stops the price reached. They are gone from the store; the market
    /// places the orders they become.
    pub fired: Vec<StopOrder>,
    /// The price that fired them.
    pub price_cents: i64,
    /// The symbol's clock after the step, for the expiry sweep.
    pub clock_ms: i64,
}

/// Why an order could not be prepared for the book.
#[derive(Clone, Debug, PartialEq)]
pub enum OrderCheck {
    /// The book itself refused the order: tick, lot, quantity.
    Invalid(String),
    /// A closed session or a halt takes no new orders at all.
    Closed(Closed),
    /// The order would trade with the trader's own resting orders.
    SelfTrade(Vec<u64>),
    /// A post-only order whose price is already tradable.
    WouldCross { price_cents: i64, best_cents: i64 },
    /// No resting order with that id belongs to the trader.
    UnknownOrder(u64),
}

/// An order the symbol has checked and priced, ready for the market to
/// fund and send.
#[derive(Clone, Debug)]
pub struct Prepared {
    /// Worst-case cash a buy can consume: `price × qty` for a limit, the
    /// preview for a market order.
    pub cost_cents: i64,
    /// Shares the traders' resting bids already speak for.
    pub bid_shares: u64,
    /// When the resting remainder is withdrawn, if ever.
    pub expires_at_ms: Option<i64>,
}

impl SymbolState {
    /// What this symbol's trading state is at `now`.
    pub fn status(&self, now: Timestamp, halts: HaltPolicy) -> SymbolStatus {
        let halted = self.halt.is_some();
        let market_open = self.is_open(now);
        SymbolStatus {
            symbol: self.info.symbol,
            ts_ms: now.0,
            market_open,
            halted,
            tradable: market_open && !halted,
            halt: self.halt,
            next_open_ms: self.next_open_ms(now),
            next_close_ms: self.next_close_ms(now),
            band_cents: self.band_cents,
            move_pct: band_move(self.band_cents, self.price_cents()),
            limit_pct: halts.price_limit_pct,
        }
    }

    /// Shares the traders' resting buy orders are still bidding for. They
    /// are counted as spoken for: were they all to fill, the traders would
    /// hold them.
    pub fn bid_shares(&self) -> u64 {
        self.exchange
            .book()
            .orders()
            .filter(|o| o.side == Side::Buy && o.owner.trader().is_some())
            .map(|o| o.outstanding())
            .fold(0, u64::saturating_add)
    }

    /// Check an order against this symbol — the book's rules, the session,
    /// the trader's own resting orders — and price its worst case. Nothing
    /// changes; the market funds it and sends it with [`SymbolState::submit`].
    pub fn prepare(
        &self,
        order: &Order,
        post_only: bool,
        day: bool,
        expires_at_ms: Option<i64>,
        now: Timestamp,
    ) -> Result<Prepared, OrderCheck> {
        // Validated against this symbol's own book: the tick and lot are a
        // property of the listing, not of orders in general.
        self.exchange
            .book()
            .validate(order)
            .map_err(|e| OrderCheck::Invalid(e.to_string()))?;
        if let Some(closed) = self.closed(now) {
            return Err(OrderCheck::Closed(closed));
        }
        if post_only {
            self.check_post_only(order)?;
        }
        let crossing = self.self_crossing(order);
        if !crossing.is_empty() {
            return Err(OrderCheck::SelfTrade(crossing));
        }
        let expires_at_ms = self.expiry_of(day, expires_at_ms, now)?;
        let cost_cents = match order.kind {
            OrderKind::Limit { price_cents } => {
                i64::try_from(i128::from(price_cents) * i128::from(order.qty)).unwrap_or(i64::MAX)
            }
            OrderKind::Market => {
                self.exchange
                    .preview_market(order.side, order.qty)
                    .notional_cents
            }
        };
        Ok(Prepared {
            cost_cents,
            bid_shares: self.bid_shares(),
            expires_at_ms,
        })
    }

    /// Send a funded order to the exchange, with an id no lower than
    /// `min_id`, and put its trades on the tape.
    ///
    /// # Errors
    /// The exchange's own refusal.
    pub fn submit(
        &mut self,
        order: Order,
        min_id: fehu::OrderId,
        display_qty: Option<u64>,
    ) -> Result<Submitted, fehu::OrderError> {
        self.exchange.advance_order_id(min_id);
        let placement = match display_qty {
            None => self.exchange.submit(order)?,
            Some(display) => self.exchange.submit_iceberg(order, display)?,
        };
        self.record_trades(&placement.trades);
        Ok(Submitted {
            placement,
            next_order_id: self.exchange.book().next_order_id(),
            clock_ms: self.exchange.clock().0,
        })
    }

    /// Withdraw one of `trader`'s resting orders from the book.
    pub fn cancel(
        &mut self,
        order_id: u64,
        trader: TraderId,
    ) -> Result<Resting, fehu::CancelError> {
        self.exchange.cancel(fehu::OrderId(order_id), trader)
    }

    /// Withdraw every resting order `trader` has here.
    pub fn cancel_all(&mut self, trader: TraderId) -> Vec<Resting> {
        self.exchange.cancel_all(trader)
    }

    /// The first half of an amendment: withdraw the resting order and build
    /// its replacement, checked the way a fresh order is. If the checks on
    /// the replacement fail the old order is already gone, which is what an
    /// amendment that cannot be placed means (see the API docs).
    ///
    /// The withdrawn order comes back for the market to release; the
    /// replacement goes through [`SymbolState::prepare`] on the way.
    pub fn amend(
        &mut self,
        order_id: u64,
        trader: TraderId,
        price_cents: Option<i64>,
        qty: Option<u64>,
        post_only: bool,
        now: Timestamp,
    ) -> Result<(Resting, Order, Prepared), OrderCheck> {
        if let Some(closed) = self.closed(now) {
            return Err(OrderCheck::Closed(closed));
        }
        let resting = *self
            .exchange
            .book()
            .get(fehu::OrderId(order_id))
            .filter(|o| o.owner == fehu::Owner::Trader(trader))
            .ok_or(OrderCheck::UnknownOrder(order_id))?;
        let order = Order {
            owner: fehu::Owner::Trader(trader),
            side: resting.side,
            kind: OrderKind::Limit {
                price_cents: price_cents.unwrap_or(resting.price_cents),
            },
            tif: fehu::TimeInForce::Gtc,
            qty: qty.unwrap_or(resting.outstanding()),
        };
        self.exchange
            .book()
            .validate(&order)
            .map_err(|e| OrderCheck::Invalid(e.to_string()))?;
        // Withdraw the old one first: it would otherwise be in the way of its
        // own replacement, both as liquidity and as a reservation.
        let cancelled = self
            .exchange
            .cancel(fehu::OrderId(order_id), trader)
            .map_err(|_| OrderCheck::UnknownOrder(order_id))?;
        let prepared = self.prepare(&order, post_only, false, None, now)?;
        Ok((cancelled, order, prepared))
    }

    /// When a submission's resting remainder should be withdrawn, if ever.
    ///
    /// `expires_at_ms` names the moment; `day` asks for the close of the
    /// session the order is sent in, which needs a trading calendar to have
    /// a close at all. Both are about the *resting* remainder: an order that
    /// trades on arrival has nothing left to expire.
    fn expiry_of(
        &self,
        day: bool,
        expires_at_ms: Option<i64>,
        now: Timestamp,
    ) -> Result<Option<i64>, OrderCheck> {
        if day && expires_at_ms.is_some() {
            return Err(OrderCheck::Invalid(
                "an order is either a day order or expires at a time of its own, not both".into(),
            ));
        }
        if day {
            return match self.next_close_ms(now) {
                Some(close) => Ok(Some(close)),
                None => Err(OrderCheck::Invalid(
                    "a day order needs a trading calendar (FEHU_MARKET_HOURS): with none, \
                     the session never closes and there is nothing to expire at"
                        .into(),
                )),
            };
        }
        match expires_at_ms {
            None => Ok(None),
            Some(at) if at > now.0 => Ok(Some(at)),
            Some(at) => Err(OrderCheck::Invalid(format!(
                "expires_at_ms {at} is not in the future (it is now {})",
                now.0
            ))),
        }
    }

    /// The price an order can reach: its limit, or for a market order the
    /// collar the exchange turns it into.
    fn reachable_price_cents(&self, order: &Order) -> i64 {
        match order.kind {
            OrderKind::Limit { price_cents } => price_cents,
            OrderKind::Market => {
                let collar = self.exchange.params().liquidity.market_collar;
                let reference = self.exchange.reference_cents() as f64;
                let price = match order.side {
                    Side::Buy => (reference * (1.0 + collar)).ceil(),
                    Side::Sell => (reference * (1.0 - collar)).floor(),
                };
                (price as i64).max(1)
            }
        }
    }

    /// The trader's own resting orders this one would trade with. Trading
    /// with yourself moves no shares and no money but does print on the tape
    /// and move the price, so it is refused rather than matched.
    ///
    /// Only orders the incoming one would actually reach count: the book's
    /// own preview says how far down the other side it would walk, and
    /// anything past that is none of its business.
    fn self_crossing(&self, order: &Order) -> Vec<u64> {
        let book = self.exchange.book();
        let limit = self.reachable_price_cents(order);
        let Some(worst) = book
            .preview(order.side, order.qty, Some(limit))
            .worst_price_cents
        else {
            return Vec::new(); // Nothing would trade at all.
        };
        book.orders_of(order.owner)
            .filter(|resting| resting.side != order.side)
            .filter(|resting| match order.side {
                Side::Buy => resting.price_cents <= worst,
                Side::Sell => resting.price_cents >= worst,
            })
            .map(|resting| resting.id.0)
            .collect()
    }

    /// A post-only order must rest. It cannot if it is a market order, if it
    /// is not good-till-cancelled, or if its price is already tradable.
    fn check_post_only(&self, order: &Order) -> Result<(), OrderCheck> {
        let OrderKind::Limit { price_cents } = order.kind else {
            return Err(OrderCheck::Invalid(
                "a market order cannot be post-only: it exists to take liquidity".into(),
            ));
        };
        if order.tif != fehu::TimeInForce::Gtc {
            return Err(OrderCheck::Invalid(
                "a post-only order must be `gtc`: the others are there to trade at once".into(),
            ));
        }
        let book = self.exchange.book();
        let best = match order.side {
            Side::Buy => book.best_ask().filter(|ask| *ask <= price_cents),
            Side::Sell => book.best_bid().filter(|bid| *bid >= price_cents),
        };
        match best {
            Some(best) => Err(OrderCheck::WouldCross {
                price_cents,
                best_cents: best,
            }),
            None => Ok(()),
        }
    }

    /// Check a stop against this symbol: the trigger has to be a price the
    /// book can print, on the far side of the market, and the order it
    /// fires has to be one the book would take. Nothing is held yet; the
    /// market checks the account and then [`arms`](SymbolState::arm) it.
    pub fn check_stop(&self, req: &StopRequest) -> Result<(), String> {
        let rules = self.exchange.book().rules();
        if req.stop_price_cents <= 0 {
            return Err("a stop price must be above zero".into());
        }
        // The trigger is a price like any other on this symbol, so it sits on
        // the same grid: a stop at a price the market cannot print at is a
        // trigger that may never be reached exactly.
        if !rules.allows_price(req.stop_price_cents) {
            return Err(format!(
                "a stop price must be a whole number of {}-cent ticks",
                rules.tick_cents
            ));
        }
        // A trigger the market has already passed is not a trigger: it is a
        // market order with extra steps, and almost certainly a mistake.
        let price = self.price_cents();
        let behind = match req.side {
            Side::Buy => req.stop_price_cents <= price,
            Side::Sell => req.stop_price_cents >= price,
        };
        if behind {
            let (side, where_) = match req.side {
                Side::Buy => ("buy", "above"),
                Side::Sell => ("sell", "below"),
            };
            return Err(format!(
                "a {side} stop must trigger {where_} the market: {} is already reached at {price}",
                req.stop_price_cents
            ));
        }
        // The order it will become has to be one the book would take, or the
        // trigger is armed to fail.
        let probe = StopOrder {
            stop_id: 0,
            trader_id: req.trader_id,
            symbol: self.info.symbol,
            side: req.side,
            qty: req.qty,
            stop_price_cents: req.stop_price_cents,
            limit_price_cents: req.limit_price_cents,
            tif: req.tif,
            client_order_id: None,
            created_at_ms: 0,
        };
        self.exchange
            .book()
            .validate(&probe.order())
            .map_err(|e| e.to_string())
    }

    /// Hold a stop until the price reaches it.
    pub fn arm(&mut self, stop: StopOrder) {
        self.stops.push(stop);
    }

    /// Withdraw a held stop. `None` if no such stop is held here, or it is
    /// somebody else's.
    pub fn disarm(&mut self, stop_id: u64, trader: TraderId) -> Option<StopOrder> {
        let at = self
            .stops
            .iter()
            .position(|s| s.stop_id == stop_id && s.trader_id == trader.0)?;
        Some(self.stops.remove(at))
    }

    /// Every stop `trader` holds here, oldest first.
    pub fn stops_of(&self, trader: TraderId) -> Vec<StopOrder> {
        self.stops
            .iter()
            .filter(|s| s.trader_id == trader.0)
            .cloned()
            .collect()
    }

    /// Stop trading by hand. It stays stopped until somebody resumes it.
    pub fn halt_by_hand(&mut self, now: Timestamp, halts: HaltPolicy) -> SymbolStatus {
        self.halt(HaltReason::Manual, None, now.0);
        self.status(now, halts)
    }

    /// Start trading again, whatever stopped it. A symbol that resumes into
    /// an open session is requoted at once; the requote's trades are on the
    /// tape and come back for the market to book.
    pub fn resume_trading(
        &mut self,
        now: Timestamp,
        halts: HaltPolicy,
    ) -> (SymbolStatus, Vec<Trade>) {
        let was_halted = self.resume().is_some();
        let trades = if was_halted && self.is_open(now) {
            self.exchange.resync()
        } else {
            Vec::new()
        };
        self.record_trades(&trades);
        (self.status(now, halts), trades)
    }

    /// One engine step: advance to `target`, review the halt, and pick out
    /// the stops the price reached. The trades are on the tape; booking
    /// them, sweeping expired orders and placing the fired stops need the
    /// accounts, so the market does those from what comes back.
    pub fn step(&mut self, target: Timestamp, halts: HaltPolicy) -> Stepped {
        if self.delisted {
            return Stepped::default();
        }
        let mut advanced = self.advance_to(target);
        // A new day is a new band to measure the limit move from.
        if advanced.closed_intervals().contains(&Interval::D1) {
            self.reband();
        }
        let closed = advanced.closed_intervals();
        let mut out = Stepped {
            ticks: advanced.ticks,
            trades: std::mem::take(&mut advanced.trader_trades),
            clock_ms: self.exchange.clock().0,
            ..Stepped::default()
        };
        if let Some(t) = advanced.last {
            let trades = advanced
                .last_trades
                .iter()
                .rev()
                .take(MAX_STREAM_TRADES)
                .map(TradeDto::from)
                .collect();
            out.tick = Some(StreamMessage::Tick {
                symbol: self.info.symbol,
                ts_ms: t.ts.0,
                price_cents: t.price_cents,
                volume: t.volume,
                closed,
                bid_cents: self.exchange.book().best_bid(),
                ask_cents: self.exchange.book().best_ask(),
                book: self.book(STREAM_BOOK_DEPTH),
                trades,
            });
        }
        out.status = self.review_halt(target, halts, &mut out.resumed);
        // After the halts, so a symbol that just resumed fires the triggers
        // the price reached while it was stopped; a symbol that is halted or
        // outside its session fires nothing and holds them.
        out.price_cents = self.price_cents();
        if !self.stops.is_empty() && self.halt.is_none() && self.is_open(target) {
            let price_cents = out.price_cents;
            let fired = &mut out.fired;
            self.stops.retain(|stop| {
                let hit = stop.triggered_by(price_cents);
                if hit {
                    fired.push(stop.clone());
                }
                !hit
            });
        }
        out
    }

    /// Halt a symbol whose price has left the band the day opened with, and
    /// lift an automatic halt once its time is up. Returns the new state if
    /// it changed; the trades of a resume go into `resumed`.
    fn review_halt(
        &mut self,
        now: Timestamp,
        halts: HaltPolicy,
        resumed: &mut Vec<Trade>,
    ) -> Option<SymbolStatus> {
        if let Some(halt) = self.halt {
            // A manual halt has no end: only a resume lifts it.
            if halt.until_ms.is_some_and(|until| until <= now.0) {
                let (status, trades) = self.resume_trading(now, halts);
                resumed.extend(trades);
                return Some(status);
            }
            return None;
        }
        if halts.price_limit_pct <= 0.0 || !self.is_open(now) {
            return None;
        }
        if band_move(self.band_cents, self.price_cents()).abs() < halts.price_limit_pct {
            return None;
        }
        let halt_ms = halts.halt_secs as i64 * 1000;
        self.halt(HaltReason::LimitMove, Some(now.0 + halt_ms), now.0);
        Some(self.status(now, halts))
    }

    /// The last thing a symbol does: everything of it that is not history is
    /// withdrawn. The resting trader orders come back for the market to
    /// release; the stops are simply dropped, having reserved nothing; the
    /// book's next id comes back so no order can ever take an id it handed
    /// out. From here on the symbol answers as if it were not there.
    pub fn wind_up(&mut self) -> WoundUp {
        let orders: Vec<(u64, TraderId)> = self
            .exchange
            .book()
            .orders()
            .filter_map(|o| o.owner.trader().map(|t| (o.id.0, t)))
            .collect();
        let mut cancelled = Vec::new();
        for (order_id, trader) in orders {
            if let Ok(resting) = self.exchange.cancel(fehu::OrderId(order_id), trader) {
                cancelled.push((trader, resting));
            }
        }
        let stops_cancelled = std::mem::take(&mut self.stops).len();
        self.delisted = true;
        WoundUp {
            symbol: self.info.symbol,
            last_price_cents: self.price_cents(),
            cancelled,
            stops_cancelled,
            next_order_id: self.exchange.book().next_order_id(),
        }
    }
}

/// What the book said to an order.
#[derive(Clone, Debug)]
pub struct Submitted {
    pub placement: Placement,
    /// The book's counter afterwards, which the market folds into its own.
    pub next_order_id: fehu::OrderId,
    /// The symbol's clock, which the order record is stamped with.
    pub clock_ms: i64,
}

/// What a symbol leaves behind when it is wound up.
#[derive(Clone, Debug)]
pub struct WoundUp {
    pub symbol: &'static str,
    pub last_price_cents: i64,
    /// Every resting trader order, withdrawn, with its owner.
    pub cancelled: Vec<(TraderId, Resting)>,
    pub stops_cancelled: usize,
    /// The book's counter, for the market to allocate above.
    pub next_order_id: fehu::OrderId,
}

/// How far `price` is from `band`, as a signed fraction. Zero when the band
/// is not a price at all, so a symbol that has never traded cannot halt.
pub fn band_move(band_cents: i64, price_cents: i64) -> f64 {
    if band_cents <= 0 {
        return 0.0;
    }
    price_cents as f64 / band_cents as f64 - 1.0
}
