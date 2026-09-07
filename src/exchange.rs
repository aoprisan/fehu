//! The exchange: a [`Simulator`] as the reference price, synthetic liquidity
//! quoted around it, and trader orders that move it.
//!
//! Every tick the exchange
//!
//! 1. converts the traders' net flow since the last tick into a price-impact
//!    event on the simulator (§14 of `DESIGN.md`),
//! 2. steps the simulator, which yields the new reference price and volume,
//! 3. realises that volume as synthetic prints against the book (biased
//!    toward the direction of the tick's return), which also fills traders'
//!    resting orders that stood in the way, and
//! 4. re-quotes the synthetic maker ladder around the new reference,
//!    executing against any trader order it crosses.
//!
//! With no trader orders the price series is bit-identical to the bare
//! simulator's: the synthetic flow uses its own RNG stream.

use alloc::vec::Vec;
use core::fmt;
use core::time::Duration;

use rand_core::SeedableRng;
use rand_xoshiro::Xoshiro256PlusPlus;

use crate::book::{
    CancelError, Order, OrderBook, OrderError, OrderId, OrderKind, Owner, Party, Placement,
    Preview, Resting, Side, TimeInForce, Trade, TraderId, notional,
};
use crate::config::{Config, ConfigError, check_finite, check_range};
use crate::event::{Event, EventKind};
use crate::math::{self, exp, expm1, ln, pow, round, sqrt, tanh};
use crate::sim::{LoadError, Simulator, SimulatorRepr, Tick};
use crate::time::Timestamp;

/// Version of the exchange state layout and flow-RNG draw order.
pub const EXCHANGE_VERSION: u32 = 1;

/// Largest log move a single tick's impact may apply.
const MAX_IMPACT: f64 = 1.0;

/// How the synthetic maker ladder is quoted around the reference price.
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct LiquidityParams {
    /// Half of the bid–ask spread as a fraction of the reference price in a
    /// calm regime; it widens with `σ_t / σ`. Never less than one cent.
    /// `[0, 0.5]`.
    pub half_spread: f64,
    /// Price levels quoted on each side. `[1, 200]`.
    pub levels: u32,
    /// Distance between levels as a fraction of the reference price. Never
    /// less than one cent. `[0, 0.5]`.
    pub level_step: f64,
    /// Shares at the best level as a fraction of `VolumeParams::base_per_day`.
    /// `(0, 1]`.
    pub touch_depth: f64,
    /// Size multiplier per level away from the touch. `[0.1, 10]`.
    pub depth_growth: f64,
    /// Lognormal noise on each level's size. `[0, 3]`.
    pub size_noise: f64,
    /// A market order is executed as an immediate-or-cancel limit this far
    /// from the reference price. `[0, 1]`.
    pub market_collar: f64,
}

impl Default for LiquidityParams {
    fn default() -> Self {
        Self {
            half_spread: 0.0005,
            levels: 10,
            level_step: 0.0005,
            touch_depth: 0.001,
            depth_growth: 1.3,
            size_noise: 0.3,
            market_collar: 0.05,
        }
    }
}

/// How the simulator's tick volume is realised as synthetic prints.
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct FlowParams {
    /// Average shares per print. `[1, 10^12]`.
    pub mean_print: f64,
    /// Most prints in one tick. `[1, 10^4]`.
    pub max_prints: u32,
    /// How strongly print direction follows the tick's return:
    /// `P(buy) = ½ + ½·imbalance·tanh(r / σ_tick)`. `[0, 1]`.
    pub imbalance: f64,
}

impl Default for FlowParams {
    fn default() -> Self {
        Self {
            mean_print: 100.0,
            max_prints: 20,
            imbalance: 0.8,
        }
    }
}

/// Price impact of traders' net flow, square-root law by default:
/// `Δ ln P = coefficient · σ_day · (|Q| / ADV)^exponent`, signed like `Q`.
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ImpactParams {
    /// `Y` in the square-root law. `[0, 10]`.
    pub coefficient: f64,
    /// Exponent on `|Q| / ADV`. `[0.1, 2]`.
    pub exponent: f64,
    /// Share of the move applied to the fundamental (permanent) rather than
    /// to the spread (transient, reverting with `mean_reversion_speed`).
    /// `[0, 1]`.
    pub permanent_fraction: f64,
}

