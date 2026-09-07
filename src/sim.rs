//! The simulator: state, per-tick algorithm and the public stepping API.

use alloc::collections::BinaryHeap;
use alloc::vec::Vec;
use core::cmp::Reverse;
use core::time::Duration;

use rand_core::SeedableRng;
use rand_xoshiro::Xoshiro256PlusPlus;

use crate::candles::{Candle, CandleBuilder, Interval};
use crate::config::{Config, ConfigError, Derived};
use crate::event::{Decaying, Event, EventError, EventKind, PRUNE_BELOW, Queued};
use crate::math::{self, LN_2, exp, expm1, ln, ln_1p, pow, round, sqrt};
use crate::time::Timestamp;

/// Version of the state layout and RNG draw order. Bumped whenever either
/// changes; saved states with a different version are rejected on load.
pub const STATE_VERSION: u32 = 2;

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

/// The price process.
///
/// With the `serde` feature the whole state (config, RNG, latent variables,
/// pending events, in-progress candles) round-trips losslessly; loading a state
/// saved with a different [`STATE_VERSION`] or an invalid config fails. Binary
/// formats are always bit-exact; for JSON enable `serde_json`'s
/// `float_roundtrip` feature, otherwise a parsed `f64` may be one ulp off.
#[derive(Clone, Debug)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(try_from = "SimulatorRepr", into = "SimulatorRepr")
)]
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
    /// In-progress bars for the candle-stream API, one per `Interval`.
    candle_cursor: [CandleBuilder; 4],
}

