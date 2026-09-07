//! Rate limiting: how fast one client may change the market.
//!
//! Reads are cheap, idempotent and public, so they are not limited here — a
//! reverse proxy is the right place for that. What is limited is everything
//! that *changes* something: orders, amendments, cancels, stops, money and
//! the game master's events. One client with a socket to spare could
//! otherwise submit orders as fast as it can open them, and every submission
//! takes the one market lock.
//!
//! The bucket is per user, because a key is the only thing the server can
//! honestly identify a client by: it does not see addresses, and would not
//! trust a forwarded one it could not verify. Requests that carry no key
//! share a single bucket, which is a blunt instrument and deliberately so —
//! the only unauthenticated writes are signing up and, on a server with no
//! `FEHU_ADMIN_KEY`, the game master's own endpoints.

use std::collections::BTreeMap;
use std::time::Duration;

use crate::account::UserId;

/// Buckets kept before the full ones are swept away. A bucket that has
/// refilled completely says nothing a fresh one would not, so dropping it
/// costs a client nothing and bounds what a stream of new users can hold.
const MAX_BUCKETS: usize = 10_000;

/// How fast one client may change things, and how much of that it may spend
/// at once.
#[derive(Clone, Copy, Debug)]
pub struct Rate {
    /// Requests per second, sustained. `0` turns limiting off entirely.
    pub per_sec: f64,
    /// Requests that may arrive at once after a quiet spell.
    pub burst: f64,
}

impl Rate {
    /// Whether this rate limits anything at all.
    #[must_use]
    pub fn is_off(&self) -> bool {
        self.per_sec <= 0.0 || self.burst <= 0.0
    }
}

/// One client's allowance: tokens that refill at [`Rate::per_sec`] and are
/// spent one per request.
#[derive(Clone, Copy, Debug)]
struct Bucket {
    tokens: f64,
    /// Milliseconds since the limiter started, when `tokens` was last true.
    at_ms: u64,
}

/// The token buckets, one per user plus one shared by requests with no key.
#[derive(Debug)]
pub struct Limiter {
    rate: Rate,
    users: BTreeMap<UserId, Bucket>,
    anonymous: Bucket,
}

/// What a bucket said. A refusal carries how long the client should wait,
/// which is what the `Retry-After` header is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Allowed,
    /// Refused; try again after this long.
    Limited {
        retry_after: Duration,
    },
}

impl Limiter {
    /// A limiter at `rate`, with everybody's bucket starting full.
    #[must_use]
    pub fn new(rate: Rate) -> Self {
        Self {
            rate,
            users: BTreeMap::new(),
            anonymous: Bucket {
                tokens: rate.burst,
                at_ms: 0,
            },
        }
    }

    /// The rate this limiter enforces.
    #[must_use]
    pub fn rate(&self) -> Rate {
        self.rate
    }

    /// Spend one request's worth of `who`'s allowance, `now_ms` milliseconds
    /// after the limiter started. `None` is the shared anonymous bucket.
    pub fn take(&mut self, who: Option<UserId>, now_ms: u64) -> Decision {
        if self.rate.is_off() {
            return Decision::Allowed;
        }
        let rate = self.rate;
        let bucket = if let Some(user) = who {
            if !self.users.contains_key(&user) {
                if self.users.len() >= MAX_BUCKETS {
                    // Only full buckets go: a client mid-burst keeps its
                    // place, so sweeping can never hand out free requests.
                    self.users
                        .retain(|_, b| refill(b, rate, now_ms) < rate.burst);
                }
                if self.users.len() < MAX_BUCKETS {
                    self.users.insert(
                        user,
                        Bucket {
                            tokens: rate.burst,
                            at_ms: now_ms,
                        },
                    );
                }
            }
            // With no room left — every bucket mid-burst — a client we
            // cannot track shares the anonymous one. That is stricter than
            // its own allowance, never looser: running out of memory must
            // not become a way to buy requests.
            self.users.get_mut(&user).unwrap_or(&mut self.anonymous)
        } else {
            &mut self.anonymous
        };
        let tokens = refill(bucket, rate, now_ms);
        bucket.at_ms = now_ms;
        if tokens >= 1.0 {
            bucket.tokens = tokens - 1.0;
            Decision::Allowed
        } else {
            bucket.tokens = tokens;
            // Long enough to have earned the token that was missing.
            let secs = (1.0 - tokens) / rate.per_sec;
            Decision::Limited {
                retry_after: Duration::from_secs_f64(secs.clamp(0.0, 3600.0)),
            }
        }
    }