impl Default for ImpactParams {
    fn default() -> Self {
        Self {
            coefficient: 0.7,
            exponent: 0.5,
            permanent_fraction: 0.0,
        }
    }
}

/// Everything the exchange adds on top of [`Config`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TradingParams {
    /// The synthetic maker ladder.
    pub liquidity: LiquidityParams,
    /// The synthetic taker prints.
    pub flow: FlowParams,
    /// Traders' price impact.
    pub impact: ImpactParams,
}

impl TradingParams {
    /// Check every field against its documented range.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let l = &self.liquidity;
        check_range(
            "trading.liquidity.half_spread",
            l.half_spread,
            0.0,
            0.5,
            "must be in [0, 0.5]",
        )?;
        if !(1..=200).contains(&l.levels) {
            return Err(ConfigError::OutOfRange {
                field: "trading.liquidity.levels",
                reason: "must be in [1, 200]",
            });
        }
        check_range(
            "trading.liquidity.level_step",
            l.level_step,
            0.0,
            0.5,
            "must be in [0, 0.5]",
        )?;
        check_finite("trading.liquidity.touch_depth", l.touch_depth)?;
        if l.touch_depth <= 0.0 || l.touch_depth > 1.0 {
            return Err(ConfigError::OutOfRange {
                field: "trading.liquidity.touch_depth",
                reason: "must be in (0, 1]",
            });
        }
        check_range(
            "trading.liquidity.depth_growth",
            l.depth_growth,
            0.1,
            10.0,
            "must be in [0.1, 10]",
        )?;
        check_range(
            "trading.liquidity.size_noise",
            l.size_noise,
            0.0,
            3.0,
            "must be in [0, 3]",
        )?;
        check_range(
            "trading.liquidity.market_collar",
            l.market_collar,
            0.0,
            1.0,
            "must be in [0, 1]",
        )?;
        let f = &self.flow;
        check_range(
            "trading.flow.mean_print",
            f.mean_print,
            1.0,
            1e12,
            "must be in [1, 10^12]",
        )?;
        if !(1..=10_000).contains(&f.max_prints) {
            return Err(ConfigError::OutOfRange {
                field: "trading.flow.max_prints",
                reason: "must be in [1, 10^4]",
            });
        }
        check_range(
            "trading.flow.imbalance",
            f.imbalance,
            0.0,
            1.0,
            "must be in [0, 1]",
        )?;
        let i = &self.impact;
        check_range(
            "trading.impact.coefficient",
            i.coefficient,
            0.0,
            10.0,
            "must be in [0, 10]",
        )?;
        check_range(
            "trading.impact.exponent",
            i.exponent,
            0.1,
            2.0,
            "must be in [0.1, 2]",
        )?;
        check_range(
            "trading.impact.permanent_fraction",
            i.permanent_fraction,
            0.0,
            1.0,
            "must be in [0, 1]",
        )
    }
}

/// What one exchange tick produced.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct StepReport {
    /// The reference tick. `volume` is the tape volume of the step: every
    /// synthetic print plus every trader execution since the previous tick.
    /// With no traders it equals the simulator's volume.
    pub tick: Tick,
    /// Everything that traded during the step, in order: synthetic prints
    /// first, then executions from re-quoting the ladder.
    pub trades: Vec<Trade>,
    /// Log move applied to the reference by traders' flow this tick.
    pub impact: f64,
}

/// A simulator with an order book around it. See the [module docs](self).
#[derive(Clone, Debug)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(try_from = "ExchangeRepr", into = "ExchangeRepr")
)]
pub struct Exchange {
    sim: Simulator,
    params: TradingParams,
    book: OrderBook,
    /// Separate stream for the synthetic flow, so trading never changes the
    /// reference path.
    flow_rng: Xoshiro256PlusPlus,
    /// Net signed shares traders took from synthetic liquidity since the
    /// last tick.
    pending_flow: i64,
    /// Shares traders executed between ticks, added to the next tick's
    /// volume.
    interim_volume: u64,
    /// Reference price of the last tick (or the start price).
    reference_cents: i64,
    /// `σ √(1/trading days)`, for impact.
    sigma_day: f64,
    /// Expected daily volume, for impact.
    adv: f64,
}

