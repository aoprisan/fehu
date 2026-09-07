//! The seeded symbols, their simulators and bar history, and the shared
//! application state.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fehu::{
    Candle, Candles, Config, Exchange, Interval, JumpParams, LiquidityParams, MarketHours, Side,
    Snapshot, Tick, Timestamp, Trade, TraderId, TradingParams, VolumeParams,
};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::account::{Account, AccountId, MoneyError, User, UserId, notional_cents};
use crate::auth::Keyring;
use crate::events::EventRecord;
use crate::limit::{Decision, Limiter, Rate};
use crate::metrics::Metrics;
use crate::save::{MarketSave, STATE_VERSION, Save, SymbolSave};
use crate::trading::{
    BookDto, Fees, FillRecord, HoldingDto, MAX_STOPS_PER_TRADER, OrderRecord, OrderResponse,
    Refused, StopOrder, StopRequest, TradeDto, Trader,
};

/// Milliseconds in one day.
pub const DAY_MS: i64 = 86_400_000;

/// Static metadata for a listed symbol.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct SymbolInfo {
    /// Ticker, e.g. `ACME`.
    pub symbol: &'static str,
    /// Company name.
    pub name: &'static str,
    /// Sector label.
    pub sector: &'static str,
    /// One-line flavour text.
    pub description: &'static str,
    /// Shares in existence. Nothing creates or destroys them: what the
    /// traders hold plus what is still out in the market adds up to this, so
    /// a buy cannot ask for more than is left (see
    /// [`Market::available_shares`]).
    pub shares_outstanding: u64,
    /// RNG seed. Same seed + same events ⇒ same prices, every run.
    pub seed: u64,
}

/// A symbol's metadata plus the simulator config and trading parameters it
/// is created with.
pub struct SymbolSpec {
    pub info: SymbolInfo,
    pub config: Config,
    pub trading: TradingParams,
}

/// The listed tickers, in listing order. The symbol set is fixed at build
/// time: it names the `&'static str`s the rest of the server uses, and a save
/// file listing anything else is refused rather than guessed at.
pub const TICKERS: [&str; 4] = ["ACME", "NBLA", "HLIO", "PXCO"];

/// `ticker` as the `&'static str` the server uses for it, matched
/// case-insensitively.
pub fn intern(ticker: &str) -> Option<&'static str> {
    TICKERS
        .iter()
        .find(|s| s.eq_ignore_ascii_case(ticker))
        .copied()
}

/// The four hardcoded symbols. `start_ts` is the first tick's timestamp; the
/// rest of the config is per symbol and deliberately varied so the charts
/// look different from one another.
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
                symbol: "ACME",
                name: "Acme Industrial",
                sector: "Industrials",
                description: "Century-old conglomerate. Low volatility, steady drift, rare jumps.",
                shares_outstanding: 240_000_000,
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
                symbol: "NBLA",
                name: "Nebula Robotics",
                sector: "Technology",
                description: "Pre-profit robotics darling. High volatility, big drift, frequent jumps.",
                shares_outstanding: 85_000_000,
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
                symbol: "HLIO",
                name: "Helio Energy",
                sector: "Energy",
                description: "Solar and storage utility. Commodity-driven, moderate volatility.",
                shares_outstanding: 610_000_000,
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
                symbol: "PXCO",
                name: "Pax Consumer Co",
                sector: "Consumer Staples",
                description: "Household brands. Defensive: low volatility, shocks fade slowly.",
                shares_outstanding: 150_000_000,
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
    pub name: &'static str,
    pub sector: &'static str,
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
    /// Shares in existence for this symbol.
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

/// One symbol: its exchange (simulator plus order book), the bars
/// aggregated from its ticks, the coarse daily bars generated as
/// pre-history at start-up, and the tape.
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
}

impl SymbolState {
    fn new(spec: SymbolSpec, max_bars: usize, tape_cap: usize) -> Self {
        let exchange = Exchange::new(spec.config, spec.trading, spec.info.seed)
            .expect("seeded configs are valid");
        Self {
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
        }
    }

    /// The reference price process.
    pub fn sim(&self) -> &fehu::Simulator {
        self.exchange.simulator()
    }

    /// Generate `coarse_days` daily bars in coarse mode, then tick finely up
    /// to `until` so the intraday intervals have history too. Both run on
    /// the bare simulator (no traders exist yet, so the ticks are the ones
    /// the exchange would have produced) and the book is synced at the end.
    fn warm_up(&mut self, coarse_days: usize, until: Timestamp) {
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
        let mut v: Vec<Candle> = Vec::new();
        if iv == Interval::D1 {
            v.extend(self.coarse_daily.iter().copied());
        }
        v.extend(self.candles.completed(iv).copied());
        v.extend(self.candles.current(iv).copied());
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

    /// Rebuild a symbol from a save. Its metadata (name, sector, share count,
    /// seed) comes from the build rather than the file: the save carries only
    /// what changed while the server ran.
    fn from_save(info: SymbolInfo, save: SymbolSave, tape_cap: usize) -> Self {
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
        notional_cents(self.price_cents(), self.info.shares_outstanding)
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
            name: self.info.name,
            sector: self.info.sector,
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
            shares_outstanding: self.info.shares_outstanding,
            market_cap_cents: notional_cents(price, self.info.shares_outstanding),
            market_open: self.is_open(Timestamp(self.last_tick.map_or(snap.ts.0, |t| t.ts.0))),
            halted: self.halt.is_some(),
        }
    }
}

/// Everything behind the mutex: the symbols, the users, their accounts and
/// traders, and the event log.
pub struct Market {
    pub symbols: Vec<SymbolState>,
    /// Most recent events, oldest first.
    pub events: VecDeque<EventRecord>,
    event_cap: usize,
    next_event_id: u64,
    pub users: BTreeMap<UserId, User>,
    next_user_id: u64,
    /// The API keys issued to those users.
    pub keys: Keyring,
    pub accounts: BTreeMap<AccountId, Account>,
    next_account_id: u64,
    pub traders: BTreeMap<TraderId, Trader>,
    next_trader_id: u64,
    /// Ids for the stops held on the symbols. Separate from order ids: a
    /// stop only becomes an order when it fires.
    next_stop_id: u64,
    /// Every order the log still holds, by order id.
    orders: BTreeMap<u64, OrderRecord>,
    /// Order ids in the order they were accepted, for eviction.
    order_ids: VecDeque<u64>,
    /// `(trader, client_order_id)` → order id, for idempotent submission.
    client_order_ids: BTreeMap<(TraderId, String), u64>,
    fill_log: usize,
    ledger_log: usize,
    order_log: usize,
    /// Orders sent to a book since start-up, and submissions turned away
    /// before they got there. Counters, not state: they are not saved, and a
    /// restart begins them again.
    pub orders_placed: u64,
    pub orders_refused: u64,
    /// Fills booked to traders' accounts since start-up.
    pub fills_booked: u64,
    /// What the venue charges for a fill.
    pub fees: Fees,
    /// A move this far from the band halts a symbol; `0` turns that off.
    price_limit_pct: f64,
    /// How long an automatic halt lasts, in simulated seconds.
    halt_secs: u64,
}

impl Market {
    /// Index of a symbol by ticker, case-insensitively.
    pub fn symbol_index(&self, ticker: &str) -> Option<usize> {
        self.symbols
            .iter()
            .position(|s| s.info.symbol.eq_ignore_ascii_case(ticker))
    }

