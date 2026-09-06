//! The simulator: state, per-tick algorithm and the public stepping API.

use core::time::Duration;

use rand_core::SeedableRng;
use rand_xoshiro::Xoshiro256PlusPlus;

use crate::config::{Config, ConfigError, Derived};
use crate::math::{self, exp, expm1, ln, round, sqrt};
use crate::time::Timestamp;

/// Version of the state layout and RNG draw order. Bumped whenever either
/// changes; saved states with a different version are rejected on load.
pub const STATE_VERSION: u32 = 1;

/// Lowest latent price the process can reach: half a cent.
const MIN_LOG_PRICE: f64 = -5.298_317_366_548_036; // ln(0.005)
/// Highest latent price: 10^13 dollars.
const MAX_LOG_PRICE: f64 = 29.933_606_208_922_594; // ln(1e13)
const MAX_PRICE_CENTS: i64 = 1_000_000_000_000_000;

/// One simulated price observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Tick {
    /// When this price was observed.
    pub ts: Timestamp,
    /// Price in cents, always `≥ 1`.
    pub price_cents: i64,
    /// Shares traded during this tick.
    pub volume: u64,
}

/// Read-only view of the latent state, for tests, tuning and UI.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Snapshot {
    /// Timestamp of the last emitted tick (or the start if none yet).
    pub ts: Timestamp,
    /// Current price in cents.
    pub price_cents: i64,
    /// Current fundamental value in cents.
    pub fundamental_cents: i64,
    /// `ln P − ln V`.
    pub log_spread: f64,
    /// Effective annualised volatility used for the last step.
    pub annual_vol: f64,
}

/// The price process. See `DESIGN.md` for the model.
#[derive(Clone, Debug)]
pub struct Simulator {
    config: Config,
    derived: Derived,
    rng: Xoshiro256PlusPlus,
    /// Wall time the simulator has been advanced to.
    clock: Timestamp,
    /// Timestamp of the next tick to emit.
    next_ts: Timestamp,
    /// Timestamp of the last emitted tick.
    last_ts: Option<Timestamp>,
    /// `p = ln P` (dollars).
    log_price: f64,
    /// `f = ln V`.
    log_fund: f64,
    /// `f*`, the fundamental's target.
    log_fund_target: f64,
    /// Effective annualised vol used in the last step (for `Snapshot`).
    last_vol: f64,
}

impl Simulator {
    /// Create a simulator from a validated config and a seed.
    ///
    /// # Errors
    /// Returns the first [`ConfigError`] found by [`Config::validate`].
    pub fn new(config: Config, seed: u64) -> Result<Self, ConfigError> {
        let derived = Derived::new(&config)?;
        let log_price = ln(config.start_price_cents as f64 / 100.0);
        let start = config.start_ts;
        Ok(Self {
            last_vol: config.volatility,
            config,
            derived,
            rng: Xoshiro256PlusPlus::seed_from_u64(seed),
            clock: start,
            next_ts: start,
            last_ts: None,
            log_price,
            log_fund: log_price,
            log_fund_target: log_price,
        })
    }

    /// The configuration this simulator was built with.
    #[must_use]
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Wall time the simulator has been advanced to.
    #[must_use]
    pub fn clock(&self) -> Timestamp {
        self.clock
    }

    /// Timestamp the next call to [`step`](Self::step) will emit.
    #[must_use]
    pub fn next_tick_ts(&self) -> Timestamp {
        self.next_ts
    }

    /// Latent-state view.
    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            ts: self.last_ts.unwrap_or(self.next_ts),
            price_cents: cents(self.log_price),
            fundamental_cents: cents(self.log_fund),
            log_spread: self.log_price - self.log_fund,
            annual_vol: self.last_vol,
        }
    }

    /// Set the fundamental's target value in cents (clamped to `≥ 1`). The
    /// fundamental relaxes toward it at `Config::fundamental_speed`.
    pub fn set_fundamental_input(&mut self, target_cents: i64) {
        let target = target_cents.clamp(1, MAX_PRICE_CENTS);
        self.log_fund_target = ln(target as f64 / 100.0);
    }

    /// Advance wall time by `dur` and lazily yield every tick that falls within
    /// it. Dropping the iterator early is safe: the remaining ticks come out of
    /// the next `advance`/`step`.
    pub fn advance(&mut self, dur: Duration) -> impl Iterator<Item = Tick> + '_ {
        self.clock = self.clock + dur;
        core::iter::from_fn(move || (self.next_ts <= self.clock).then(|| self.step()))
    }

    /// Emit exactly one tick and move the clock to it.
    pub fn step(&mut self) -> Tick {
        let ts = self.next_ts;
        let model_secs = self.derived.tick_secs;
        let delta = model_secs / self.derived.year_secs;

        // 3. Fundamental: target grows, value relaxes toward it.
        let spread = self.log_price - self.log_fund;
        self.log_fund_target += self.config.drift * delta;
        let relax = -expm1(-self.config.fundamental_speed * delta);
        self.log_fund += (self.log_fund_target - self.log_fund) * relax;

        // 5. Effective vol.
        let sigma_t = self.config.volatility;
        self.last_vol = sigma_t;

        // 6. Exact OU step on the spread.
        let theta = self.config.mean_reversion_speed;
        let z = math::normal(&mut self.rng);
        let ou_std = sigma_t * sqrt(-expm1(-2.0 * theta * delta) / (2.0 * theta));
        let spread = spread * exp(-theta * delta) + ou_std * z;

        // 9. Recombine and clamp.
        self.log_price = (self.log_fund + spread).clamp(MIN_LOG_PRICE, MAX_LOG_PRICE);
        debug_assert!(self.log_price.is_finite());

        // 11. Emit and schedule.
        self.last_ts = Some(ts);
        if self.clock < ts {
            self.clock = ts;
        }
        self.next_ts = Timestamp(ts.0.saturating_add(self.derived.tick_ms));
        Tick {
            ts,
            price_cents: cents(self.log_price),
            volume: 0,
        }
    }
}

/// Round a log-dollar price to cents, clamped to `[1, 10^15]`.
fn cents(log_price: f64) -> i64 {
    let c = round(100.0 * exp(log_price));
    if c.is_nan() {
        return 1;
    }
    (c as i64).clamp(1, MAX_PRICE_CENTS)
}
