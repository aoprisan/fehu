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
use crate::save::{MarketSave, STATE_VERSION, Save, SymbolSave};
use crate::trading::{BookDto, FillRecord, HoldingDto, OrderRecord, TradeDto, Trader};

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
    /// Every order the log still holds, by order id.
    orders: BTreeMap<u64, OrderRecord>,
    /// Order ids in the order they were accepted, for eviction.
    order_ids: VecDeque<u64>,
    /// `(trader, client_order_id)` → order id, for idempotent submission.
    client_order_ids: BTreeMap<(TraderId, String), u64>,
    fill_log: usize,
    ledger_log: usize,
    order_log: usize,
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
                    fills.extend(trader.apply_trade(account, sym, t));
                }
            }
        }
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
            orders: BTreeMap::new(),
            order_ids: VecDeque::new(),
            client_order_ids: BTreeMap::new(),
            fill_log: options.fill_log,
            ledger_log: options.ledger_log,
            order_log: options.order_log.max(1),
            price_limit_pct: options.price_limit_pct.max(0.0),
            halt_secs: options.halt_secs,
        };
        // Through the same door as a live order, so the client-id index and
        // the eviction order come out the same.
        let responses: BTreeMap<u64, crate::trading::OrderResponse> =
            save.order_responses.into_iter().collect();
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

/// What goes out over the SSE stream.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamMessage {
    /// First message on every connection.
    Hello {
        sim_now_ms: i64,
        time_scale: f64,
        quotes: Vec<Quote>,
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
        }
    }
}

impl Options {
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
    pub tx: broadcast::Sender<StreamMessage>,
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
                // Every symbol trades on the same calendar, if there is one.
                spec.config.market_hours = options.market_hours;
                let mut s = SymbolState::new(spec, options.max_bars, options.tape_len);
                s.warm_up(options.history_days, now);
                s
            })
            .collect();
        let (tx, _) = broadcast::channel(4096);
        Arc::new(Self {
            clock: SimClock {
                wall_epoch: Instant::now(),
                sim_epoch: now,
                scale: options.time_scale,
            },
            started_at: SystemTime::now(),
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
                orders: BTreeMap::new(),
                order_ids: VecDeque::new(),
                client_order_ids: BTreeMap::new(),
                fill_log: options.fill_log,
                ledger_log: options.ledger_log,
                order_log: options.order_log.max(1),
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
        Arc::new(Self {
            clock: SimClock {
                wall_epoch: Instant::now(),
                sim_epoch: now,
                scale: options.time_scale,
            },
            started_at: SystemTime::now(),
            market: Mutex::new(market),
            tx,
            options,
        })
    }

    /// Lock the market. A poisoned lock is recovered: the state is plain data
    /// and every mutation is either complete or not started.
    pub fn market(&self) -> MutexGuard<'_, Market> {
        self.market.lock().unwrap_or_else(|e| e.into_inner())
    }
}