impl Exchange {
    /// Create an exchange from a validated config, trading parameters and a
    /// seed. The simulator uses the seed exactly as [`Simulator::new`] does;
    /// the flow RNG is the same generator after a `long_jump`.
    ///
    /// # Errors
    /// The first [`ConfigError`] found in either parameter set.
    pub fn new(config: Config, params: TradingParams, seed: u64) -> Result<Self, ConfigError> {
        params.validate()?;
        let sim = Simulator::new(config, seed)?;
        let mut flow_rng = Xoshiro256PlusPlus::seed_from_u64(seed);
        flow_rng.long_jump();
        let mut ex = Self::assemble(sim, params, OrderBook::new(), flow_rng, 0, 0);
        let mut trades = Vec::new();
        ex.requote(&mut trades);
        Ok(ex)
    }

    fn assemble(
        sim: Simulator,
        params: TradingParams,
        book: OrderBook,
        flow_rng: Xoshiro256PlusPlus,
        pending_flow: i64,
        interim_volume: u64,
    ) -> Self {
        let cfg = sim.config();
        let days = cfg
            .market_hours
            .as_ref()
            .map_or(365.25, crate::time::MarketHours::trading_days_per_year);
        let sigma_day = cfg.volatility * sqrt(1.0 / days);
        let adv = cfg.volume.base_per_day
            * (1.0 + cfg.volume.return_sensitivity * sqrt(2.0 / core::f64::consts::PI));
        let reference_cents = sim.snapshot().price_cents;
        Self {
            sim,
            params,
            book,
            flow_rng,
            pending_flow,
            interim_volume,
            reference_cents,
            sigma_day,
            adv,
        }
    }

    /// The reference price process.
    #[must_use]
    pub fn simulator(&self) -> &Simulator {
        &self.sim
    }

    /// Mutable access to the simulator, e.g. to push events or to generate
    /// history with [`Simulator::coarse_candles`] or [`Simulator::advance`]
    /// before trading starts. Stepping the simulator this way is much
    /// cheaper than stepping the exchange and, with no traders, yields the
    /// same ticks; call [`resync`](Self::resync) afterwards so the ladder and
    /// reference price catch up before any order is submitted.
    pub fn simulator_mut(&mut self) -> &mut Simulator {
        &mut self.sim
    }

    /// Re-read the reference price from the simulator and re-quote the
    /// synthetic ladder around it. Needed only after stepping the simulator
    /// through [`simulator_mut`](Self::simulator_mut); [`step`](Self::step)
    /// does this itself. Returns the trades the new quotes executed against
    /// traders' resting orders.
    pub fn resync(&mut self) -> Vec<Trade> {
        self.reference_cents = self.sim.snapshot().price_cents;
        let mut trades = Vec::new();
        self.requote(&mut trades);
        for t in &trades {
            self.interim_volume = self.interim_volume.saturating_add(t.qty);
        }
        trades
    }

    /// The trading parameters.
    #[must_use]
    pub fn params(&self) -> &TradingParams {
        &self.params
    }

    /// The book.
    #[must_use]
    pub fn book(&self) -> &OrderBook {
        &self.book
    }

    /// Skip unused order ids below `minimum`, without changing liquidity or
    /// the price process. Used to coordinate trader ids across exchanges.
    pub fn advance_order_id(&mut self, minimum: OrderId) {
        self.book.advance_order_id(minimum);
    }

    /// Wall time the exchange has been advanced to.
    #[must_use]
    pub fn clock(&self) -> Timestamp {
        self.sim.clock()
    }

    /// Reference price of the last tick.
    #[must_use]
    pub fn reference_cents(&self) -> i64 {
        self.reference_cents
    }

    /// Traders' net signed flow that will hit the reference on the next tick.
    #[must_use]
    pub fn pending_flow(&self) -> i64 {
        self.pending_flow
    }