    /// Register a user and issue their API key. `name` and `email` are
    /// trimmed and truncated; an empty name becomes `user-{id}`. The key is
    /// read back once with [`crate::auth::Keyring::take_issued_key`], for the response
    /// that created them.
    pub fn create_user(
        &mut self,
        name: Option<String>,
        email: Option<String>,
        now_ms: i64,
    ) -> UserId {
        let id = UserId(self.next_user_id);
        self.next_user_id += 1;
        let user = User {
            id,
            name: clean(name, 64).unwrap_or_else(|| format!("user-{}", id.0)),
            email: clean(email, 254),
            created_at_ms: now_ms,
            accounts: Vec::new(),
        };
        self.users.insert(id, user);
        self.keys.issue(id);
        id
    }

    /// Open an account for `user_id` with an opening balance of
    /// `cash_cents`. The user must exist and the balance must be a
    /// non-negative number of cents no larger than the balance cap.
    pub fn open_account(
        &mut self,
        user_id: UserId,
        name: Option<String>,
        cash_cents: i64,
        now_ms: i64,
    ) -> Result<AccountId, MoneyError> {
        debug_assert!(self.users.contains_key(&user_id), "unknown user");
        let id = AccountId(self.next_account_id);
        let account = Account::open(
            id,
            user_id,
            clean(name, 64).unwrap_or_else(|| format!("account-{}", id.0)),
            cash_cents,
            self.ledger_log,
            now_ms,
        )?;
        self.next_account_id += 1;
        self.accounts.insert(id, account);
        if let Some(user) = self.users.get_mut(&user_id) {
            user.accounts.push(id);
        }
        Ok(id)
    }

    /// Create a trader for `user_id` that trades on `account_id`.
    pub fn create_trader(
        &mut self,
        user_id: UserId,
        account_id: AccountId,
        name: Option<String>,
        now_ms: i64,
    ) -> TraderId {
        let id = TraderId(self.next_trader_id);
        self.next_trader_id += 1;
        let name = clean(name, 64).unwrap_or_else(|| format!("trader-{}", id.0));
        self.traders.insert(
            id,
            Trader::new(id, user_id, account_id, name, self.fill_log, now_ms),
        );
        id
    }

    /// The one-call sign-up behind `POST /api/traders`: a user, an account
    /// funded with `cash_cents`, and a trader that trades on it.
    pub fn sign_up(
        &mut self,
        name: Option<String>,
        email: Option<String>,
        cash_cents: i64,
        now_ms: i64,
    ) -> Result<TraderId, MoneyError> {
        let user = self.create_user(name.clone(), email, now_ms);
        let account = self.open_account(user, name.clone(), cash_cents, now_ms)?;
        Ok(self.create_trader(user, account, name, now_ms))
    }

    /// The account a trader trades on.
    pub fn account_of(&self, trader: TraderId) -> Option<&Account> {
        let t = self.traders.get(&trader)?;
        self.accounts.get(&t.account_id)
    }

    /// The trader that trades on `account`, if there is one.
    pub fn trader_on(&self, account: AccountId) -> Option<&Trader> {
        self.traders.values().find(|t| t.account_id == account)
    }

    /// A trader and its account, both mutable.
    pub fn trader_and_account(&mut self, trader: TraderId) -> Option<(&mut Trader, &mut Account)> {
        let Self {
            traders, accounts, ..
        } = self;
        let t = traders.get_mut(&trader)?;
        let a = accounts.get_mut(&t.account_id)?;
        Some((t, a))
    }

