//! The seeded symbols, their simulators and bar history, and the shared
//! application state.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fehu::{
    Candle, Candles, Config, Interval, JumpParams, Simulator, Snapshot, Tick, Timestamp,
    VolumeParams,
};
use serde::Serialize;
use tokio::sync::broadcast;

use crate::events::EventRecord;

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
    /// RNG seed. Same seed + same events ⇒ same prices, every run.
    pub seed: u64,
}

/// A symbol's metadata plus the simulator config it is created with.
pub struct SymbolSpec {
    pub info: SymbolInfo,
    pub config: Config,
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
        },
        SymbolSpec {
            info: SymbolInfo {
                symbol: "NBLA",
                name: "Nebula Robotics",
                sector: "Technology",
                description: "Pre-profit robotics darling. High volatility, big drift, frequent jumps.",
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
        },
        SymbolSpec {
            info: SymbolInfo {
                symbol: "HLIO",
                name: "Helio Energy",
                sector: "Energy",
                description: "Solar and storage utility. Commodity-driven, moderate volatility.",
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
        },
        SymbolSpec {
            info: SymbolInfo {
                symbol: "PXCO",
                name: "Pax Consumer Co",
                sector: "Consumer Staples",
                description: "Household brands. Defensive: low volatility, shocks fade slowly.",
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
        },
    ]
}

/// What one call to [`SymbolState::advance_to`] produced.
#[derive(Clone, Copy, Debug, Default)]
pub struct Advanced {
    /// Last tick emitted, if any.
    pub last: Option<Tick>,
    /// Number of ticks emitted.
    pub ticks: u64,
    /// Per interval in [`Interval::ALL`] order: did at least one bar close?
    pub closed: [bool; 4],
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

/// One symbol: its simulator, the bars aggregated from its ticks, and the
/// coarse daily bars generated as pre-history at start-up.
pub struct SymbolState {
    pub info: SymbolInfo,
    pub sim: Simulator,
    /// 1 m / 5 m / 1 h / 1 d bars aggregated from fine ticks.
    pub candles: Candles,
    /// Daily bars from coarse mode, before the fine-tick history starts.
    pub coarse_daily: Vec<Candle>,
    pub last_tick: Option<Tick>,
    /// Fine ticks emitted since start-up (warm-up included).
    pub ticks_total: u64,
}

impl SymbolState {
    fn new(spec: SymbolSpec, max_bars: usize) -> Self {
        let sim = Simulator::new(spec.config, spec.info.seed).expect("seeded configs are valid");
        Self {
            info: spec.info,
            sim,
            candles: Candles::new(max_bars),
            coarse_daily: Vec::new(),
            last_tick: None,
            ticks_total: 0,
        }
    }

    /// Generate `coarse_days` daily bars in coarse mode, then tick finely up
    /// to `until` so the intraday intervals have history too.
    fn warm_up(&mut self, coarse_days: usize, until: Timestamp) {
        self.coarse_daily = self
            .sim
            .coarse_candles(Interval::D1)
            .take(coarse_days)
            .collect();
        self.advance_to(until);
    }

    /// Advance the simulator's wall clock to `target` (no-op if it is not in
    /// the future), aggregating every tick into the bars.
    pub fn advance_to(&mut self, target: Timestamp) -> Advanced {
        let mut out = Advanced::default();
        let dur = target - self.sim.clock();
        if dur <= 0 {
            return out;
        }
        let Self { sim, candles, .. } = self;
        for tick in sim.advance(Duration::from_millis(dur as u64)) {
            let closed = candles.push(&tick);
            for (flag, c) in out.closed.iter_mut().zip(closed) {
                *flag |= c.is_some();
            }
            out.last = Some(tick);
            out.ticks += 1;
        }
        if out.last.is_some() {
            self.last_tick = out.last;
            self.ticks_total += out.ticks;
        }
        out
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

    /// Current quote.
    pub fn quote(&self) -> Quote {
        let snap = self.sim.snapshot();
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
        }
    }
}

/// Everything behind the mutex: the symbols and the event log.
pub struct Market {
    pub symbols: Vec<SymbolState>,
    /// Most recent events, oldest first.
    pub events: VecDeque<EventRecord>,
    event_cap: usize,
    next_event_id: u64,
}

impl Market {
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
    },
    /// An event was accepted.
    Event(EventRecord),
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
        }
    }
}

fn env_parse<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
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
            .map(|spec| {
                let mut s = SymbolState::new(spec, options.max_bars);
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
            }),
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
