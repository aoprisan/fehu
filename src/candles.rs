//! OHLCV aggregation of ticks at fixed intervals.

use alloc::collections::VecDeque;

use crate::sim::Tick;
use crate::time::Timestamp;

/// Candle intervals the aggregator supports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Interval {
    /// One minute.
    M1,
    /// Five minutes.
    M5,
    /// One hour.
    H1,
    /// One day (UTC).
    D1,
}

impl Interval {
    /// Every interval, in ascending order.
    pub const ALL: [Interval; 4] = [Interval::M1, Interval::M5, Interval::H1, Interval::D1];

    /// Length in milliseconds.
    #[must_use]
    pub const fn millis(self) -> i64 {
        match self {
            Self::M1 => 60_000,
            Self::M5 => 300_000,
            Self::H1 => 3_600_000,
            Self::D1 => 86_400_000,
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::M1 => 0,
            Self::M5 => 1,
            Self::H1 => 2,
            Self::D1 => 3,
        }
    }

    /// Start of the bucket containing `ts`: `ts − ts mod interval`.
    #[must_use]
    pub const fn bucket(self, ts: Timestamp) -> Timestamp {
        Timestamp(ts.0 - ts.0.rem_euclid(self.millis()))
    }
}

/// One OHLCV bar. Prices in cents.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Candle {
    /// Start of the bucket (aligned to a multiple of the interval).
    pub open_ts: Timestamp,
    /// First price in the bucket.
    pub open: i64,
    /// Highest price in the bucket.
    pub high: i64,
    /// Lowest price in the bucket.
    pub low: i64,
    /// Last price in the bucket.
    pub close: i64,
    /// Total volume in the bucket.
    pub volume: u64,
    /// Number of ticks aggregated.
    pub ticks: u32,
}

impl Candle {
    fn open_with(open_ts: Timestamp, t: &Tick) -> Self {
        Self {
            open_ts,
            open: t.price_cents,
            high: t.price_cents,
            low: t.price_cents,
            close: t.price_cents,
            volume: t.volume,
            ticks: 1,
        }
    }

    fn absorb(&mut self, t: &Tick) {
        self.high = self.high.max(t.price_cents);
        self.low = self.low.min(t.price_cents);
        self.close = t.price_cents;
        self.volume = self.volume.saturating_add(t.volume);
        self.ticks = self.ticks.saturating_add(1);
    }
}

/// Builds one candle at a time for a single interval.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub(crate) struct CandleBuilder {
    interval: Interval,
    current: Option<Candle>,
}

impl CandleBuilder {
    pub(crate) const fn new(interval: Interval) -> Self {
        Self {
            interval,
            current: None,
        }
    }

    /// Feed a tick; returns the candle that just closed, if any. Ticks are
    /// assumed to arrive in non-decreasing time order; an earlier tick is
    /// folded into the current candle.
    pub(crate) fn push(&mut self, t: &Tick) -> Option<Candle> {
        let bucket = self.interval.bucket(t.ts);
        match &mut self.current {
            Some(c) if bucket <= c.open_ts => {
                c.absorb(t);
                None
            }
            Some(c) => {
                let closed = *c;
                *c = Candle::open_with(bucket, t);
                Some(closed)
            }
            None => {
                self.current = Some(Candle::open_with(bucket, t));
                None
            }
        }
    }

    pub(crate) const fn current(&self) -> Option<&Candle> {
        self.current.as_ref()
    }
}

/// Aggregates ticks into 1 m / 5 m / 1 h / 1 d candles and keeps a bounded
/// history of each.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Candles {
    builders: [CandleBuilder; 4],
    history: [VecDeque<Candle>; 4],
    max_per_interval: usize,
}

impl Candles {
    /// Keep at most `max_per_interval` completed candles per interval; the
    /// oldest are evicted.
    #[must_use]
    pub fn new(max_per_interval: usize) -> Self {
        Self {
            builders: Interval::ALL.map(CandleBuilder::new),
            history: [const { VecDeque::new() }; 4],
            max_per_interval,
        }
    }

    /// Feed one tick. Returns, per interval in [`Interval::ALL`] order, the
    /// candle that just closed.
    pub fn push(&mut self, tick: &Tick) -> [Option<Candle>; 4] {
        let mut closed = [None; 4];
        for (i, b) in self.builders.iter_mut().enumerate() {
            if let Some(c) = b.push(tick) {
                let h = &mut self.history[i];
                if self.max_per_interval == 0 {
                    // Nothing retained.
                } else {
                    if h.len() >= self.max_per_interval {
                        h.pop_front();
                    }
                    h.push_back(c);
                }
                closed[i] = Some(c);
            }
        }
        closed
    }

    /// Completed candles for `iv`, oldest first.
    pub fn completed(&self, iv: Interval) -> impl ExactSizeIterator<Item = &Candle> + '_ {
        self.history[iv.index()].iter()
    }

    /// The candle currently being built for `iv`, if any tick has been fed.
    #[must_use]
    pub fn current(&self, iv: Interval) -> Option<&Candle> {
        self.builders[iv.index()].current()
    }

    /// Maximum completed candles retained per interval.
    #[must_use]
    pub fn max_per_interval(&self) -> usize {
        self.max_per_interval
    }
}