    /// The canonical ticker of `ticker`, matched case-insensitively.
    pub fn ticker(&self, ticker: &str) -> Option<&'static str> {
        self.symbol(ticker).map(|s| s.info.symbol)
    }

    /// Shares of `symbol` the traders hold between them.
    pub fn held_shares(&self, symbol: &str) -> u64 {
        let Some(sym) = self.ticker(symbol) else {
            return 0;
        };
        self.traders
            .values()
            .map(|t| t.held_shares(sym))
            .fold(0, u64::saturating_add)
    }

    /// Shares of `symbol` the traders' resting buy orders are still bidding
    /// for. They are counted as spoken for: were they all to fill, the
    /// traders would hold them.
    pub fn bid_shares(&self, symbol: &str) -> u64 {
        self.symbol(symbol).map_or(0, |s| {
            s.exchange
                .book()
                .orders()
                .filter(|o| o.side == Side::Buy && o.owner.trader().is_some())
                .map(|o| o.remaining)
                .fold(0, u64::saturating_add)
        })
    }

    /// Shares of `symbol` no trader holds or has bid for — what a buy can
    /// still be filled from. A symbol has a fixed number of shares
    /// ([`SymbolInfo::shares_outstanding`]), so once the traders between them
    /// hold or bid for all of them there is nothing left to buy.
    pub fn available_shares(&self, symbol: &str) -> u64 {
        self.symbol(symbol).map_or(0, |s| {
            s.info
                .shares_outstanding
                .saturating_sub(self.held_shares(symbol))
                .saturating_sub(self.bid_shares(symbol))
        })
    }

    /// What `user` owns, one entry per symbol, added up across every trader
    /// of theirs and marked to the reference price. Ordered by ticker.
    pub fn user_holdings(&self, user: UserId) -> Vec<HoldingDto> {
        let mut by_symbol: BTreeMap<&'static str, HoldingDto> = BTreeMap::new();
        for trader in self.traders.values().filter(|t| t.user_id == user) {
            for sym in trader.positions.keys() {
                by_symbol
                    .entry(sym)
                    .or_insert_with(|| HoldingDto::empty(sym))
                    .add(trader);
            }
        }
        by_symbol
            .into_values()
            .map(|mut h| {
                h.mark(
                    self.symbol(h.symbol)
                        .map_or(0, |s| s.exchange.reference_cents()),
                );
                h
            })
            .collect()
    }

    /// Shares of `symbol` that `user` could sell right now: what their
    /// traders hold, less what their resting sells already promised.
    pub fn user_free_shares(&self, user: UserId, symbol: &str) -> u64 {
        let Some(sym) = self.ticker(symbol) else {
            return 0;
        };
        self.traders
            .values()
            .filter(|t| t.user_id == user)
            .map(|t| t.free_shares(sym))
            .fold(0, u64::saturating_add)
    }

    /// One order by id, whatever became of it — as long as the log still
    /// holds it.
    pub fn order(&self, order_id: u64) -> Option<&OrderRecord> {
        self.orders.get(&order_id)
    }

    /// A trader's orders, newest first.
    pub fn orders_of(&self, trader: TraderId) -> impl Iterator<Item = &OrderRecord> {
        self.order_ids
            .iter()
            .rev()
            .filter_map(|id| self.orders.get(id))
            .filter(move |o| o.trader_id == trader.0)
    }

    /// The order a `client_order_id` was already used for, if any. A repeat
    /// submission is answered from this rather than sent to the book again.
    pub fn order_by_client_id(&self, trader: TraderId, client_id: &str) -> Option<&OrderRecord> {
        self.client_order_ids
            .get(&(trader, client_id.to_string()))
            .and_then(|id| self.orders.get(id))
    }

    /// Add an accepted order to the log, evicting the oldest finished ones
    /// once it is over `order_log`. Live orders are never evicted.
    pub fn record_order(&mut self, record: OrderRecord) {
        let id = record.order_id;
        if let Some(client_id) = record.client_order_id.clone() {
            self.client_order_ids
                .insert((TraderId(record.trader_id), client_id), id);
        }
        self.orders.insert(id, record);
        self.order_ids.push_back(id);
        while self.orders.len() > self.order_log {
            let Some(pos) = self
                .order_ids
                .iter()
                .position(|id| self.orders.get(id).is_some_and(|o| !o.is_live()))
            else {
                break; // Everything still in the log is live: keep it all.
            };
            let Some(id) = self.order_ids.remove(pos) else {
                break;
            };
            if let Some(old) = self.orders.remove(&id)
                && let Some(client_id) = old.client_order_id
            {
                self.client_order_ids
                    .remove(&(TraderId(old.trader_id), client_id));
            }
        }
    }

    /// Mark an order cancelled in the log.
    pub fn cancel_order_record(&mut self, symbol: &str, order_id: u64, remaining: u64, ts_ms: i64) {
        if let Some(record) = self.orders.get_mut(&order_id)
            && record.symbol == symbol
        {
            record.cancel(remaining, ts_ms);
        }
    }

    /// Book every trade in `trades` (for symbol `sym`) to the traders
    /// involved. Returns the fills created, in order.
    pub fn apply_trades(&mut self, sym: &'static str, trades: &[Trade]) -> Vec<FillRecord> {
        let fees = self.fees;
        let mut fills = Vec::new();
        for t in trades {
            // A resting order that trades has moved on since it was accepted.
            for party in [&t.taker, &t.maker] {
                if party.owner.trader().is_some()
                    && let Some(record) = self.orders.get_mut(&party.order.0)
                    && record.symbol == sym
                {
                    record.fill(t.qty, t.price_cents, t.ts.0);
                }
            }
            let mut parties = [t.taker.owner.trader(), t.maker.owner.trader()];
            if parties[0] == parties[1] {
                parties[1] = None;
            }
            for id in parties.into_iter().flatten() {
                if let Some((trader, account)) = self.trader_and_account(id) {
                    fills.extend(trader.apply_trade(account, sym, t, fees));
                }
            }
        }
        self.fills_booked = self.fills_booked.saturating_add(fills.len() as u64);
        fills
    }

    /// What a symbol's trading state is at `now`.
    pub fn status(&self, index: usize, now: Timestamp) -> Option<SymbolStatus> {
        let s = self.symbols.get(index)?;
        let halted = s.halt.is_some();
        let market_open = s.is_open(now);
        Some(SymbolStatus {
            symbol: s.info.symbol,
            ts_ms: now.0,
            market_open,
            halted,
            tradable: market_open && !halted,
            halt: s.halt,
            next_open_ms: s.next_open_ms(now),
            next_close_ms: s.next_close_ms(now),
            band_cents: s.band_cents,
            move_pct: band_move(s.band_cents, s.price_cents()),
            limit_pct: self.price_limit_pct,
        })
    }

    /// The move that halts a symbol; `0` when automatic halts are off.
    pub fn price_limit_pct(&self) -> f64 {
        self.price_limit_pct
    }

    /// Stop trading in the symbol at `index` by hand. It stays stopped until
    /// somebody resumes it.
    pub fn halt(&mut self, index: usize, now: Timestamp) -> Option<SymbolStatus> {
        self.symbols
            .get_mut(index)?
            .halt(HaltReason::Manual, None, now.0);
        self.status(index, now)
    }

    /// Pay `cents_per_share` on every share of `index` a trader holds, and
    /// drop the price by the same amount.
    ///
    /// Both halves matter. Paying without the price move would be money from
    /// nothing — buy the day before, collect, sell the day after — so the
    /// reference and the fundamental both fall by the dividend, which is what
    /// going ex-dividend means. The shares themselves do not move: nothing is
    /// created or destroyed, so `shares_outstanding` is untouched.
    ///
    /// A frozen account is paid too: it still owns its shares.
    pub fn pay_dividend(
        &mut self,
        index: usize,
        cents_per_share: i64,
        note: Option<String>,
        now_ms: i64,
    ) -> Option<Dividend> {
        let symbol = self.symbols.get(index)?;
        let sym = symbol.info.symbol;
        let price_cents = symbol.price_cents();
        if cents_per_share <= 0 || cents_per_share >= price_cents {
            return None;
        }
        let owed: Vec<(AccountId, i64, u64)> = self
            .traders
            .values()
            .filter_map(|t| {
                let qty = u64::try_from(t.positions.get(sym).map_or(0, |p| p.qty)).ok()?;
                (qty > 0).then(|| (t.account_id, notional_cents(cents_per_share, qty), qty))
            })
            .collect();
        let mut paid = Dividend {
            symbol: sym,
            cents_per_share,
            price_cents,
            shares_paid: 0,
            accounts_paid: 0,
            total_cents: 0,
        };
        for (account_id, amount, qty) in owed {
            let Some(account) = self.accounts.get_mut(&account_id) else {
                continue;
            };
            if account
                .pay_dividend(amount, sym, note.clone(), now_ms)
                .is_some()
            {
                paid.accounts_paid += 1;
                paid.shares_paid = paid.shares_paid.saturating_add(qty);
                paid.total_cents = paid.total_cents.saturating_add(amount);
            }
        }
        Some(paid)
    }

    /// Hold a stop until the price reaches it.
    ///
    /// The account is checked here so an obviously unfundable trigger is
    /// refused while the trader is still looking at the response, but nothing
    /// is reserved: a stop that never fires costs its owner nothing, and the
    /// real check is the one made when it does fire.
    pub fn place_stop(
        &mut self,
        idx: usize,
        req: &StopRequest,
        now_ms: i64,
    ) -> Result<StopOrder, PlaceError> {
        let trader = TraderId(req.trader_id);
        let sym = self.symbols[idx].info.symbol;
        let rules = self.symbols[idx].exchange.book().rules();
        let fees = self.fees;
        if req.stop_price_cents <= 0 {
            return Err(PlaceError::Invalid(
                "a stop price must be above zero".into(),
            ));
        }
        // The trigger is a price like any other on this symbol, so it sits on
        // the same grid: a stop at a price the market cannot print at is a
        // trigger that may never be reached exactly.
        if !rules.allows_price(req.stop_price_cents) {
            return Err(PlaceError::Invalid(format!(
                "a stop price must be a whole number of {}-cent ticks",
                rules.tick_cents
            )));
        }
        // A trigger the market has already passed is not a trigger: it is a
        // market order with extra steps, and almost certainly a mistake.
        let price = self.symbols[idx].price_cents();
        let behind = match req.side {
            Side::Buy => req.stop_price_cents <= price,
            Side::Sell => req.stop_price_cents >= price,
        };
        if behind {
            let (side, where_) = match req.side {
                Side::Buy => ("buy", "above"),
                Side::Sell => ("sell", "below"),
            };
            return Err(PlaceError::Invalid(format!(
                "a {side} stop must trigger {where_} the market: {} is already reached at {price}",
                req.stop_price_cents
            )));
        }
        if self.stops_of(trader).count() >= MAX_STOPS_PER_TRADER {
            return Err(PlaceError::Invalid(format!(
                "a trader may hold {MAX_STOPS_PER_TRADER} stops at once; cancel one first"
            )));
        }
        let stop = StopOrder {
            stop_id: self.next_stop_id,
            trader_id: req.trader_id,
            symbol: sym,
            side: req.side,
            qty: req.qty,
            stop_price_cents: req.stop_price_cents,
            limit_price_cents: req.limit_price_cents,
            tif: req.tif,
            client_order_id: req.client_order_id.clone(),
            created_at_ms: now_ms,
        };
        // The order it will become has to be one the book would take, or the
        // trigger is armed to fail.
        self.symbols[idx]
            .exchange
            .book()
            .validate(&stop.order())
            .map_err(|e| PlaceError::Invalid(e.to_string()))?;
        let account = self
            .account_of(trader)
            .ok_or(PlaceError::UnknownTrader(trader.0))?;
        self.traders[&trader]
            .check(account, sym, req.side, req.qty, {
                // The order it fires will take liquidity, so the advisory
                // check counts the taker fee too.
                let cost = stop.cost_cents();
                cost.saturating_add(fees.taker_cost(cost))
            })
            .map_err(PlaceError::Refused)?;
        self.next_stop_id += 1;
        self.symbols[idx].stops.push(stop.clone());
        Ok(stop)
    }

    /// Every stop a trader is holding, oldest first, across all symbols.
    pub fn stops_of(&self, trader: TraderId) -> impl Iterator<Item = &StopOrder> {
        self.symbols
            .iter()
            .flat_map(|s| s.stops.iter())
            .filter(move |stop| stop.trader_id == trader.0)
    }

    /// Withdraw a held stop. `None` if no such stop is held, or it is
    /// somebody else's.
    pub fn cancel_stop(&mut self, stop_id: u64, trader: TraderId) -> Option<StopOrder> {
        for symbol in &mut self.symbols {
            if let Some(at) = symbol
                .stops
                .iter()
                .position(|s| s.stop_id == stop_id && s.trader_id == trader.0)
            {
                return Some(symbol.stops.remove(at));
            }
        }
        None
    }

    /// Fire every stop the last price has reached, oldest first, and place
    /// the orders they become.
    ///
    /// A symbol that is halted or outside its session fires nothing: the
    /// triggers are held, and a resume in this same step lets them go. A stop
    /// that fires is gone from the store either way — the account is checked
    /// a second time here, and a trigger the money no longer covers is
    /// reported rather than retried.
    fn fire_stops(
        &mut self,
        index: usize,
        now: Timestamp,
        fills: &mut Vec<FillRecord>,
    ) -> Vec<StreamMessage> {
        let symbol = &mut self.symbols[index];
        if symbol.stops.is_empty() || symbol.halt.is_some() || !symbol.is_open(now) {
            return Vec::new();
        }
        let price_cents = symbol.price_cents();
        let mut fired = Vec::new();
        symbol.stops.retain(|stop| {
            let hit = stop.triggered_by(price_cents);
            if hit {
                fired.push(stop.clone());
            }
            !hit
        });
        fired
            .into_iter()
            .map(|stop| {
                let trader = TraderId(stop.trader_id);
                let placed = self.place(index, trader, stop.order(), stop.client_order_id.clone());
                let (order, refused) = match placed {
                    Ok((response, placed_fills)) => {
                        fills.extend(placed_fills);
                        (Some(response), None)
                    }
                    Err(e) => (None, Some(e.to_string())),
                };
                StreamMessage::StopTriggered {
                    trader_id: stop.trader_id,
                    stop,
                    price_cents,
                    order,
                    refused,
                }
            })
            .collect()
    }

    /// Withdraw every resting order whose time is up.
    ///
    /// A day order and a good-till-date order differ only in where the
    /// deadline came from; by the time the engine sees them they are both
    /// just a resting order with an `expires_at_ms`. The sweep runs at the
    /// end of every step, on every symbol — a halted one included, because a
    /// halt stops trading, not the clock, and an order whose date has passed
    /// should not come back when the market does.
    fn sweep_expired(&mut self, index: usize) -> Vec<StreamMessage> {
        let sym = self.symbols[index].info.symbol;
        let now_ms = self.symbols[index].exchange.clock().0;
        let due: Vec<u64> = self
            .orders
            .values()
            .filter(|o| o.symbol == sym && o.has_expired(now_ms))
            .map(|o| o.order_id)
            .collect();
        let mut messages = Vec::new();
        for order_id in due {
            let trader = TraderId(self.orders[&order_id].trader_id);
            let Ok(cancelled) = self.symbols[index]
                .exchange
                .cancel(fehu::OrderId(order_id), trader)
            else {
                // Filled or already gone between the log and the book: the
                // record is no longer live, so nothing is owed.
                continue;
            };
            if let Some((t, account)) = self.trader_and_account(trader) {
                t.release(
                    account,
                    sym,
                    cancelled.side,
                    cancelled.remaining,
                    cancelled.price_cents,
                );
            }
            self.cancel_order_record(sym, order_id, cancelled.remaining, now_ms);
            if let Some(record) = self.orders.get(&order_id) {
                messages.push(StreamMessage::OrderExpired {
                    trader_id: trader.0,
                    order: record.clone(),
                });
            }
        }
        messages
    }

    /// Start trading again, whatever stopped it.
    pub fn resume(
        &mut self,
        index: usize,
        now: Timestamp,
    ) -> Option<(SymbolStatus, Vec<FillRecord>)> {
        let symbol = self.symbols.get_mut(index)?;
        let was_halted = symbol.resume().is_some();
        let trades = if was_halted && symbol.is_open(now) {
            symbol.exchange.resync()
        } else {
            Vec::new()
        };
        symbol.record_trades(&trades);
        let ticker = symbol.info.symbol;
        let fills = self.apply_trades(ticker, &trades);
        Some((self.status(index, now)?, fills))
    }

    /// Halt a symbol whose price has left the band the day opened with, and
    /// lift an automatic halt once its time is up. Returns the new state if
    /// it changed.
    fn review_halt(
        &mut self,
        index: usize,
        now: Timestamp,
        fills: &mut Vec<FillRecord>,
    ) -> Option<SymbolStatus> {
        let (limit_pct, halt_ms) = (self.price_limit_pct, self.halt_secs as i64 * 1000);
        let s = self.symbols.get_mut(index)?;
        if let Some(halt) = s.halt {
            // A manual halt has no end: only a resume lifts it.
            if halt.until_ms.is_some_and(|until| until <= now.0) {
                let (status, resumed_fills) = self.resume(index, now)?;
                fills.extend(resumed_fills);
                return Some(status);
            }
            return None;
        }
        if limit_pct <= 0.0 || !s.is_open(now) {
            return None;
        }
        if band_move(s.band_cents, s.price_cents()).abs() < limit_pct {
            return None;
        }
        s.halt(HaltReason::LimitMove, Some(now.0 + halt_ms), now.0);
        self.status(index, now)
    }

    /// Advance every symbol to `target`, book the resulting fills, and
    /// return the stream messages describing what happened.
    pub fn advance_to(&mut self, target: Timestamp) -> (u64, Vec<StreamMessage>) {
        let mut total = 0;
        let mut messages = Vec::new();
        let mut fills = Vec::new();
        for i in 0..self.symbols.len() {
            let advanced = self.symbols[i].advance_to(target);
            total += advanced.ticks;
            // A new day is a new band to measure the limit move from.
            if advanced.closed_intervals().contains(&Interval::D1) {
                self.symbols[i].reband();
            }
            let sym = self.symbols[i].info.symbol;
            if !advanced.trader_trades.is_empty() {
                fills.extend(self.apply_trades(sym, &advanced.trader_trades));
            }
            if let Some(t) = advanced.last {
                let s = &self.symbols[i];
                let trades = advanced
                    .last_trades
                    .iter()
                    .rev()
                    .take(MAX_STREAM_TRADES)
                    .map(TradeDto::from)
                    .collect();
                messages.push(StreamMessage::Tick {
                    symbol: sym,
                    ts_ms: t.ts.0,
                    price_cents: t.price_cents,
                    volume: t.volume,
                    closed: advanced.closed_intervals(),
                    bid_cents: s.exchange.book().best_bid(),
                    ask_cents: s.exchange.book().best_ask(),
                    book: s.book(STREAM_BOOK_DEPTH),
                    trades,
                });
            }
        }
        for i in 0..self.symbols.len() {
            if let Some(status) = self.review_halt(i, target, &mut fills) {
                messages.push(StreamMessage::Status(status));
            }
        }
        // Before the stops, so a trigger cannot fire an order that would
        // immediately be swept, and after the halts for the same reason a
        // resume settles first.
        for i in 0..self.symbols.len() {
            let expired = self.sweep_expired(i);
            messages.extend(expired);
        }
        // After the halts, so a symbol that just resumed fires the triggers
        // the price reached while it was stopped.
        for i in 0..self.symbols.len() {
            let triggered = self.fire_stops(i, target, &mut fills);
            messages.extend(triggered);
        }
        messages.extend(fills.into_iter().map(|fill| StreamMessage::Fill {
            trader_id: fill_trader(&fill),
            fill,
        }));
        (total, messages)
    }

    /// Look a symbol up by ticker, case-insensitively.
    pub fn symbol(&self, ticker: &str) -> Option<&SymbolState> {
        self.symbols
            .iter()
            .find(|s| s.info.symbol.eq_ignore_ascii_case(ticker))
    }

    pub fn symbol_mut(&mut self, ticker: &str) -> Option<&mut SymbolState> {
        self.symbols
            .iter_mut()
            .find(|s| s.info.symbol.eq_ignore_ascii_case(ticker))
    }

    /// The people, their money and every log, ready to be written out.
    pub fn to_save(&self) -> MarketSave {
        MarketSave {
            users: self.users.values().cloned().collect(),
            accounts: self.accounts.values().cloned().collect(),
            traders: self.traders.values().cloned().collect(),
            // Oldest first, so restoring in order rebuilds the same log.
            orders: self
                .order_ids
                .iter()
                .filter_map(|id| self.orders.get(id))
                .cloned()
                .collect(),
            order_responses: self
                .order_ids
                .iter()
                .filter_map(|id| {
                    let record = self.orders.get(id)?;
                    Some((*id, record.accepted.clone()?))
                })
                .collect(),
            events: self.events.iter().cloned().collect(),
            api_keys: self.keys.pairs(),
            next_user_id: self.next_user_id,
            next_account_id: self.next_account_id,
            next_trader_id: self.next_trader_id,
            next_event_id: self.next_event_id,
            next_stop_id: self.next_stop_id,
        }
    }

    /// Put a saved market back: users, money, traders and every log, with the
    /// id counters where they left off so nothing is ever handed out twice.
    fn from_save(symbols: Vec<SymbolState>, save: MarketSave, options: &Options) -> Self {
        let mut market = Self {
            symbols,
            events: save.events.into_iter().collect(),
            event_cap: options.event_log.max(1),
            next_event_id: save.next_event_id.max(1),
            users: save.users.into_iter().map(|u| (u.id, u)).collect(),
            next_user_id: save.next_user_id.max(1),
            keys: Keyring::from_pairs(save.api_keys),
            accounts: save.accounts.into_iter().map(|a| (a.id, a)).collect(),
            next_account_id: save.next_account_id.max(1),
            traders: save.traders.into_iter().map(|t| (t.id, t)).collect(),
            next_trader_id: save.next_trader_id.max(1),
            next_stop_id: save.next_stop_id.max(1),
            orders: BTreeMap::new(),
            order_ids: VecDeque::new(),
            client_order_ids: BTreeMap::new(),
            fill_log: options.fill_log,
            ledger_log: options.ledger_log,
            order_log: options.order_log.max(1),
            orders_placed: 0,
            orders_refused: 0,
            fills_booked: 0,
            fees: options.fees(),
            price_limit_pct: options.price_limit_pct.max(0.0),
            halt_secs: options.halt_secs,
        };
        // Through the same door as a live order, so the client-id index and
        // the eviction order come out the same.
        let responses: BTreeMap<u64, OrderResponse> = save.order_responses.into_iter().collect();
        for mut record in save.orders {
            record.accepted = responses.get(&record.order_id).cloned();
            market.record_order(record);
        }
        while market.events.len() > market.event_cap {
            market.events.pop_front();
        }
        market
    }

    /// Assign the next id to `rec`, append it to the log and return it.
    pub fn record(&mut self, mut rec: EventRecord) -> EventRecord {
        rec.id = self.next_event_id;
        self.next_event_id += 1;
        if self.events.len() >= self.event_cap {
            self.events.pop_front();
        }
        self.events.push_back(rec.clone());
        rec
    }
}

