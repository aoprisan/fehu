//! Wall-clock timestamps.

use core::ops::{Add, Sub};
use core::time::Duration;

/// Milliseconds since the Unix epoch, UTC.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
pub struct Timestamp(pub i64);

impl Timestamp {
    /// Build from milliseconds since the epoch.
    #[must_use]
    pub const fn from_millis(ms: i64) -> Self {
        Self(ms)
    }

    /// Milliseconds since the epoch.
    #[must_use]
    pub const fn millis(self) -> i64 {
        self.0
    }
}

/// Whole milliseconds in `d`, or `None` if it has a sub-millisecond part or
/// does not fit an `i64`.
pub(crate) fn whole_millis(d: Duration) -> Option<i64> {
    if !d.subsec_nanos().is_multiple_of(1_000_000) {
        return None;
    }
    i64::try_from(d.as_millis()).ok()
}

impl Add<Duration> for Timestamp {
    type Output = Timestamp;
    /// Saturates on overflow; sub-millisecond parts of `rhs` are truncated.
    fn add(self, rhs: Duration) -> Timestamp {
        let ms = i64::try_from(rhs.as_millis()).unwrap_or(i64::MAX);
        Timestamp(self.0.saturating_add(ms))
    }
}

impl Sub<Duration> for Timestamp {
    type Output = Timestamp;
    /// Saturates on overflow; sub-millisecond parts of `rhs` are truncated.
    fn sub(self, rhs: Duration) -> Timestamp {
        let ms = i64::try_from(rhs.as_millis()).unwrap_or(i64::MAX);
        Timestamp(self.0.saturating_sub(ms))
    }
}

impl Sub<Timestamp> for Timestamp {
    type Output = i64;
    /// Difference in milliseconds.
    fn sub(self, rhs: Timestamp) -> i64 {
        self.0 - rhs.0
    }
}