    /// Submit a trader order. It matches immediately against the book (the
    /// synthetic ladder and other traders' orders); a `Gtc` limit remainder
    /// rests until it fills, is cancelled, or the ladder crosses it. Market
    /// orders become immediate-or-cancel limits `market_collar` away from
    /// the reference.
    ///
    /// # Errors
    /// See [`OrderError`]. Synthetic owners are rejected.
    pub fn submit(&mut self, order: Order) -> Result<Placement, OrderError> {
        if order.owner == Owner::Synthetic {
            return Err(OrderError::SyntheticOwner);
        }
        OrderBook::validate(&order)?;
        let order = match order.kind {
            OrderKind::Market => {
                let collar = self.params.liquidity.market_collar;
                let r = self.reference_cents as f64;
                let price_cents = match order.side {
                    Side::Buy => libm::ceil(r * (1.0 + collar)) as i64,
                    Side::Sell => libm::floor(r * (1.0 - collar)) as i64,
                }
                .clamp(1, crate::book::MAX_PRICE_CENTS);
                let tif = match order.tif {
                    TimeInForce::Fok => TimeInForce::Fok,
                    _ => TimeInForce::Ioc,
                };
                Order {
                    kind: OrderKind::Limit { price_cents },
                    tif,
                    ..order
                }
            }
            OrderKind::Limit { .. } => order,
        };
        let placement = self.book.submit(order, self.sim.clock())?;
        self.account(&placement.trades);
        for t in &placement.trades {
            self.interim_volume = self.interim_volume.saturating_add(t.qty);
        }
        Ok(placement)
    }

    /// Cancel a trader's resting order.
    ///
    /// # Errors
    /// [`CancelError::Unknown`] if nothing rests under that id,
    /// [`CancelError::NotOwner`] if it belongs to someone else.
    pub fn cancel(&mut self, id: OrderId, trader: TraderId) -> Result<Resting, CancelError> {
        match self.book.get(id) {
            None => Err(CancelError::Unknown),
            Some(o) if o.owner != Owner::Trader(trader) => Err(CancelError::NotOwner),
            Some(_) => self.book.cancel(id).ok_or(CancelError::Unknown),
        }
    }

    /// Cancel every resting order of a trader.
    pub fn cancel_all(&mut self, trader: TraderId) -> Vec<Resting> {
        self.book.cancel_all(Owner::Trader(trader))
    }

    /// What a market order would execute right now, within the collar.
    #[must_use]
    pub fn preview_market(&self, side: Side, qty: u64) -> Preview {
        let collar = self.params.liquidity.market_collar;
        let r = self.reference_cents as f64;
        let limit = match side {
            Side::Buy => libm::ceil(r * (1.0 + collar)) as i64,
            Side::Sell => libm::floor(r * (1.0 - collar)) as i64,
        };
        self.book.preview(side, qty, Some(limit))
    }

