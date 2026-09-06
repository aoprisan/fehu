//! Simulator configuration, validation and the per-tick derived quantities.

use core::fmt;
use core::time::Duration;

use crate::math;
use crate::time::{Timestamp, whole_millis};

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
    /// Wall time between ticks. Whole milliseconds, `[1 ms, 1 day]`.
    pub tick: Duration,
    /// Timestamp of the first tick.
    pub start_ts: Timestamp,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            start_price_cents: 10_000,
            drift: 0.05,
            volatility: 0.40,
            mean_reversion_speed: 50.0,
            fundamental_speed: 36.0,
            tick: Duration::from_secs(1),
            start_ts: Timestamp(0),
        }
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
        Ok(())
    }
}

/// Per-tick quantities derived from a validated [`Config`]. Never serialised;
/// rebuilt on construction and on load.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Derived {
    /// Model-seconds in one year (`Y`).
    pub year_secs: f64,
    /// Wall milliseconds per tick.
    pub tick_ms: i64,
    /// Model-seconds per regular tick.
    pub tick_secs: f64,
    /// Years per regular tick.
    pub dt: f64,
    /// Unconditional per-tick return std, `σ √dt`.
    pub tick_std: f64,
}

impl Derived {
    pub(crate) fn new(cfg: &Config) -> Result<Self, ConfigError> {
        cfg.validate()?;
        let tick_ms = whole_millis(cfg.tick).expect("validated");
        let tick_secs = tick_ms as f64 / 1000.0;
        let year_secs = CALENDAR_YEAR_SECS;
        let dt = tick_secs / year_secs;
        Ok(Self {
            year_secs,
            tick_ms,
            tick_secs,
            dt,
            tick_std: cfg.volatility * math::sqrt(dt),
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