/// Maps wall time to simulated time: `sim_now = sim_epoch + elapsed × scale`.
#[derive(Clone, Copy, Debug)]
pub struct SimClock {
    wall_epoch: Instant,
    sim_epoch: Timestamp,
    /// Simulated seconds per wall second.
    pub scale: f64,
}

impl SimClock {
    /// The simulated time right now.
    pub fn now(&self) -> Timestamp {
        let ms = self.wall_epoch.elapsed().as_secs_f64() * self.scale * 1000.0;
        Timestamp(self.sim_epoch.0.saturating_add(ms as i64))
    }
}

/// Most recent prints carried in one `tick` stream message.
pub const MAX_STREAM_TRADES: usize = 20;
/// Book levels per side carried in one `tick` stream message.
pub const STREAM_BOOK_DEPTH: usize = 8;

/// Trader id of a fill (fills are always attributed; see `Trader::record`).
fn fill_trader(f: &FillRecord) -> u64 {
    f.trader_id
}

/// Why a submission could not be placed. The web layer turns these into
/// status codes; the engine turns them into a stop that fired and was
/// refused.
#[derive(Clone, Debug)]
pub enum PlaceError {
    /// The account or the trader's shares would not fund it.
    Refused(Refused),
    /// The trader is not one this market knows.
    UnknownTrader(u64),
    /// The book itself refused the order.
    Invalid(String),
}

