//! Simulator configuration, validation and the per-tick derived quantities.

use core::fmt;
use core::time::Duration;

use crate::math::{self, LN_2};
use crate::time::{MarketHours, Timestamp, whole_millis};

/// Seconds in a calendar year (365.25 days).
const CALENDAR_YEAR_SECS: f64 = 365.25 * 86_400.0;

/// Everything the simulator needs. All rates are annualised; the crate scales
/// them to the tick. `Config::default()` is a reasonable mid-cap stock.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Config {
    /// Initial price and fundamental value, in cents. `[1, 10^15]`.
    pub start_price_cents: i64,
    /// Long-run log growth of the fundamental value, per year. `[-2, 2]`.
    pub drift: f64,
    /// Unconditional annualised volatility of the price. `[0, 5]`.
    pub volatility: f64,
    /// Speed `θ` at which the log-spread `ln P − ln V` reverts, per year. Half-life
    /// is `ln2/θ`. `(0, 10^5]`.
    pub mean_reversion_speed: f64,
    /// Speed `κ_f` at which the fundamental relaxes toward its target, per year.
    /// `[0, 10^5]`; `0` means it only moves through `drift` and shifts.
    pub fundamental_speed: f64,
    /// GARCH(1,1) volatility clustering.
    pub garch: GarchParams,
    /// Random Merton-style jumps.
    pub jumps: JumpParams,
    /// How tick volume is derived from returns and the vol regime.
    pub volume: VolumeParams,
    /// Wall time between ticks. Whole milliseconds, `[1 ms, 1 day]`.
    pub tick: Duration,
    /// Timestamp of the first tick (aligned to the next open under market hours).
    pub start_ts: Timestamp,
    /// Optional trading calendar. `None` trades around the clock.
    pub market_hours: Option<MarketHours>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            start_price_cents: 10_000,
            drift: 0.05,
            volatility: 0.40,
            mean_reversion_speed: 50.0,
            fundamental_speed: 36.0,
            garch: GarchParams::default(),
            jumps: JumpParams::default(),
            volume: VolumeParams::default(),
            tick: Duration::from_secs(1),
            start_ts: Timestamp(0),
            market_hours: None,
        }
    }
}

/// GARCH(1,1) variance dynamics in tick-invariant form.
///
/// The classic per-tick `(α, β)` are derived for the actual tick from these two
/// numbers (Nelson's continuous-time limit), so changing `Config::tick` does not
/// change the behaviour of the vol process. With Gaussian innovations:
///
/// - a variance shock decays with half-life `variance_half_life`;
/// - `Var(h) / E[h]² = r / (1 − r)` where `r = variance_dispersion`;
/// - returns shorter than the half-life have kurtosis `3 / (1 − r)`.
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct GarchParams {
    /// Half-life of a variance shock, in model time. `[1 s, 1 year]`.
    pub variance_half_life: Duration,
    /// Dispersion of the variance regime, `r ∈ [0, 1)`. `0` disables clustering
    /// (constant variance); `0.5` gives `std(h) = E[h]`.
    pub variance_dispersion: f64,
}

impl Default for GarchParams {
    fn default() -> Self {
        Self {
            variance_half_life: Duration::from_secs(3600),
            variance_dispersion: 0.5,
        }
    }
}

impl GarchParams {
    /// Classic per-tick coefficients `(α, β)` for a tick of `dt` years, given
    /// the model-time year length used by the simulator.
    ///
    /// `q = ln2 · tick_secs / half_life_secs`, `α = sqrt(r·q)`, `β = 1 − q − α`.
    #[must_use]
    pub fn alpha_beta(&self, tick: Duration) -> (f64, f64) {
        let q = LN_2 * tick.as_secs_f64() / self.variance_half_life.as_secs_f64();
        let alpha = math::sqrt(self.variance_dispersion * q);
        (alpha, 1.0 - q - alpha)
    }

    /// Build from classic per-tick coefficients measured at a given tick.
    ///
    /// Inverse of [`alpha_beta`](Self::alpha_beta): `q = 1 − α − β`,
    /// `r = α² / q`, `half_life = ln2 · tick / q`. Requires `α + β < 1`.
    #[must_use]
    pub fn from_alpha_beta(alpha: f64, beta: f64, tick: Duration) -> Self {
        let q = 1.0 - alpha - beta;
        Self {
            variance_half_life: Duration::from_secs_f64(LN_2 * tick.as_secs_f64() / q),
            variance_dispersion: alpha * alpha / q,
        }
    }