impl Simulator {
    /// Create a simulator from a validated config and a seed.
    ///
    /// # Errors
    /// Returns the first [`ConfigError`] found by [`Config::validate`].
    pub fn new(config: Config, seed: u64) -> Result<Self, ConfigError> {
        let derived = Derived::new(&config)?;
        let log_price = ln(config.start_price_cents as f64 / 100.0);
        let start = match &config.market_hours {
            Some(mh) => mh.align(config.start_ts),
            None => config.start_ts,
        };
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
            candle_cursor: Interval::ALL.map(CandleBuilder::new),
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

    /// Unconditional standard deviation of one regular tick's log return,
    /// `σ √dt`.
    #[must_use]
    pub fn tick_std(&self) -> f64 {
        self.derived.tick_std
    }

    /// Move the wall clock forward to `ts` without emitting ticks (no-op if
    /// `ts` is not in the future).
    pub(crate) fn set_clock(&mut self, ts: Timestamp) {
        if ts > self.clock {
            self.clock = ts;
        }
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

    /// Apply every queued event with `at < before`.
    fn apply_due_events(&mut self, before: Timestamp) {
        while let Some(Reverse(q)) = self.pending.peek() {
            if q.at >= before {
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

    /// Run ticks internally and yield each candle of `iv` as it closes. The
    /// stream is endless; the in-progress bar is kept across calls, so it is
    /// bit-identical to aggregating [`step`](Self::step) output by hand.
    pub fn candles(&mut self, iv: Interval) -> impl Iterator<Item = Candle> + '_ {
        core::iter::from_fn(move || {
            loop {
                let tick = self.step();
                if let Some(c) = self.candle_cursor[iv as usize].push(&tick) {
                    return Some(c);
                }
            }
        })
    }

    /// Advance wall time by `dur` and yield the candles of `iv` that close
    /// within it. The partially built last bar is kept for the next call.
    pub fn advance_candles(
        &mut self,
        dur: Duration,
        iv: Interval,
    ) -> impl Iterator<Item = Candle> + '_ {
        self.clock = self.clock + dur;
        core::iter::from_fn(move || {
            while self.next_ts <= self.clock {
                let tick = self.step();
                if let Some(c) = self.candle_cursor[iv as usize].push(&tick) {
                    return Some(c);
                }
            }
            None
        })
    }

    /// **Coarse mode.** Yield candles of `iv` endlessly, taking one latent
    /// step per candle and synthesising high/low from the Brownian-bridge
    /// extremum distribution instead of simulating every tick. Years of daily
    /// history cost one step per day.
    ///
    /// The bars are *not* the aggregate of the tick path for the same seed:
    /// they are a separate, statistically equivalent path (same fundamental,
    /// mean reversion, jumps, events and vol regime; the intra-bar extremes
    /// are drawn from the bridge given the endpoints, ignoring reversion
    /// within the bar). `Candle::ticks` is `0` for coarse bars. The first bar
    /// may cover only part of its bucket if the simulator is not on a
    /// boundary; under market hours bars are clipped to the session. Coarse
    /// and fine stepping may be mixed on one simulator; the state simply
    /// continues from wherever the last call stopped.
    pub fn coarse_candles(&mut self, iv: Interval) -> impl Iterator<Item = Candle> + '_ {
        core::iter::from_fn(move || Some(self.coarse_step(iv)))
    }

    /// Coarse mode bounded by wall time: yield the coarse bars of `iv` that
    /// close within `dur`. See [`coarse_candles`](Self::coarse_candles).
    pub fn advance_coarse(
        &mut self,
        dur: Duration,
        iv: Interval,
    ) -> impl Iterator<Item = Candle> + '_ {
        self.clock = self.clock + dur;
        core::iter::from_fn(move || {
            let a = self.next_ts;
            let end = Timestamp(iv.bucket(a).0.saturating_add(iv.millis()));
            let end = match &self.config.market_hours {
                Some(mh) => end.min(mh.session_close(a)),
                None => end,
            };
            (end <= self.clock).then(|| self.coarse_step(iv))
        })
    }

    /// Emit exactly one tick and move the clock to it.
    pub fn step(&mut self) -> Tick {
        // 1. Timestamp and model-time of this step. The first tick of a session
        //    (after a previous one) also covers the closed period.
        let ts = self.next_ts;
        let gap = self.config.market_hours.is_some()
            && self
                .last_ts
                .is_some_and(|last| ts - last > self.derived.tick_ms);
        let model_secs = if gap {
            self.derived.gap_secs + self.derived.tick_secs
        } else {
            self.derived.tick_secs
        };
        let delta = model_secs / self.derived.year_secs;

        // 2–9. Events, fundamental, effects, OU, jumps, GARCH.
        let d = &self.derived;
        let garch = (d.omega, d.alpha, d.beta);
        let out = self.evolve(Timestamp(ts.0.saturating_add(1)), delta, model_secs, garch);

        // 10. Volume from |return| and the vol regime.
        let (base, ret_std, noise) = (
            self.derived.base_tick_volume,
            self.derived.tick_std,
            self.config.volume.noise,
        );
        let volume = self.volume(
            self.log_price - out.p_old,
            out.sigma_t,
            base,
            ret_std,
            noise,
        );

        // 11. Emit and schedule.
        self.last_ts = Some(ts);
        if self.clock < ts {
            self.clock = ts;
        }
        let next = Timestamp(ts.0.saturating_add(self.derived.tick_ms));
        self.next_ts = self.align(next);
        Tick {
            ts,
            price_cents: cents(self.log_price),
            volume,
        }
    }

    /// `ts` if it is inside a session (or there is no calendar), else the next
    /// open.
    fn align(&self, ts: Timestamp) -> Timestamp {
        match &self.config.market_hours {
            Some(mh) => mh.align(ts),
            None => ts,
        }
    }

    /// Steps 2–9 of the per-tick algorithm over a step of `delta` years
    /// (`model_secs` model-seconds): apply events due before `before`, advance
    /// the fundamental, integrate and decay the effects, draw the OU noise and
    /// jumps, update the GARCH variance with the given per-tick `(ω, α, β)`,
    /// and recombine into `log_price`. Shared by the fine and coarse paths.
    fn evolve(
        &mut self,
        before: Timestamp,
        delta: f64,
        model_secs: f64,
        garch: (f64, f64, f64),
    ) -> Evolved {
        let p_old = self.log_price;

        // 2. Events due now.
        self.apply_due_events(before);

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
        let (omega, alpha, beta) = garch;
        self.variance = omega + alpha * self.variance * z * z + beta * self.variance;

        // 9. Recombine and clamp.
        self.log_price = (self.log_fund + spread).clamp(MIN_LOG_PRICE, MAX_LOG_PRICE);
        debug_assert!(self.log_price.is_finite());
        Evolved { p_old, sigma_t }
    }

    /// Advance the latent state by one candle of `iv` in a single step and
    /// synthesise the bar. See [`coarse_candles`](Self::coarse_candles).
    fn coarse_step(&mut self, iv: Interval) -> Candle {
        let d = &self.derived;
        let cfg = &self.config;

        // 1. Wall interval [a, b): from the current position to the end of its
        //    bucket, clipped to the session close under market hours.
        let a = self.next_ts;
        let bucket = iv.bucket(a);
        let b = Timestamp(bucket.0.saturating_add(iv.millis()));
        let (end, session_ms) = match &cfg.market_hours {
            Some(mh) => {
                let end = b.min(mh.session_close(a));
                (end, end - a)
            }
            None => (b, b - a),
        };
        let gap =
            cfg.market_hours.is_some() && self.last_ts.is_some_and(|last| a - last > d.tick_ms);
        let session_secs = session_ms as f64 / 1000.0;
        let model_secs = if gap {
            d.gap_secs + session_secs
        } else {
            session_secs
        };
        let delta = model_secs / d.year_secs;

        // GARCH coefficients for a step of this length:
        // φ = e^{−κ_v Δ}, α = min(sqrt(r (1 − φ²) / 2), φ), β = φ − α,
        // ω = (1 − φ) σ² dt in per-tick units.
        let phi = exp(-LN_2 * model_secs / cfg.garch.variance_half_life.as_secs_f64());
        let alpha = sqrt(cfg.garch.variance_dispersion * (1.0 - phi * phi) / 2.0).min(phi);
        let garch = ((1.0 - phi) * d.var_unc, alpha, phi - alpha);

        // 2–9. Same latent evolution as a tick, over the whole step.
        let out = self.evolve(b, delta, model_secs, garch);
        let d = &self.derived;
        let cfg = &self.config;

        // High/low from the Brownian-bridge extremum distribution given the
        // endpoints: for a bridge from 0 to x with variance v,
        // P(max ≥ m) = exp(−2 m (m − x) / v), so m = (x + sqrt(x² − 2 v ln u)) / 2;
        // the minimum is the mirror image with an independent uniform.
        let x = self.log_price - out.p_old;
        let jump_var = cfg.jumps.intensity
            * delta
            * (cfg.jumps.mean * cfg.jumps.mean + cfg.jumps.std * cfg.jumps.std);
        let v = out.sigma_t * out.sigma_t * delta + jump_var;
        let u_hi = math::uniform(&mut self.rng);
        let u_lo = math::uniform(&mut self.rng);
        let (hi, lo) = if v > 0.0 {
            let span_hi = sqrt(x * x - 2.0 * v * ln(1.0 - u_hi));
            let span_lo = sqrt(x * x - 2.0 * v * ln(1.0 - u_lo));
            ((x + span_hi) / 2.0, (x - span_lo) / 2.0)
        } else {
            (x.max(0.0), x.min(0.0))
        };

        // Volume: the tick formula at the step scale. Its expectation matches
        // the fine aggregate; the lognormal noise shrinks as 1/sqrt(ticks).
        let n_ticks = (session_secs / d.tick_secs).max(1.0);
        let base = d.base_tick_volume * n_ticks;
        let ret_std = cfg.volatility * sqrt(delta);
        let noise = cfg.volume.noise / sqrt(n_ticks);
        let volume = self.volume(x, out.sigma_t, base, ret_std, noise);

        // Schedule.
        self.last_ts = Some(end);
        if self.clock < end {
            self.clock = end;
        }
        self.next_ts = self.align(b);

        let open = cents(out.p_old);
        let close = cents(self.log_price);
        Candle {
            open_ts: bucket,
            open,
            high: cents(out.p_old + hi).max(open).max(close),
            low: cents(out.p_old + lo).min(open).min(close),
            close,
            volume,
            ticks: 0,
        }
    }

    /// Expected volume `base · (σ_t/σ)^γ · (1 + c |ret| / ret_std)` with
    /// lognormal noise of std `noise`. Always draws one normal so the RNG
    /// stream is config-independent.
    fn volume(&mut self, ret: f64, sigma_t: f64, base: f64, ret_std: f64, noise: f64) -> u64 {
        let v = self.config.volume;
        let zv = math::normal(&mut self.rng);
        let sigma = self.config.volatility;
        let regime = if sigma > 0.0 {
            pow(sigma_t / sigma, v.vol_exponent)
        } else {
            1.0
        };
        let activity = if ret_std > 0.0 {
            1.0 + v.return_sensitivity * ret.abs() / ret_std
        } else {
            1.0
        };
        let expected = base * regime * activity;
        let noisy = expected * exp(noise * zv - 0.5 * noise * noise);
        let r = round(noisy);
        if r.is_nan() || r <= 0.0 {
            0
        } else if r >= u64::MAX as f64 {
            u64::MAX
        } else {
            r as u64
        }
    }
}

/// What [`Simulator::evolve`] hands back to the fine and coarse paths.
struct Evolved {
    /// `ln P` before the step, including before any events applied in it.
    p_old: f64,
    /// Effective annualised vol used for the step.
    sigma_t: f64,
}

/// Serialisable form of [`Simulator`]: everything except the derived
/// quantities, which are rebuilt on load.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SimulatorRepr {
    /// [`STATE_VERSION`] at save time.
    pub version: u32,
    config: Config,
    rng: Xoshiro256PlusPlus,
    clock: Timestamp,
    next_ts: Timestamp,
    last_ts: Option<Timestamp>,
    log_price: f64,
    log_fund: f64,
    log_fund_target: f64,
    variance: f64,
    last_vol: f64,
    drift_effects: Vec<Decaying>,
    vol_effects: Vec<Decaying>,
    pending: Vec<Queued>,
    next_seq: u64,
    candle_cursor: [CandleBuilder; 4],
}