    /// Advance wall time by `dur` and yield every tick within it.
    pub fn advance(&mut self, dur: Duration) -> impl Iterator<Item = StepReport> + '_ {
        let target = self.sim.clock() + dur;
        core::iter::from_fn(move || {
            (self.sim.next_tick_ts() <= target).then(|| {
                let r = self.step();
                // `step` moves the simulator clock to the tick; keep the
                // requested wall time so a partial tick is not lost.
                self.sim.set_clock(target);
                r
            })
        })
    }

    /// Advance the reference process without matching or requoting the book.
    /// Resting orders and their queue positions remain unchanged. Tick volume
    /// includes only executions already submitted before this advance.
    /// Call [`resync`](Self::resync) and settle its returned trades when
    /// matching resumes, before accepting new orders against the old quotes.
    pub fn advance_without_matching(
        &mut self,
        dur: Duration,
    ) -> impl Iterator<Item = StepReport> + '_ {
        let target = self.sim.clock() + dur;
        core::iter::from_fn(move || {
            (self.sim.next_tick_ts() <= target).then(|| {
                let impact = self.apply_impact();
                let tick = self.sim.step();
                self.reference_cents = tick.price_cents;
                let volume = core::mem::take(&mut self.interim_volume);
                self.sim.set_clock(target);
                StepReport {
                    tick: Tick { volume, ..tick },
                    trades: Vec::new(),
                    impact,
                }
            })
        })
    }

    /// Emit exactly one tick.
    pub fn step(&mut self) -> StepReport {
        let impact = self.apply_impact();
        let p_old = self.reference_cents;
        let tick = self.sim.step();
        let mut trades = Vec::new();
        self.synthetic_flow(&tick, p_old, &mut trades);
        self.reference_cents = tick.price_cents;
        self.requote(&mut trades);
        let step_volume = trades.iter().fold(0u64, |a, t| a.saturating_add(t.qty));
        let volume = step_volume.saturating_add(self.interim_volume);
        self.interim_volume = 0;
        StepReport {
            tick: Tick { volume, ..tick },
            trades,
            impact,
        }
    }

    /// Book-keep trader flow from a batch of trades.
    fn account(&mut self, trades: &[Trade]) {
        for t in trades {
            self.pending_flow = self.pending_flow.saturating_add(t.trader_flow());
        }
    }

    /// Turn the pending flow into events on the simulator. Returns the log
    /// move.
    fn apply_impact(&mut self) -> f64 {
        if self.pending_flow == 0 {
            return 0.0;
        }
        let q = self.pending_flow as f64;
        self.pending_flow = 0;
        let i = self.params.impact;
        let size = pow(q.abs() / self.adv, i.exponent);
        let mv = (i.coefficient * self.sigma_day * size).min(MAX_IMPACT) * q.signum();
        if mv == 0.0 {
            return 0.0;
        }
        let at = self.sim.next_tick_ts();
        let permanent = mv * i.permanent_fraction;
        let transient = mv - permanent;
        if transient != 0.0 {
            self.sim
                .push_event(Event {
                    at,
                    kind: EventKind::Jump(expm1(transient)),
                })
                .expect("finite jump above -1");
        }
        if permanent != 0.0 {
            self.sim
                .push_event(Event {
                    at,
                    kind: EventKind::FundamentalShift(permanent),
                })
                .expect("finite shift");
        }
        mv
    }

    /// Realise `tick.volume` as prints against the current book. Draw order:
    /// one uniform for the print count, one uniform per print for its
    /// weight, one uniform per print for its side.
    fn synthetic_flow(&mut self, tick: &Tick, p_old: i64, out: &mut Vec<Trade>) {
        let v = tick.volume;
        if v == 0 {
            return;
        }
        let f = self.params.flow;
        let r = ln(tick.price_cents as f64 / p_old as f64);
        let tick_std = self.sim.tick_std();
        let bias = if tick_std > 0.0 {
            tanh(r / tick_std)
        } else {
            0.0
        };
        let p_buy = 0.5 + 0.5 * f.imbalance * bias;
        let expected = (v as f64 / f.mean_print)
            .max(1.0)
            .min(f64::from(f.max_prints));
        let n = math::poisson(&mut self.flow_rng, expected - 1.0) + 1;
        let n = n.min(f.max_prints).max(1) as usize;
        // Weights in [0.5, 1.5) so no print is empty.
        let mut weights = Vec::with_capacity(n);
        let mut total = 0.0;
        for _ in 0..n {
            let w = 0.5 + math::uniform(&mut self.flow_rng);
            weights.push(w);
            total += w;
        }
        let mut left = v;
        for (k, w) in weights.iter().enumerate() {
            let qty = if k + 1 == n {
                left
            } else {
                let q = round(v as f64 * w / total);
                let q = if q < 1.0 { 1 } else { q as u64 };
                q.min(left)
            };
            let side = if math::uniform(&mut self.flow_rng) < p_buy {
                Side::Buy
            } else {
                Side::Sell
            };
            left -= qty;
            if qty == 0 {
                continue;
            }
            self.print(side, qty, tick, out);
        }
    }

    /// One synthetic print: a market order against the book; whatever the
    /// visible book cannot absorb prints against hidden liquidity at the
    /// tick's reference price.
    fn print(&mut self, side: Side, qty: u64, tick: &Tick, out: &mut Vec<Trade>) {
        let placement = self
            .book
            .submit(Order::market(Owner::Synthetic, side, qty), tick.ts)
            .expect("validated");
        self.account(&placement.trades);
        let taker = Party {
            order: placement.id,
            owner: Owner::Synthetic,
        };
        out.extend(placement.trades);
        if placement.remaining > 0 {
            out.push(Trade {
                ts: tick.ts,
                price_cents: tick.price_cents,
                qty: placement.remaining,
                taker_side: side,
                taker,
                maker: Party {
                    order: OrderId::HIDDEN,
                    owner: Owner::Synthetic,
                },
            });
        }
    }

    /// Replace the synthetic ladder around the reference price. Draw order:
    /// one normal per level, bid then ask, best level first.
    fn requote(&mut self, out: &mut Vec<Trade>) {
        self.book.cancel_all(Owner::Synthetic);
        let l = self.params.liquidity;
        let cfg = self.sim.config();
        let r = self.reference_cents;
        let rf = r as f64;
        let snap = self.sim.snapshot();
        let regime = if cfg.volatility > 0.0 {
            (snap.annual_vol / cfg.volatility).max(1.0)
        } else {
            1.0
        };
        let hs = l.half_spread * regime;
        let bid1 = (libm::floor(rf * (1.0 - hs)) as i64).min(r - 1);
        let ask1 = (libm::ceil(rf * (1.0 + hs)) as i64).max(r + 1);
        let step = (round(rf * l.level_step) as i64).max(1);
        let touch = l.touch_depth * cfg.volume.base_per_day;
        let ts = self.sim.clock();
        let mut growth = 1.0;
        for k in 0..l.levels {
            let offset = step * i64::from(k);
            for side in [Side::Buy, Side::Sell] {
                let z = math::normal(&mut self.flow_rng);
                let size =
                    touch * growth * exp(l.size_noise * z - 0.5 * l.size_noise * l.size_noise);
                let qty = round(size).max(1.0);
                let qty = if qty >= crate::book::MAX_ORDER_QTY as f64 {
                    crate::book::MAX_ORDER_QTY
                } else {
                    qty as u64
                };
                let price = match side {
                    Side::Buy => bid1 - offset,
                    Side::Sell => ask1.saturating_add(offset),
                };
                if !(1..=crate::book::MAX_PRICE_CENTS).contains(&price) {
                    continue;
                }
                let placement = self
                    .book
                    .submit(Order::limit(Owner::Synthetic, side, price, qty), ts)
                    .expect("validated");
                self.account(&placement.trades);
                out.extend(placement.trades);
            }
            growth *= l.depth_growth;
        }
    }
}