    fn validate(&self, tick: Duration) -> Result<(), ConfigError> {
        let hl = self.variance_half_life.as_secs_f64();
        if !(1.0..=CALENDAR_YEAR_SECS).contains(&hl) {
            return Err(ConfigError::OutOfRange {
                field: "garch.variance_half_life",
                reason: "must be in [1 s, 1 year]",
            });
        }
        check_finite("garch.variance_dispersion", self.variance_dispersion)?;
        if self.variance_dispersion < 0.0 || self.variance_dispersion >= 1.0 {
            return Err(ConfigError::OutOfRange {
                field: "garch.variance_dispersion",
                reason: "must be in [0, 1)",
            });
        }
        let (_, beta) = self.alpha_beta(tick);
        if beta < 0.0 {
            return Err(ConfigError::OutOfRange {
                field: "garch.variance_half_life",
                reason: "tick too coarse for variance_half_life (β < 0)",
            });
        }
        Ok(())
    }
}

/// Poisson jumps on the log-spread. Like all shocks they revert toward the
/// fundamental. No drift compensator is applied.
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct JumpParams {
    /// Expected jumps per year, `λ`. `[0, 10^4]`.
    pub intensity: f64,
    /// Mean log-jump size, `μ_J`. `[-1, 1]`.
    pub mean: f64,
    /// Std of the log-jump size, `σ_J`. `[0, 1]`.
    pub std: f64,
}

impl Default for JumpParams {
    fn default() -> Self {
        Self {
            intensity: 100.0,
            mean: -0.005,
            std: 0.03,
        }
    }
}

impl JumpParams {
    fn validate(&self) -> Result<(), ConfigError> {
        check_range(
            "jumps.intensity",
            self.intensity,
            0.0,
            1e4,
            "must be in [0, 10^4] per year",
        )?;
        check_range("jumps.mean", self.mean, -1.0, 1.0, "must be in [-1, 1]")?;
        check_range("jumps.std", self.std, 0.0, 1.0, "must be in [0, 1]")
    }
}

/// Volume is derived from each tick's return and the current vol regime:
///
/// ```text
/// E[v] = base_per_day · tick_s / S_day · (σ_t / σ)^vol_exponent
///        · (1 + return_sensitivity · |r_t| / (σ √dt))
/// v    = round(E[v] · exp(noise · z − noise² / 2))
/// ```
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct VolumeParams {
    /// Shares per trading day in a calm regime. `(0, 10^15]`.
    pub base_per_day: f64,
    /// Extra volume per unit of `|r| / tick_std`. `[0, 100]`.
    pub return_sensitivity: f64,
    /// Exponent on the vol regime ratio. `[0, 4]`.
    pub vol_exponent: f64,
    /// Lognormal noise std on the volume. `[0, 3]`.
    pub noise: f64,
}

impl Default for VolumeParams {
    fn default() -> Self {
        Self {
            base_per_day: 1_000_000.0,
            return_sensitivity: 2.0,
            vol_exponent: 1.0,
            noise: 0.5,
        }
    }
}

impl VolumeParams {
    fn validate(&self) -> Result<(), ConfigError> {
        check_finite("volume.base_per_day", self.base_per_day)?;
        if self.base_per_day <= 0.0 || self.base_per_day > 1e15 {
            return Err(ConfigError::OutOfRange {
                field: "volume.base_per_day",
                reason: "must be in (0, 10^15]",
            });
        }
        check_range(
            "volume.return_sensitivity",
            self.return_sensitivity,
            0.0,
            100.0,
            "must be in [0, 100]",
        )?;
        check_range(
            "volume.vol_exponent",
            self.vol_exponent,
            0.0,
            4.0,
            "must be in [0, 4]",
        )?;
        check_range("volume.noise", self.noise, 0.0, 3.0, "must be in [0, 3]")
    }
}