impl From<Simulator> for SimulatorRepr {
    fn from(s: Simulator) -> Self {
        Self {
            version: STATE_VERSION,
            config: s.config,
            rng: s.rng,
            clock: s.clock,
            next_ts: s.next_ts,
            last_ts: s.last_ts,
            log_price: s.log_price,
            log_fund: s.log_fund,
            log_fund_target: s.log_fund_target,
            variance: s.variance,
            last_vol: s.last_vol,
            drift_effects: s.drift_effects,
            vol_effects: s.vol_effects,
            pending: s.pending.into_iter().map(|Reverse(q)| q).collect(),
            next_seq: s.next_seq,
            candle_cursor: s.candle_cursor,
        }
    }
}

impl TryFrom<SimulatorRepr> for Simulator {
    type Error = LoadError;

    fn try_from(r: SimulatorRepr) -> Result<Self, LoadError> {
        if r.version != STATE_VERSION {
            return Err(LoadError::VersionMismatch {
                found: r.version,
                expected: STATE_VERSION,
            });
        }
        let derived = Derived::new(&r.config).map_err(LoadError::Config)?;
        let finite = [
            r.log_price,
            r.log_fund,
            r.log_fund_target,
            r.variance,
            r.last_vol,
        ]
        .iter()
        .all(|v| v.is_finite())
            && r.variance >= 0.0
            && r.drift_effects
                .iter()
                .chain(&r.vol_effects)
                .all(|e| e.amplitude.is_finite() && e.lambda.is_finite() && e.lambda > 0.0)
            && r.pending.iter().all(|q| q.kind.validate().is_ok());
        if !finite {
            return Err(LoadError::Corrupt);
        }
        Ok(Self {
            config: r.config,
            derived,
            rng: r.rng,
            clock: r.clock,
            next_ts: r.next_ts,
            last_ts: r.last_ts,
            log_price: r.log_price.clamp(MIN_LOG_PRICE, MAX_LOG_PRICE),
            log_fund: r.log_fund,
            log_fund_target: r.log_fund_target,
            variance: r.variance,
            last_vol: r.last_vol,
            drift_effects: r.drift_effects,
            vol_effects: r.vol_effects,
            pending: r.pending.into_iter().map(Reverse).collect(),
            next_seq: r.next_seq,
            candle_cursor: r.candle_cursor,
        })
    }
}

/// Why a saved [`Simulator`] state could not be loaded.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum LoadError {
    /// Saved with a different [`STATE_VERSION`].
    VersionMismatch {
        /// Version in the saved state.
        found: u32,
        /// Version this crate expects.
        expected: u32,
    },
    /// The saved config does not validate.
    Config(ConfigError),
    /// A latent value is not finite or an effect/event is malformed.
    Corrupt,
}

impl core::fmt::Display for LoadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::VersionMismatch { found, expected } => {
                write!(f, "saved state version {found}, expected {expected}")
            }
            Self::Config(e) => write!(f, "saved config invalid: {e}"),
            Self::Corrupt => f.write_str("saved state is corrupt"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for LoadError {}

/// Round a log-dollar price to cents, clamped to `[1, 10^15]`.
fn cents(log_price: f64) -> i64 {
    let c = round(100.0 * exp(log_price));
    if c.is_nan() {
        return 1;
    }
    (c as i64).clamp(1, MAX_PRICE_CENTS)
}