/// Serialisable form of [`Exchange`].
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ExchangeRepr {
    /// [`EXCHANGE_VERSION`] at save time.
    pub version: u32,
    /// The simulator, in its own versioned form.
    pub sim: SimulatorRepr,
    params: TradingParams,
    book: OrderBook,
    flow_rng: Xoshiro256PlusPlus,
    pending_flow: i64,
    interim_volume: u64,
}

impl From<Exchange> for ExchangeRepr {
    fn from(e: Exchange) -> Self {
        Self {
            version: EXCHANGE_VERSION,
            sim: e.sim.into(),
            params: e.params,
            book: e.book,
            flow_rng: e.flow_rng,
            pending_flow: e.pending_flow,
            interim_volume: e.interim_volume,
        }
    }
}

impl TryFrom<ExchangeRepr> for Exchange {
    type Error = LoadError;

    fn try_from(r: ExchangeRepr) -> Result<Self, LoadError> {
        if r.version != EXCHANGE_VERSION {
            return Err(LoadError::VersionMismatch {
                found: r.version,
                expected: EXCHANGE_VERSION,
            });
        }
        r.params.validate().map_err(LoadError::Config)?;
        let sim = Simulator::try_from(r.sim)?;
        Ok(Self::assemble(
            sim,
            r.params,
            r.book,
            r.flow_rng,
            r.pending_flow,
            r.interim_volume,
        ))
    }
}

/// Average-cost position and cash ledger for one trader in one symbol.
/// Long and short positions are both supported.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Position {
    /// Shares held; negative when short.
    pub qty: i64,
    /// Cash paid in or out by trades in this symbol (saturating).
    pub cash_cents: i64,
    /// Cost basis of the open position: `|qty| × average cost`, in cents.
    pub cost_cents: i64,
    /// Profit realised by closing trades, in cents.
    pub realised_pnl_cents: i64,
}