impl std::fmt::Display for PlaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(e) => write!(f, "{e}"),
            Self::UnknownTrader(id) => write!(f, "no such trader: {id}"),
            Self::Invalid(message) => write!(f, "{message}"),
        }
    }
}

impl Market {
    /// Send a validated order to the exchange and book everything that
    /// follows: the money check, the share checks, the fills, the reservation
    /// of what rests, and the order log. The caller holds the market lock and
    /// has already established who the trader is and that the symbol is open
    /// to them.
    pub fn place(
        &mut self,
        idx: usize,
        trader: TraderId,
        order: fehu::Order,
        client_order_id: Option<String>,
    ) -> Result<(OrderResponse, Vec<FillRecord>), PlaceError> {
        self.place_expiring(idx, trader, order, client_order_id, None)
    }

    /// [`place`](Self::place), for an order that is withdrawn at `expires_at_ms`
    /// if it is still resting then.
    pub fn place_expiring(
        &mut self,
        idx: usize,
        trader: TraderId,
        order: fehu::Order,
        client_order_id: Option<String>,
        expires_at_ms: Option<i64>,
    ) -> Result<(OrderResponse, Vec<FillRecord>), PlaceError> {
        let sym = self.symbols[idx].info.symbol;
        // Worst-case cash a buy can consume.
        let cost = match order.kind {
            fehu::OrderKind::Limit { price_cents } => {
                i64::try_from(i128::from(price_cents) * i128::from(order.qty)).unwrap_or(i64::MAX)
            }
            fehu::OrderKind::Market => {
                self.symbols[idx]
                    .exchange
                    .preview_market(order.side, order.qty)
                    .notional_cents
            }
        };
        // A symbol has a fixed number of shares: a buy can only be filled from
        // the ones no trader holds or is already bidding for.
        if order.side == Side::Buy {
            let available = self.available_shares(sym);
            if order.qty > available {
                self.orders_refused = self.orders_refused.saturating_add(1);
                return Err(PlaceError::Refused(Refused::SupplyExhausted {
                    needed: order.qty,
                    available,
                }));
            }
        }
        // A crossing order pays the taker fee out of the same cash, and it
        // is charged the moment it fills, so it is checked here rather than
        // discovered afterwards.
        let cost = cost.saturating_add(self.fees.taker_cost(cost));
        // Validate the order against the account that would fund it: it must
        // be active, a buy must have the cash available, and a sell the shares
        // — nothing may be sold that the trader does not hold.
        let account = self
            .account_of(trader)
            .ok_or(PlaceError::UnknownTrader(trader.0))?;
        if let Err(e) = self.traders[&trader].check(account, sym, order.side, order.qty, cost) {
            self.orders_refused = self.orders_refused.saturating_add(1);
            return Err(PlaceError::Refused(e));
        }
        // The log and retry index span all symbols. Allocate above every
        // book's counter while holding the market lock, including ids used by
        // synthetic flow since the last trader submission. Counters already
        // persist in saves.
        let next_id = self
            .symbols
            .iter()
            .map(|s| s.exchange.book().next_order_id())
            .max()
            .unwrap_or(fehu::OrderId(1));
        self.symbols[idx].exchange.advance_order_id(next_id);
        let placement = match self.symbols[idx].exchange.submit(order) {
            Ok(placement) => placement,
            Err(e) => {
                self.orders_refused = self.orders_refused.saturating_add(1);
                return Err(PlaceError::Invalid(e.to_string()));
            }
        };
        self.orders_placed = self.orders_placed.saturating_add(1);
        self.symbols[idx].record_trades(&placement.trades);
        let fills = self.apply_trades(sym, &placement.trades);
        if placement.status == fehu::OrderStatus::Resting
            && let fehu::OrderKind::Limit { price_cents } = order.kind
            && let Some((t, account)) = self.trader_and_account(trader)
        {
            t.reserve(account, sym, order.side, placement.remaining, price_cents);
        }
        let response = OrderResponse::new(sym, trader, order.side, order.qty, &placement);
        let now = self.symbols[idx].exchange.clock().0;
        self.record_order(
            OrderRecord::new(
                client_order_id,
                trader,
                sym,
                &order,
                &placement,
                response.clone(),
                now,
            )
            .expiring_at(expires_at_ms),
        );
        Ok((response, fills))
    }
}