/// Why a [`Config`] was rejected.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConfigError {
    /// A float field is NaN or infinite.
    NotFinite {
        /// Dotted path of the field, e.g. `"garch.variance_dispersion"`.
        field: &'static str,
    },
    /// A field is outside its documented range.
    OutOfRange {
        /// Dotted path of the field.
        field: &'static str,
        /// Human-readable constraint that was violated.
        reason: &'static str,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFinite { field } => write!(f, "config field `{field}` is not finite"),
            Self::OutOfRange { field, reason } => {
                write!(f, "config field `{field}` out of range: {reason}")
            }
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for ConfigError {}

pub(crate) fn check_finite(field: &'static str, v: f64) -> Result<(), ConfigError> {
    if v.is_finite() {
        Ok(())
    } else {
        Err(ConfigError::NotFinite { field })
    }
}

pub(crate) fn check_range(
    field: &'static str,
    v: f64,
    lo: f64,
    hi: f64,
    reason: &'static str,
) -> Result<(), ConfigError> {
    check_finite(field, v)?;
    if v < lo || v > hi {
        return Err(ConfigError::OutOfRange { field, reason });
    }
    Ok(())
}

impl Config {
    /// Check every field against its documented range.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !(1..=1_000_000_000_000_000).contains(&self.start_price_cents) {
            return Err(ConfigError::OutOfRange {
                field: "start_price_cents",
                reason: "must be in [1, 10^15]",
            });
        }
        check_range(
            "drift",
            self.drift,
            -2.0,
            2.0,
            "must be in [-2, 2] per year",
        )?;
        check_range("volatility", self.volatility, 0.0, 5.0, "must be in [0, 5]")?;
        check_finite("mean_reversion_speed", self.mean_reversion_speed)?;
        if self.mean_reversion_speed <= 0.0 || self.mean_reversion_speed > 1e5 {
            return Err(ConfigError::OutOfRange {
                field: "mean_reversion_speed",
                reason: "must be in (0, 10^5] per year",
            });
        }
        check_range(
            "fundamental_speed",
            self.fundamental_speed,
            0.0,
            1e5,
            "must be in [0, 10^5] per year",
        )?;
        match whole_millis(self.tick) {
            Some(ms) if (1..=86_400_000).contains(&ms) => {}
            _ => {
                return Err(ConfigError::OutOfRange {
                    field: "tick",
                    reason: "must be whole milliseconds in [1 ms, 1 day]",
                });
            }
        }
        self.garch.validate(self.tick)?;
        self.jumps.validate()?;
        self.volume.validate()?;
        if let Some(mh) = &self.market_hours {
            mh.validate(self.tick)?;
        }
        Ok(())
    }
}

/// Per-tick quantities derived from a validated [`Config`]. Never serialised;
/// rebuilt on construction and on load.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Derived {
    /// Model-seconds in one year (`Y`).
    pub year_secs: f64,
    /// Model-seconds covered by one closed period (`g·S`), 0 without market hours.
    pub gap_secs: f64,
    /// Wall milliseconds per tick.
    pub tick_ms: i64,
    /// Model-seconds per regular tick.
    pub tick_secs: f64,
    /// Years per regular tick.
    pub dt: f64,
    /// Unconditional per-tick return std, `σ √dt`.
    pub tick_std: f64,
    /// GARCH `α` for the regular tick.
    pub alpha: f64,
    /// GARCH `β` for the regular tick.
    pub beta: f64,
    /// GARCH `ω` for the regular tick, so that `E[h] = σ² dt`.
    pub omega: f64,
    /// Unconditional per-tick variance `σ² dt`.
    pub var_unc: f64,
    /// Expected volume of a regular tick in a calm regime.
    pub base_tick_volume: f64,
}

impl Derived {
    pub(crate) fn new(cfg: &Config) -> Result<Self, ConfigError> {
        cfg.validate()?;
        let tick_ms = whole_millis(cfg.tick).expect("validated");
        let tick_secs = tick_ms as f64 / 1000.0;
        let (year_secs, gap_secs, day_secs) = match &cfg.market_hours {
            None => (CALENDAR_YEAR_SECS, 0.0, 86_400.0),
            Some(mh) => {
                let session = f64::from(mh.session_secs());
                let gap = mh.gap_weight * session;
                (mh.trading_days_per_year() * (session + gap), gap, session)
            }
        };
        let dt = tick_secs / year_secs;
        let (alpha, beta) = cfg.garch.alpha_beta(cfg.tick);
        let q = 1.0 - alpha - beta;
        let var_unc = cfg.volatility * cfg.volatility * dt;
        Ok(Self {
            year_secs,
            gap_secs,
            tick_ms,
            tick_secs,
            dt,
            tick_std: cfg.volatility * math::sqrt(dt),
            alpha,
            beta,
            omega: q * var_unc,
            var_unc,
            base_tick_volume: cfg.volume.base_per_day * tick_secs / day_secs,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_valid() {
        Config::default().validate().unwrap();
    }

    #[test]
    fn rejects_bad_fields() {
        let c = Config {
            volatility: f64::NAN,
            ..Config::default()
        };
        assert_eq!(
            c.validate(),
            Err(ConfigError::NotFinite {
                field: "volatility"
            })
        );
        let c = Config {
            tick: Duration::from_micros(1500),
            ..Config::default()
        };
        assert!(matches!(
            c.validate(),
            Err(ConfigError::OutOfRange { field: "tick", .. })
        ));
        let c = Config {
            mean_reversion_speed: 0.0,
            ..Config::default()
        };
        assert!(matches!(
            c.validate(),
            Err(ConfigError::OutOfRange {
                field: "mean_reversion_speed",
                ..
            })
        ));
    }
}