    /// Buckets currently held, for `/api/health`.
    #[must_use]
    pub fn tracked(&self) -> usize {
        self.users.len()
    }
}

/// `bucket`'s tokens brought up to `now_ms`, capped at the burst. The clock
/// only ever moves forward here, but a bucket created in the same call can
/// be exactly at `now_ms`, so the elapsed time is saturating.
fn refill(bucket: &mut Bucket, rate: Rate, now_ms: u64) -> f64 {
    let elapsed = now_ms.saturating_sub(bucket.at_ms) as f64 / 1000.0;
    let tokens = bucket.tokens + elapsed * rate.per_sec;
    bucket.tokens = tokens.min(rate.burst);
    bucket.at_ms = now_ms;
    bucket.tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: Rate = Rate {
        per_sec: 2.0,
        burst: 3.0,
    };

    fn allowed(d: Decision) -> bool {
        d == Decision::Allowed
    }

    #[test]
    fn a_burst_is_spent_then_refills_at_the_rate() {
        let mut limiter = Limiter::new(RATE);
        let user = Some(UserId(1));
        for i in 0..3 {
            assert!(allowed(limiter.take(user, 0)), "burst request {i}");
        }
        let Decision::Limited { retry_after } = limiter.take(user, 0) else {
            panic!("the fourth request in the same instant must be refused");
        };
        assert!(
            (retry_after.as_secs_f64() - 0.5).abs() < 1e-6,
            "half a second earns the next token at 2/s: {retry_after:?}"
        );
        // Half a second later there is exactly one token, and no more.
        assert!(allowed(limiter.take(user, 500)));
        assert!(!allowed(limiter.take(user, 500)));
        // A long quiet spell refills to the burst and no further.
        assert!(allowed(limiter.take(user, 60_000)));
        assert!(allowed(limiter.take(user, 60_000)));
        assert!(allowed(limiter.take(user, 60_000)));
        assert!(!allowed(limiter.take(user, 60_000)));
    }

    #[test]
    fn one_client_cannot_spend_anothers_allowance() {
        let mut limiter = Limiter::new(RATE);
        for _ in 0..3 {
            assert!(allowed(limiter.take(Some(UserId(1)), 0)));
        }
        assert!(!allowed(limiter.take(Some(UserId(1)), 0)));
        assert!(allowed(limiter.take(Some(UserId(2)), 0)));
        // And the anonymous bucket is separate from both.
        assert!(allowed(limiter.take(None, 0)));
        assert!(allowed(limiter.take(None, 0)));
        assert!(allowed(limiter.take(None, 0)));
        assert!(!allowed(limiter.take(None, 0)));
        assert!(allowed(limiter.take(Some(UserId(2)), 0)));
    }

    #[test]
    fn a_zero_rate_limits_nothing() {
        let mut limiter = Limiter::new(Rate {
            per_sec: 0.0,
            burst: 0.0,
        });
        for _ in 0..1_000 {
            assert!(allowed(limiter.take(Some(UserId(1)), 0)));
        }
    }

    #[test]
    fn a_full_table_gets_stricter_rather_than_looser() {
        let mut limiter = Limiter::new(RATE);
        // One client mid-burst, then enough others to fill the table. None
        // of them is full, so there is nothing to sweep.
        let held = Some(UserId(0));
        for _ in 0..3 {
            assert!(allowed(limiter.take(held, 0)));
        }
        for i in 1..MAX_BUCKETS as u64 {
            assert!(allowed(limiter.take(Some(UserId(i)), 0)));
        }
        assert_eq!(limiter.tracked(), MAX_BUCKETS, "the table is full");

        // A client there is no room for falls back to the shared bucket
        // instead of being given one of its own.
        let crowd = MAX_BUCKETS as u64 + 1;
        for i in 0..3 {
            assert!(allowed(limiter.take(Some(UserId(crowd + i)), 0)));
        }
        assert!(!allowed(limiter.take(Some(UserId(crowd + 99)), 0)));
        assert_eq!(limiter.tracked(), MAX_BUCKETS, "and nothing was evicted");
        assert!(
            !allowed(limiter.take(held, 0)),
            "the client mid-burst kept its empty bucket"
        );

        // Once the table's buckets refill they are swept, and a new client
        // gets its own again.
        assert!(allowed(limiter.take(Some(UserId(crowd)), 600_000)));
        assert!(limiter.tracked() < MAX_BUCKETS, "the full ones went");
    }
}