/// What goes out over the SSE stream.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamMessage {
    /// First message on every connection. Its `seq` is where the connection
    /// joins rather than a message number of its own.
    Hello {
        sim_now_ms: i64,
        time_scale: f64,
        quotes: Vec<Quote>,
        /// The earliest sequence `?since=` can still ask for.
        oldest_seq: u64,
        /// Set when this connection asked to resume from further back than
        /// the replay buffer reaches: messages were missed for good, and the
        /// client should reload its snapshots rather than trust its state.
        gap: bool,
    },
    /// The last tick of one engine step for one symbol.
    Tick {
        symbol: &'static str,
        ts_ms: i64,
        price_cents: i64,
        volume: u64,
        /// Intervals for which at least one bar closed during the step, so a
        /// client can refetch instead of extending its last bar.
        closed: Vec<Interval>,
        bid_cents: Option<i64>,
        ask_cents: Option<i64>,
        /// Top of the book after the step.
        book: BookDto,
        /// Newest prints of the step, newest first, at most
        /// [`MAX_STREAM_TRADES`].
        trades: Vec<TradeDto>,
    },
    /// An event was accepted.
    Event(EventRecord),
    /// A trader's order executed (in whole or part).
    Fill { trader_id: u64, fill: FillRecord },
    /// A symbol stopped trading, or started again.
    Status(SymbolStatus),
    /// A resting order reached its expiry and was withdrawn.
    OrderExpired { trader_id: u64, order: OrderRecord },
    /// A stop fired. It is held no longer: it either became the order in
    /// `order`, or was `refused` when the account was checked again.
    StopTriggered {
        trader_id: u64,
        stop: StopOrder,
        /// The price that reached the trigger.
        price_cents: i64,
        order: Option<OrderResponse>,
        refused: Option<String>,
    },
}

/// What a dividend paid, and the price it went ex at.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Dividend {
    pub symbol: &'static str,
    pub cents_per_share: i64,
    /// The price the dividend was declared against, before it went ex.
    pub price_cents: i64,
    pub shares_paid: u64,
    /// Accounts credited. A user with two traders holding the same symbol is
    /// paid once per trader, into whichever account each trades on.
    pub accounts_paid: usize,
    pub total_cents: i64,
}

/// A stream message with its place in the stream.
///
/// Every message the server publishes takes the next number, so a client can
/// tell a gap from a quiet market and ask for what it missed with `?since=`.
/// On the `hello` that opens a connection the number means something slightly
/// different: it is where the connection joins, so the first live message is
/// `seq + 1`.
#[derive(Clone, Debug, Serialize)]
pub struct Sequenced {
    pub seq: u64,
    #[serde(flatten)]
    pub message: StreamMessage,
}

/// The sequence counter and the bounded buffer behind `?since=`.
#[derive(Debug)]
struct StreamLog {
    /// The number the next published message will take.
    next_seq: u64,
    /// The most recently published messages, oldest first.
    recent: VecDeque<Sequenced>,
    cap: usize,
}

impl StreamLog {
    fn new(cap: usize) -> Self {
        Self {
            // The first message published is 1, so 0 is "nothing yet" and a
            // client may ask for everything with `?since=0`.
            next_seq: 1,
            recent: VecDeque::new(),
            cap,
        }
    }
}

/// A new stream connection: where it joins, what it missed, and the live
/// feed from there.
pub struct Subscription {
    pub rx: broadcast::Receiver<Sequenced>,
    /// The last sequence published before this connection opened.
    pub seq: u64,
    /// The earliest sequence the replay buffer still holds. Equal to
    /// `seq + 1` when nothing is buffered.
    pub oldest_seq: u64,
    /// What `?since=` asked for and the buffer still had, oldest first.
    pub replay: Vec<Sequenced>,
    /// Set when `?since=` reached further back than the buffer goes: some
    /// messages are gone for good and the client must reload its snapshots.
    pub gap: bool,
}

/// Start-up options, all overridable through `FEHU_*` environment variables.
#[derive(Clone, Debug)]
pub struct Options {
    /// Daily bars of coarse pre-history to generate. `FEHU_HISTORY_DAYS`.
    pub history_days: usize,
    /// Hours of fine 1 s ticks before "now" (rounded down to the start of that
    /// UTC day, so the intraday history is between this and this + 24 h).
    /// `FEHU_WARMUP_HOURS`.
    pub warmup_hours: u64,
    /// Simulated seconds per wall second. `FEHU_TIME_SCALE`.
    pub time_scale: f64,
    /// Simulated "now" at start-up; defaults to the wall clock. `FEHU_NOW_MS`.
    pub now_ms: Option<i64>,
    /// Completed bars retained per interval. `FEHU_MAX_BARS`.
    pub max_bars: usize,
    /// Events retained in the log. `FEHU_EVENT_LOG`.
    pub event_log: usize,
    /// Trades retained on each symbol's tape. `FEHU_TAPE`.
    pub tape_len: usize,
    /// Fills retained per trader. `FEHU_FILL_LOG`.
    pub fill_log: usize,
    /// Ledger entries retained per account. `FEHU_LEDGER_LOG`.
    pub ledger_log: usize,
    /// Orders retained in the order log. Live orders are never dropped.
    /// `FEHU_ORDER_LOG`.
    pub order_log: usize,
    /// Cash a new account is opened with, in cents.
    /// `FEHU_STARTING_CASH_CENTS`.
    pub starting_cash_cents: i64,
    /// Key the game-master endpoints (pushing events into the simulation)
    /// require. `FEHU_ADMIN_KEY`; unset leaves them open, which is what a
    /// single-player game on localhost wants and a shared server does not.
    pub admin_key: Option<String>,
    /// Where the market is saved, and read back from at start-up.
    /// `FEHU_STATE_FILE`; unset means nothing is kept and every start warms
    /// up a fresh market.
    pub state_file: Option<std::path::PathBuf>,
    /// Seconds between saves. `FEHU_SAVE_SECS`.
    pub save_secs: u64,
    /// The trading calendar every symbol runs on. `FEHU_MARKET_HOURS`, as
    /// `HH:MM-HH:MM` in UTC (`09:30-16:00`); unset means the market never
    /// closes, which is what a game whose players log in at all hours wants.
    pub market_hours: Option<MarketHours>,
    /// How far the price may move from the band the day opened with before
    /// trading stops, as a fraction. `FEHU_PRICE_LIMIT_PCT`; `0` turns
    /// automatic halts off.
    pub price_limit_pct: f64,
    /// How long an automatic halt lasts, in simulated seconds.
    /// `FEHU_HALT_SECS`.
    pub halt_secs: u64,
    /// Stream messages kept for `?since=` replay. `FEHU_STREAM_REPLAY`; `0`
    /// keeps none, and every reconnect is then a gap.
    pub stream_replay: usize,
    /// How fast one client may change the market, in requests per second.
    /// `FEHU_RATE_PER_SEC`; `0` turns rate limiting off. Reads are never
    /// limited.
    pub rate_per_sec: f64,
    /// Requests one client may send at once after a quiet spell.
    /// `FEHU_RATE_BURST`.
    pub rate_burst: f64,
    /// The price step every symbol quotes and trades in, in cents.
    /// `FEHU_TICK_CENTS`; `1` allows every cent, which is the default.
    pub tick_cents: i64,
    /// The share lot every symbol trades in. `FEHU_LOT`; `1` allows every
    /// share, which is the default.
    pub lot: u64,
    /// What a taker pays, in basis points of the fill's notional.
    /// `FEHU_TAKER_FEE_BPS`; `0`, the default, charges nothing. Negative
    /// values are refused: the venue does not pay takers.
    pub taker_fee_bps: i64,
    /// What a maker is paid, in basis points, as a negative number.
    /// `FEHU_MAKER_FEE_BPS`; `0`, the default, pays nothing. Positive values
    /// are refused — see [`crate::trading::Fees`].
    pub maker_fee_bps: i64,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            history_days: 365,
            warmup_hours: 72,
            time_scale: 1.0,
            now_ms: None,
            max_bars: 5_000,
            event_log: 500,
            tape_len: 2_000,
            fill_log: 500,
            ledger_log: 500,
            order_log: 2_000,
            starting_cash_cents: 10_000_000,
            admin_key: None,
            state_file: None,
            save_secs: 30,
            market_hours: None,
            price_limit_pct: 0.10,
            halt_secs: 300,
            stream_replay: 1_024,
            rate_per_sec: 20.0,
            rate_burst: 40.0,
            tick_cents: 1,
            lot: 1,
            taker_fee_bps: 0,
            maker_fee_bps: 0,
        }
    }
}

