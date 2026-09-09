//! External events and the bookkeeping for their decaying effects.

use core::cmp::Ordering;
use core::fmt;
use core::time::Duration;

use crate::math::LN_2;
use crate::time::Timestamp;

/// Something the game did to the company or the market.
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Event {
    /// When it takes effect. Applied at the first tick with `ts ≥ at`; a
    /// timestamp in the past applies on the next tick.
    pub at: Timestamp,
    /// What happens.
    pub kind: EventKind,
}

/// The kinds of event the simulator understands.
///
/// Jumps and shifts act on the log-spread and therefore revert toward the
/// fundamental with half-life `ln2 / mean_reversion_speed`. Only
/// [`FundamentalShift`](Self::FundamentalShift) is permanent.
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum EventKind {
    /// Immediate multiplicative move: `0.10` is +10 %, `-0.25` is −25 %. Must be
    /// greater than `-1`.
    Jump(f64),
    /// Adds `delta` (annualised drift) to the price process, decaying with the
    /// given half-life in model time. Superposes with other shifts.
    DriftShift {
        /// Annualised log-drift added at `t = 0`.
        delta: f64,
        /// Time for the effect to halve. Must be positive.
        half_life: Duration,
    },
    /// Adds `delta` (annualised vol, may be negative) to the effective
    /// volatility, decaying with the given half-life in model time.
    VolShift {
        /// Annualised vol added at `t = 0`.
        delta: f64,
        /// Time for the effect to halve. Must be positive.
        half_life: Duration,
    },
    /// Permanently multiplies the fundamental target by `e^delta`.
    FundamentalShift(f64),
}

impl EventKind {
    /// A [`DriftShift`](Self::DriftShift) whose integrated effect (ignoring
    /// mean reversion) is a log move of `total`, e.g. `0.05` for about +5 %,
    /// spread over roughly `half_life` (most of it within three half-lives).
    ///
    /// `delta = total · ln2 / half_life_years`, using a calendar year of
    /// 365.25 days; under market hours the effect is measured in model time.
    #[must_use]
    pub fn drift_for_total_move(total: f64, half_life: Duration) -> Self {
        let years = half_life.as_secs_f64() / (365.25 * 86_400.0);
        Self::DriftShift {
            delta: total * LN_2 / years,
            half_life,
        }
    }

    pub(crate) fn validate(&self) -> Result<(), EventError> {
        let finite = |v: f64| {
            if v.is_finite() {
                Ok(())
            } else {
                Err(EventError::NotFinite)
            }
        };
        match *self {
            Self::Jump(pct) => {
                finite(pct)?;
                if pct <= -1.0 {
                    return Err(EventError::JumpBelowMinusOne);
                }
            }
            Self::DriftShift { delta, half_life } | Self::VolShift { delta, half_life } => {
                finite(delta)?;
                if half_life.is_zero() {
                    return Err(EventError::ZeroHalfLife);
                }
            }
            Self::FundamentalShift(delta) => finite(delta)?,
        }
        Ok(())
    }
}

/// Why an [`Event`] was rejected by `push_event`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum EventError {
    /// A magnitude is NaN or infinite.
    NotFinite,
    /// `Jump(pct)` with `pct ≤ -1` would take the price to zero or below.
    JumpBelowMinusOne,
    /// A shift with a zero half-life would never take effect.
    ZeroHalfLife,
}

impl fmt::Display for EventError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotFinite => "event magnitude is not finite",
            Self::JumpBelowMinusOne => "jump percentage must be greater than -1",
            Self::ZeroHalfLife => "shift half-life must be positive",
        })
    }
}

#[cfg(feature = "std")]
impl std::error::Error for EventError {}

/// A queued event, ordered by `(at, seq)` so equal timestamps apply in push
/// order.
#[derive(Clone, Copy, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub(crate) struct Queued {
    pub at: Timestamp,
    pub seq: u64,
    pub kind: EventKind,
}

impl PartialEq for Queued {
    fn eq(&self, other: &Self) -> bool {
        self.at == other.at && self.seq == other.seq
    }
}
impl Eq for Queued {}
impl PartialOrd for Queued {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Queued {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.at, self.seq).cmp(&(other.at, other.seq))
    }
}

/// An exponentially decaying effect amplitude.
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub(crate) struct Decaying {
    /// Current amplitude (annualised drift or vol).
    pub amplitude: f64,
    /// Decay rate per model-second, `ln2 / half_life`.
    pub lambda: f64,
}

impl Decaying {
    pub(crate) fn new(delta: f64, half_life: Duration) -> Self {
        Self {
            amplitude: delta,
            lambda: LN_2 / half_life.as_secs_f64(),
        }
    }
}

/// Amplitudes below this are pruned.
pub(crate) const PRUNE_BELOW: f64 = 1e-12;