impl Position {
    /// Apply an execution.
    pub fn apply(&mut self, side: Side, qty: u64, price_cents: i64) {
        let q = i64::try_from(qty).unwrap_or(i64::MAX);
        let value = notional(price_cents, qty);
        self.cash_cents = self.cash_cents.saturating_sub(side.sign() * value);
        let signed = side.sign() * q;
        let same_direction = self.qty == 0 || (self.qty > 0) == (signed > 0);
        if same_direction {
            self.qty = self.qty.saturating_add(signed);
            self.cost_cents = self.cost_cents.saturating_add(value);
            return;
        }
        // Closing (part of) the position, possibly flipping through zero.
        let closing = q.min(self.qty.abs());
        let avg = self.avg_cost_cents().unwrap_or(price_cents as f64);
        let pnl = (price_cents as f64 - avg) * closing as f64 * self.qty.signum() as f64;
        self.realised_pnl_cents = self.realised_pnl_cents.saturating_add(round(pnl) as i64);
        let basis_out = round(avg * closing as f64) as i64;
        self.cost_cents = (self.cost_cents - basis_out).max(0);
        self.qty += -self.qty.signum() * closing;
        let opening = q - closing;
        if opening > 0 {
            self.qty = signed.signum() * opening;
            self.cost_cents = notional(price_cents, opening as u64);
        }
        if self.qty == 0 {
            self.cost_cents = 0;
        }
    }

    /// Average cost per share of the open position.
    #[must_use]
    pub fn avg_cost_cents(&self) -> Option<f64> {
        (self.qty != 0).then(|| self.cost_cents as f64 / self.qty.abs() as f64)
    }

    /// `qty × mark`, saturating.
    #[must_use]
    pub fn market_value_cents(&self, mark_cents: i64) -> i64 {
        let v = i128::from(self.qty) * i128::from(mark_cents);
        i64::try_from(v).unwrap_or(if v < 0 { i64::MIN } else { i64::MAX })
    }

    /// Unrealised profit at `mark`.
    #[must_use]
    pub fn unrealised_pnl_cents(&self, mark_cents: i64) -> i64 {
        let value = self.market_value_cents(mark_cents);
        if self.qty >= 0 {
            value.saturating_sub(self.cost_cents)
        } else {
            self.cost_cents.saturating_add(value)
        }
    }
}

impl fmt::Display for Position {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} shares, cash {} cents, realised {} cents",
            self.qty, self.cash_cents, self.realised_pnl_cents
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn position_accounting() {
        let mut p = Position::default();
        p.apply(Side::Buy, 100, 1_000);
        p.apply(Side::Buy, 100, 1_200);
        assert_eq!(p.qty, 200);
        assert_eq!(p.avg_cost_cents(), Some(1_100.0));
        assert_eq!(p.cash_cents, -220_000);
        p.apply(Side::Sell, 150, 1_300);
        assert_eq!(p.qty, 50);
        assert_eq!(p.realised_pnl_cents, 150 * 200);
        assert_eq!(p.cost_cents, 55_000);
        assert_eq!(p.unrealised_pnl_cents(1_300), 50 * 200);
        // Flip through zero into a short.
        p.apply(Side::Sell, 80, 1_000);
        assert_eq!(p.qty, -30);
        assert_eq!(p.realised_pnl_cents, 150 * 200 - 50 * 100);
        assert_eq!(p.avg_cost_cents(), Some(1_000.0));
        assert_eq!(p.unrealised_pnl_cents(900), 30 * 100);
        p.apply(Side::Buy, 30, 900);
        assert_eq!(p.qty, 0);
        assert_eq!(p.realised_pnl_cents, 150 * 200 - 50 * 100 + 30 * 100);
        assert_eq!(p.cost_cents, 0);
    }

    #[test]
    fn params_validate() {
        TradingParams::default().validate().unwrap();
        let bad = TradingParams {
            flow: FlowParams {
                imbalance: 1.5,
                ..FlowParams::default()
            },
            ..TradingParams::default()
        };
        assert!(matches!(
            bad.validate(),
            Err(ConfigError::OutOfRange {
                field: "trading.flow.imbalance",
                ..
            })
        ));
    }
}