impl Options {
    /// What the venue charges, with the two restrictions the settlement path
    /// depends on applied: a taker fee is never negative and a maker fee is
    /// never positive.
    #[must_use]
    pub fn fees(&self) -> Fees {
        Fees {
            taker_bps: self.taker_fee_bps.clamp(0, 10_000),
            maker_bps: self.maker_fee_bps.clamp(-10_000, 0),
        }
    }

    /// Defaults overridden by any `FEHU_*` variable that parses.
    pub fn from_env() -> Self {
        let d = Self::default();
        Self {
            history_days: env_parse("FEHU_HISTORY_DAYS", d.history_days),
            warmup_hours: env_parse("FEHU_WARMUP_HOURS", d.warmup_hours),
            time_scale: env_parse("FEHU_TIME_SCALE", d.time_scale),
            now_ms: std::env::var("FEHU_NOW_MS")
                .ok()
                .and_then(|v| v.parse().ok()),
            max_bars: env_parse("FEHU_MAX_BARS", d.max_bars),
            event_log: env_parse("FEHU_EVENT_LOG", d.event_log),
            tape_len: env_parse("FEHU_TAPE", d.tape_len),
            fill_log: env_parse("FEHU_FILL_LOG", d.fill_log),
            ledger_log: env_parse("FEHU_LEDGER_LOG", d.ledger_log),
            order_log: env_parse("FEHU_ORDER_LOG", d.order_log),
            starting_cash_cents: env_parse("FEHU_STARTING_CASH_CENTS", d.starting_cash_cents),
            admin_key: std::env::var("FEHU_ADMIN_KEY")
                .ok()
                .map(|k| k.trim().to_string())
                .filter(|k| !k.is_empty()),
            state_file: std::env::var_os("FEHU_STATE_FILE")
                .map(std::path::PathBuf::from)
                .filter(|p| !p.as_os_str().is_empty()),
            save_secs: env_parse("FEHU_SAVE_SECS", d.save_secs).max(1),
            market_hours: std::env::var("FEHU_MARKET_HOURS")
                .ok()
                .as_deref()
                .and_then(parse_market_hours),
            price_limit_pct: env_parse("FEHU_PRICE_LIMIT_PCT", d.price_limit_pct).max(0.0),
            halt_secs: env_parse("FEHU_HALT_SECS", d.halt_secs).max(1),
            stream_replay: env_parse("FEHU_STREAM_REPLAY", d.stream_replay),
            rate_per_sec: env_parse("FEHU_RATE_PER_SEC", d.rate_per_sec).max(0.0),
            rate_burst: env_parse("FEHU_RATE_BURST", d.rate_burst).max(0.0),
            tick_cents: env_parse("FEHU_TICK_CENTS", d.tick_cents).clamp(1, 1_000_000),
            lot: env_parse("FEHU_LOT", d.lot).clamp(1, 1_000_000),
            taker_fee_bps: env_parse("FEHU_TAKER_FEE_BPS", d.taker_fee_bps),
            maker_fee_bps: env_parse("FEHU_MAKER_FEE_BPS", d.maker_fee_bps),
        }
    }
}

/// A trading calendar from `HH:MM-HH:MM` (UTC, Monday to Friday). Anything
/// else — `off`, an empty string, nonsense — means no calendar at all.
fn parse_market_hours(value: &str) -> Option<MarketHours> {
    let (open, close) = value.trim().split_once('-')?;
    let secs = |hhmm: &str| -> Option<u32> {
        let (h, m) = hhmm.trim().split_once(':')?;
        let (h, m): (u32, u32) = (h.trim().parse().ok()?, m.trim().parse().ok()?);
        (h < 24 && m < 60).then_some(h * 3600 + m * 60)
    };
    let (open_secs, close_secs) = (secs(open)?, secs(close)?);
    (open_secs < close_secs).then_some(MarketHours {
        open_secs,
        close_secs,
        ..MarketHours::default()
    })
}

fn env_parse<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Trim a user-supplied name, cap it at `max` characters, and treat an
/// empty result as absent.
fn clean(value: Option<String>, max: usize) -> Option<String> {
    value
        .map(|v| v.trim().chars().take(max).collect::<String>())
        .filter(|v| !v.is_empty())
}

/// How far `price` is from `band`, as a signed fraction. Zero when the band
/// is not a price at all, so a symbol that has never traded cannot halt.
fn band_move(band_cents: i64, price_cents: i64) -> f64 {
    if band_cents <= 0 {
        return 0.0;
    }
    price_cents as f64 / band_cents as f64 - 1.0
}

/// Milliseconds since the Unix epoch on the wall clock.
pub fn wall_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Shared application state.
pub struct App {
    pub options: Options,
    pub clock: SimClock,
    pub started_at: SystemTime,
    pub market: Mutex<Market>,
    /// Fan-out for the SSE stream. Sending with no subscribers is fine.
    /// Publish through [`App::publish`] rather than sending here directly:
    /// the sequence number and the replay buffer are assigned there.
    pub tx: broadcast::Sender<Sequenced>,
    /// The sequence counter and replay buffer. Held behind its own lock, and
    /// never taken while the market lock is held.
    stream: Mutex<StreamLog>,
    /// How fast each client may change the market. Its own lock, held only
    /// for the moment it takes to spend a token.
    limits: Mutex<Limiter>,
    /// Counters for `GET /api/health`. Atomics, so nothing waits on them.
    pub metrics: Metrics,
}

