//! Wall-clock timestamps and the optional market-hours calendar.

use core::ops::{Add, Sub};
use core::time::Duration;

use crate::config::ConfigError;

/// Milliseconds in one day.
pub(crate) const DAY_MS: i64 = 86_400_000;

/// Milliseconds since the Unix epoch, UTC.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
pub struct Timestamp(pub i64);

impl Timestamp {
    /// Days since the epoch (floor) and milliseconds into that day.
    const fn day_and_time(self) -> (i64, i64) {
        (self.0.div_euclid(DAY_MS), self.0.rem_euclid(DAY_MS))
    }

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

/// A simple UTC trading calendar: one session per trading weekday, no
/// holidays. When enabled, ticks exist only inside sessions and the first
/// tick of each session carries the closed-period ("overnight") move.
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct MarketHours {
    /// Session open, seconds after 00:00 UTC. Default 34 200 (09:30).
    pub open_secs: u32,
    /// Session close, seconds after 00:00 UTC. Default 57 600 (16:00). Must
    /// satisfy `open < close ≤ 86 400`.
    pub close_secs: u32,
    /// Trading days as a bitmask: bit 0 = Monday … bit 6 = Sunday. Default
    /// Monday–Friday (`0b001_1111`). Must be non-zero.
    pub weekdays: u8,
    /// Variance of one closed period relative to one full session, `g`.
    /// Default 0.30, range `[0, 2]`. Every gap is weighted the same, whether
    /// overnight or a weekend.
    pub gap_weight: f64,
}

impl Default for MarketHours {
    fn default() -> Self {
        Self {
            open_secs: 34_200,
            close_secs: 57_600,
            weekdays: 0b001_1111,
            gap_weight: 0.30,
        }
    }
}

impl MarketHours {
    /// Monday–Friday, 09:30–16:00 UTC.
    pub const WEEKDAYS: u8 = 0b001_1111;

    /// Length of one session in seconds.
    #[must_use]
    pub const fn session_secs(&self) -> u32 {
        self.close_secs - self.open_secs
    }

    /// Trading days per year, `365.25 · n_days / 7`.
    #[must_use]
    pub fn trading_days_per_year(&self) -> f64 {
        365.25 * f64::from(self.weekdays.count_ones()) / 7.0
    }

    /// Whether the day `days_since_epoch` is a trading day.
    #[must_use]
    pub const fn is_trading_day(&self, days_since_epoch: i64) -> bool {
        // 1970-01-01 was a Thursday; Monday = 0.
        let weekday = (days_since_epoch + 3).rem_euclid(7);
        self.weekdays & (1 << weekday) != 0
    }

    /// Whether `ts` lies inside a session (`open ≤ t < close`).
    #[must_use]
    pub const fn contains(&self, ts: Timestamp) -> bool {
        let (day, tod) = ts.day_and_time();
        self.is_trading_day(day)
            && tod >= self.open_secs as i64 * 1000
            && tod < self.close_secs as i64 * 1000
    }

    /// Close of the session on `ts`'s calendar day (whether or not it is a
    /// trading day).
    #[must_use]
    pub const fn session_close(&self, ts: Timestamp) -> Timestamp {
        let (day, _) = ts.day_and_time();
        Timestamp(day * DAY_MS + self.close_secs as i64 * 1000)
    }

    /// First session open at or after `ts`. If `ts` is inside a session this
    /// is the *next* session's open, not the current one's.
    #[must_use]
    pub fn next_open(&self, ts: Timestamp) -> Timestamp {
        let (day, tod) = ts.day_and_time();
        let open_ms = i64::from(self.open_secs) * 1000;
        let mut d = if tod <= open_ms { day } else { day + 1 };
        // `weekdays != 0` is validated, so at most 7 iterations.
        for _ in 0..8 {
            if self.is_trading_day(d) {
                return Timestamp(d * DAY_MS + open_ms);
            }
            d += 1;
        }
        Timestamp(d * DAY_MS + open_ms)
    }

    /// The timestamp of the first tick at or after `ts`: `ts` itself if it is
    /// inside a session, otherwise the next open.
    #[must_use]
    pub fn align(&self, ts: Timestamp) -> Timestamp {
        if self.contains(ts) {
            ts
        } else {
            self.next_open(ts)
        }
    }

    pub(crate) fn validate(&self, tick: Duration) -> Result<(), ConfigError> {
        if self.open_secs >= self.close_secs || self.close_secs > 86_400 {
            return Err(ConfigError::OutOfRange {
                field: "market_hours.close_secs",
                reason: "must satisfy open < close ≤ 86400",
            });
        }
        if self.weekdays == 0 || self.weekdays > 0b111_1111 {
            return Err(ConfigError::OutOfRange {
                field: "market_hours.weekdays",
                reason: "must have at least one of the low 7 bits set",
            });
        }
        if !self.gap_weight.is_finite() {
            return Err(ConfigError::NotFinite {
                field: "market_hours.gap_weight",
            });
        }
        if !(0.0..=2.0).contains(&self.gap_weight) {
            return Err(ConfigError::OutOfRange {
                field: "market_hours.gap_weight",
                reason: "must be in [0, 2]",
            });
        }
        if tick.as_secs_f64() > f64::from(self.session_secs()) {
            return Err(ConfigError::OutOfRange {
                field: "tick",
                reason: "must not exceed the market session length",
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendar_basics() {
        let mh = MarketHours::default();
        // Day 0 (Thursday) is a trading day; day 2 (Saturday) is not.
        assert!(mh.is_trading_day(0));
        assert!(!mh.is_trading_day(2));
        assert!(!mh.is_trading_day(3));
        assert!(mh.is_trading_day(4));
        assert!(mh.is_trading_day(-1)); // Wednesday 1969-12-31
        let open = Timestamp(34_200_000);
        assert!(mh.contains(open));
        assert!(!mh.contains(Timestamp(57_600_000)));
        assert_eq!(mh.next_open(Timestamp(0)), open);
        assert_eq!(mh.next_open(open), open);
        assert_eq!(
            mh.next_open(Timestamp(open.0 + 1)),
            Timestamp(DAY_MS + open.0)
        );
        // Friday close → Monday open.
        let fri_close = Timestamp(DAY_MS + 57_600_000);
        assert_eq!(mh.next_open(fri_close), Timestamp(4 * DAY_MS + open.0));
        assert_eq!(mh.align(Timestamp(40_000_000)), Timestamp(40_000_000));
    }
}
