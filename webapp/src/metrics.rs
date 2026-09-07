//! What the server is doing, counted while it does it.
//!
//! `GET /api/health` used to answer "is it up, and how much has happened" —
//! uptime, ticks, trades, users. That says nothing about whether it is
//! *keeping up*: how long a request takes, how long the engine step takes,
//! how many orders are arriving and how many are being turned away.
//!
//! These are counters and running maxima since start-up, not a time series:
//! there is no window, no percentile and no history, because a game server
//! that wants those should be scraped by something that keeps them. What is
//! here is enough to answer "is it slow, and since when" from one request,
//! and it costs an atomic add on a path that already does far more work.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::Serialize;

/// A count, a total and a worst case, all since start-up.
#[derive(Debug, Default)]
struct Timing {
    count: AtomicU64,
    micros_total: AtomicU64,
    micros_max: AtomicU64,
    micros_last: AtomicU64,
}

impl Timing {
    fn record(&self, took: Duration) {
        let micros = u64::try_from(took.as_micros()).unwrap_or(u64::MAX);
        self.count.fetch_add(1, Ordering::Relaxed);
        self.micros_total.fetch_add(micros, Ordering::Relaxed);
        self.micros_last.store(micros, Ordering::Relaxed);
        self.micros_max.fetch_max(micros, Ordering::Relaxed);
    }

    fn snapshot(&self) -> TimingDto {
        let count = self.count.load(Ordering::Relaxed);
        let micros_total = self.micros_total.load(Ordering::Relaxed);
        TimingDto {
            count,
            micros_last: self.micros_last.load(Ordering::Relaxed),
            micros_max: self.micros_max.load(Ordering::Relaxed),
            micros_mean: micros_total.checked_div(count).unwrap_or(0),
        }
    }
}

/// How often something happened and how long it took, since start-up.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct TimingDto {
    pub count: u64,
    /// The most recent one.
    pub micros_last: u64,
    /// The worst since start-up. Never decays: it is a high-water mark.
    pub micros_max: u64,
    pub micros_mean: u64,
}

/// Everything counted on the way past.
#[derive(Debug, Default)]
pub struct Metrics {
    /// Every HTTP request the router handled, whatever it answered.
    requests: Timing,
    /// Requests answered `4xx` or `5xx`, the rate-limited ones included.
    requests_failed: AtomicU64,
    /// Requests refused by the rate limiter.
    requests_limited: AtomicU64,
    /// One engine step: every symbol advanced and every fill booked.
    engine_step: Timing,
}

impl Metrics {
    /// Book one handled request.
    pub fn request(&self, took: Duration, status: u16) {
        self.requests.record(took);
        if status >= 400 {
            self.requests_failed.fetch_add(1, Ordering::Relaxed);
        }
        if status == 429 {
            self.requests_limited.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Book one engine step.
    pub fn engine_step(&self, took: Duration) {
        self.engine_step.record(took);
    }

    /// Everything counted so far.
    #[must_use]
    pub fn snapshot(&self) -> MetricsDto {
        MetricsDto {
            requests: self.requests.snapshot(),
            requests_failed: self.requests_failed.load(Ordering::Relaxed),
            requests_limited: self.requests_limited.load(Ordering::Relaxed),
            engine_step: self.engine_step.snapshot(),
        }
    }
}

/// The counters, as `GET /api/health` reports them.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct MetricsDto {
    pub requests: TimingDto,
    pub requests_failed: u64,
    pub requests_limited: u64,
    pub engine_step: TimingDto,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_counted_by_outcome_and_timed() {
        let metrics = Metrics::default();
        assert_eq!(metrics.snapshot().requests.count, 0);
        assert_eq!(
            metrics.snapshot().requests.micros_mean,
            0,
            "no requests is not a division by zero"
        );

        metrics.request(Duration::from_micros(100), 200);
        metrics.request(Duration::from_micros(300), 404);
        metrics.request(Duration::from_micros(200), 429);
        let m = metrics.snapshot();
        assert_eq!(m.requests.count, 3);
        assert_eq!(m.requests.micros_mean, 200);
        assert_eq!(m.requests.micros_max, 300, "the worst is kept");
        assert_eq!(m.requests.micros_last, 200);
        assert_eq!(m.requests_failed, 2, "429 is a failure too");
        assert_eq!(m.requests_limited, 1);

        // A fast request after a slow one does not lower the high-water mark.
        metrics.request(Duration::from_micros(1), 200);
        let m = metrics.snapshot();
        assert_eq!(m.requests.micros_max, 300);
        assert_eq!(m.requests.micros_last, 1);
    }

    #[test]
    fn engine_steps_are_timed_separately() {
        let metrics = Metrics::default();
        metrics.request(Duration::from_micros(10), 200);
        metrics.engine_step(Duration::from_micros(4_000));
        let m = metrics.snapshot();
        assert_eq!(m.engine_step.count, 1);
        assert_eq!(m.engine_step.micros_max, 4_000);
        assert_eq!(m.requests.count, 1, "a step is not a request");
    }
}
