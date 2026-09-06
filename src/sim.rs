//! The simulator: state, per-tick algorithm and the public stepping API.

use alloc::collections::BinaryHeap;
use alloc::vec::Vec;
use core::cmp::Reverse;
use core::time::Duration;

use rand_core::SeedableRng;
use rand_xoshiro::Xoshiro256PlusPlus;

use crate::config::{Config, ConfigError, Derived};
use crate::event::{Decaying, Event, EventError, EventKind, PRUNE_BELOW, Queued};
use crate::math::{self, exp, expm1, ln, ln_1p, round, sqrt};
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
    /// Sum of active drift-shift effects, annualised.
    pub drift_effect: f64,
    /// Sum of active vol-shift effects, annualised.
    pub vol_effect: f64,
    /// Events queued and not yet applied.
    pub pending_events: usize,
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
    /// GARCH per-tick variance `h`.
    variance: f64,
    /// Effective annualised vol used in the last step (for `Snapshot`).
    last_vol: f64,
    /// Active drift-shift effects.
    drift_effects: Vec<Decaying>,
    /// Active vol-shift effects.
    vol_effects: Vec<Decaying>,
    /// Events not yet applied, earliest first.
    pending: BinaryHeap<Reverse<Queued>>,
    /// Sequence number for the next pushed event.
    next_seq: u64,
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
            variance: derived.var_unc,
            config,
            derived,
            rng: Xoshiro256PlusPlus::seed_from_u64(seed),
            clock: start,
            next_ts: start,
            last_ts: None,
            log_price,
            log_fund: log_price,
            log_fund_target: log_price,
            drift_effects: Vec::new(),
            vol_effects: Vec::new(),
            pending: BinaryHeap::new(),
            next_seq: 0,
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
            drift_effect: self.drift_effects.iter().map(|e| e.amplitude).sum(),
            vol_effect: self.vol_effects.iter().map(|e| e.amplitude).sum(),
            pending_events: self.pending.len(),
        }
    }

    /// Queue an event. It is applied at the first tick with `ts ≥ event.at`;
    /// events with equal timestamps apply in push order.
    ///
    /// # Errors
    /// Rejects non-finite magnitudes, `Jump(pct)` with `pct ≤ -1`, and shifts
    /// with a zero half-life.
    pub fn push_event(&mut self, event: Event) -> Result<(), EventError> {
        event.kind.validate()?;
        self.pending.push(Reverse(Queued {
            at: event.at,
            seq: self.next_seq,
            kind: event.kind,
        }));
        self.next_seq += 1;
        Ok(())
    }

    /// Apply every queued event due at or before `ts`.
    fn apply_due_events(&mut self, ts: Timestamp) {
        while let Some(Reverse(q)) = self.pending.peek() {
            if q.at > ts {
                break;
            }
            let Reverse(q) = self.pending.pop().expect("peeked");
            match q.kind {
                EventKind::Jump(pct) => self.log_price += ln_1p(pct),
                EventKind::DriftShift { delta, half_life } => {
                    self.drift_effects.push(Decaying::new(delta, half_life));
                }
                EventKind::VolShift { delta, half_life } => {
                    self.vol_effects.push(Decaying::new(delta, half_life));
                }
                EventKind::FundamentalShift(delta) => self.log_fund_target += delta,
            }
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

        // 2. Events due now.
        self.apply_due_events(ts);

        // 3. Fundamental: target grows, value relaxes toward it.
        let spread = self.log_price - self.log_fund;
        self.log_fund_target += self.config.drift * delta;
        let relax = -expm1(-self.config.fundamental_speed * delta);
        self.log_fund += (self.log_fund_target - self.log_fund) * relax;

        // 4. Decaying effects. Drift shifts enter through the exact solution of
        //    ds = −θ s dt + a e^{−λt} dt over the step:
        //    a (e^{−λΔ} − e^{−θΔ}) / (θ − λ), or a Δ e^{−θΔ} when θ ≈ λ.
        //    Vol shifts are taken at the start of the step. Then decay and prune.
        let year_secs = self.derived.year_secs;
        let theta = self.config.mean_reversion_speed;
        let ou_decay = exp(-theta * delta);
        let mut drift_int = 0.0;
        for e in &mut self.drift_effects {
            let lam_year = e.lambda * year_secs;
            let lam_decay = exp(-lam_year * delta);
            let diff = theta - lam_year;
            drift_int += if diff.abs() > 1e-9 * theta {
                e.amplitude * (lam_decay - ou_decay) / diff
            } else {
                e.amplitude * delta * ou_decay
            };
            e.amplitude *= lam_decay;
        }
        self.drift_effects
            .retain(|e| e.amplitude.abs() >= PRUNE_BELOW);
        let mut vol_add = 0.0;
        for e in &mut self.vol_effects {
            vol_add += e.amplitude;
            e.amplitude *= exp(-e.lambda * model_secs);
        }
        self.vol_effects
            .retain(|e| e.amplitude.abs() >= PRUNE_BELOW);

        // 5. Effective vol from the GARCH variance plus vol shifts.
        let sigma_t = (sqrt(self.variance / self.derived.dt) + vol_add).max(0.0);
        self.last_vol = sigma_t;

        // 6. Exact OU step on the spread.
        let z = math::normal(&mut self.rng);
        let ou_std = sigma_t * sqrt(-expm1(-2.0 * theta * delta) / (2.0 * theta));
        let mut spread = spread * ou_decay + drift_int + ou_std * z;

        // 7. Poisson jumps.
        let jumps = self.config.jumps;
        let n_jumps = math::poisson(&mut self.rng, jumps.intensity * delta);
        for _ in 0..n_jumps {
            spread += jumps.mean + jumps.std * math::normal(&mut self.rng);
        }

        // 8. GARCH update on the standardised diffusive shock.
        let d = &self.derived;
        self.variance = d.omega + d.alpha * self.variance * z * z + d.beta * self.variance;

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