impl App {
    /// Build the market and warm every symbol up to "now". This runs the
    /// simulators synchronously; with the defaults it is roughly a million
    /// ticks in total, well under a second in a release build.
    pub fn new(options: Options) -> Arc<Self> {
        let now = Timestamp(options.now_ms.unwrap_or_else(wall_now_ms));
        let fine_start =
            Interval::D1.bucket(now - Duration::from_secs(options.warmup_hours * 3600));
        let start_ts = Timestamp(fine_start.0 - options.history_days as i64 * DAY_MS);
        let symbols = seeded_symbols(start_ts)
            .into_iter()
            .map(|mut spec| {
                // Every symbol trades on the same calendar, if there is one,
                // and in the same tick and lot. Both are properties of the
                // listing, so a restored market keeps the ones it was saved
                // with rather than whatever the environment now says.
                spec.config.market_hours = options.market_hours;
                spec.trading.rules = fehu::MarketRules {
                    tick_cents: options.tick_cents,
                    lot: options.lot,
                };
                let mut s = SymbolState::new(spec, options.max_bars, options.tape_len);
                s.warm_up(options.history_days, now);
                s
            })
            .collect();
        let (tx, _) = broadcast::channel(4096);
        let stream = Mutex::new(StreamLog::new(options.stream_replay));
        let limits = Mutex::new(Limiter::new(Rate {
            per_sec: options.rate_per_sec,
            burst: options.rate_burst,
        }));
        Arc::new(Self {
            clock: SimClock {
                wall_epoch: Instant::now(),
                sim_epoch: now,
                scale: options.time_scale,
            },
            started_at: SystemTime::now(),
            stream,
            limits,
            metrics: Metrics::default(),
            market: Mutex::new(Market {
                symbols,
                events: VecDeque::new(),
                event_cap: options.event_log.max(1),
                next_event_id: 1,
                users: BTreeMap::new(),
                next_user_id: 1,
                keys: Keyring::default(),
                accounts: BTreeMap::new(),
                next_account_id: 1,
                traders: BTreeMap::new(),
                next_trader_id: 1,
                next_stop_id: 1,
                orders: BTreeMap::new(),
                order_ids: VecDeque::new(),
                client_order_ids: BTreeMap::new(),
                fill_log: options.fill_log,
                ledger_log: options.ledger_log,
                order_log: options.order_log.max(1),
                orders_placed: 0,
                orders_refused: 0,
                fills_booked: 0,
                fees: options.fees(),
                price_limit_pct: options.price_limit_pct.max(0.0),
                halt_secs: options.halt_secs,
            }),
            tx,
            options,
        })
    }

    /// Everything the server would need to carry on after a restart.
    pub fn save(&self) -> Save {
        let market = self.market();
        // The furthest the market has reached: usually the clock, but an
        // engine step can leave a symbol ahead of it, and starting up behind
        // a symbol's own clock would freeze it until wall time caught up.
        let sim_now_ms = market
            .symbols
            .iter()
            .map(|s| s.exchange.clock().0)
            .fold(self.clock.now().0, i64::max);
        Save {
            version: STATE_VERSION,
            saved_at_ms: wall_now_ms(),
            sim_now_ms,
            symbols: market.symbols.iter().map(SymbolState::to_save).collect(),
            market: market.to_save(),
        }
    }

    /// Build the app from a save instead of warming up: the market carries on
    /// from the simulated time it had reached, with the same books, bars,
    /// users and money.
    ///
    /// The save's symbol list must be the build's — [`crate::save::read`]
    /// checks that — and each symbol's metadata comes from the build.
    pub fn restore(options: Options, save: Save) -> Arc<Self> {
        let now = Timestamp(save.sim_now_ms);
        let specs: BTreeMap<&'static str, SymbolInfo> = seeded_symbols(now)
            .into_iter()
            .map(|spec| (spec.info.symbol, spec.info))
            .collect();
        let symbols = save
            .symbols
            .into_iter()
            .filter_map(|s| {
                let info = *specs.get(intern(&s.symbol)?)?;
                Some(SymbolState::from_save(info, s, options.tape_len))
            })
            .collect();
        let market = Market::from_save(symbols, save.market, &options);
        let (tx, _) = broadcast::channel(4096);
        let stream = Mutex::new(StreamLog::new(options.stream_replay));
        let limits = Mutex::new(Limiter::new(Rate {
            per_sec: options.rate_per_sec,
            burst: options.rate_burst,
        }));
        Arc::new(Self {
            clock: SimClock {
                wall_epoch: Instant::now(),
                sim_epoch: now,
                scale: options.time_scale,
            },
            started_at: SystemTime::now(),
            market: Mutex::new(market),
            tx,
            stream,
            limits,
            metrics: Metrics::default(),
            options,
        })
    }

    /// Publish a message to every open stream, numbering it and keeping it
    /// in the replay buffer. Returns the number it was given.
    ///
    /// Nothing reaches a client any other way: the number is what lets a
    /// reconnecting one tell "nothing happened" from "I missed something".
    pub fn publish(&self, message: StreamMessage) -> u64 {
        let sequenced = {
            let mut log = self.stream.lock().unwrap_or_else(|e| e.into_inner());
            let seq = log.next_seq;
            log.next_seq += 1;
            let sequenced = Sequenced { seq, message };
            if log.cap > 0 {
                while log.recent.len() >= log.cap {
                    log.recent.pop_front();
                }
                log.recent.push_back(sequenced.clone());
            }
            sequenced
        };
        let seq = sequenced.seq;
        // `Err` only means nobody is listening right now.
        let _ = self.tx.send(sequenced);
        seq
    }

    /// Open a stream connection, optionally asking for everything after
    /// `since`.
    ///
    /// The subscription is taken while the sequence lock is held, so nothing
    /// can slip between the replay and the live feed: every message is either
    /// in `replay` or arrives on `rx`, exactly once, in order.
    pub fn subscribe(&self, since: Option<u64>) -> Subscription {
        let log = self.stream.lock().unwrap_or_else(|e| e.into_inner());
        let rx = self.tx.subscribe();
        let seq = log.next_seq.saturating_sub(1);
        let oldest_seq = log.recent.front().map_or(log.next_seq, |m| m.seq);
        let (replay, gap) = match since {
            None => (Vec::new(), false),
            Some(since) => (
                log.recent
                    .iter()
                    .filter(|m| m.seq > since)
                    .cloned()
                    .collect(),
                // The buffer starts after the first message they wanted, so
                // whatever fell out of it is gone for good.
                since + 1 < oldest_seq,
            ),
        };
        Subscription {
            rx,
            seq,
            oldest_seq,
            replay,
            gap,
        }
    }

    /// Spend one request's worth of a client's allowance for a request that
    /// would change something. `who` is `None` for a request with no key,
    /// which shares one bucket with every other.
    ///
    /// Timed off the wall clock rather than the simulated one: a limit is
    /// about how fast requests actually arrive, and `FEHU_TIME_SCALE` must
    /// not be able to buy a client more of them.
    pub fn allow(&self, who: Option<UserId>) -> Decision {
        let since_start = self.started_at.elapsed().unwrap_or_default();
        self.limits.lock().unwrap_or_else(|e| e.into_inner()).take(
            who,
            since_start.as_millis().min(u128::from(u64::MAX)) as u64,
        )
    }

    /// Messages published to the stream since start-up.
    #[must_use]
    pub fn published(&self) -> u64 {
        self.stream
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .next_seq
            .saturating_sub(1)
    }

    /// Clients whose allowance the limiter is currently tracking.
    #[must_use]
    pub fn tracked_clients(&self) -> usize {
        self.limits
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .tracked()
    }

    /// Lock the market. A poisoned lock is recovered: the state is plain data
    /// and every mutation is either complete or not started.
    pub fn market(&self) -> MutexGuard<'_, Market> {
        self.market.lock().unwrap_or_else(|e| e.into_inner())
    }
}
