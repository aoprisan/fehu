//! How much the server will take: how fast from one client, and how much at
//! once from everybody.
//!
//! Two bounds, and they answer different questions. The [`Limiter`] is *per
//! client*: how fast one of them may change the market, so that no single
//! client can crowd the rest out. [`Admission`] is *global*: how much work
//! may be inside the server at one time, so that a hundred well-behaved
//! clients cannot do together what none of them could do alone.
//!
//! The second is what keeps latency finite. Every mutation runs as one job
//! on the market actor and its mailbox is unbounded, so without a bound the
//! answer to a burst is a queue that grows until memory runs out, with every
//! client in it waiting longer and longer for a reply that will eventually
//! arrive far too late to be worth anything. A server that will not take the
//! work should say so at once — `503 overloaded`, with a `Retry-After` —
//! rather than accept it and be slow.
//!
//! # Rate limiting: how fast one client may change the market
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
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

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

// ---------------------------------------------------------------------------
// Admission control: how much work may be inside the server at once.

/// Mutating requests admitted at once by default.
///
/// They are serialised by the market actor anyway, so a larger number does
/// not make the server faster: it makes the queue longer and every reply
/// later. What this is really choosing is the worst wait a client can be
/// made to sit through before it is told the answer is "later" instead.
pub const DEFAULT_MAX_INFLIGHT: usize = 128;

/// Stream connections held open at once by default. Each one holds a
/// broadcast receiver and a task, and nothing else bounds how many a client
/// may open.
pub const DEFAULT_MAX_STREAMS: usize = 256;

/// A place in the server, given out while there is one and held for as long
/// as the work lasts.
///
/// Dropping it gives the place back, so it is carried by the request that
/// took it — for a stream, by the connection, for as long as it is open.
#[derive(Debug)]
pub struct Place(#[allow(dead_code)] Option<OwnedSemaphorePermit>);

/// The bound on concurrent work: mutations in flight, and streams open.
///
/// Both are `0` for "no bound", which is what the tests that are about
/// something else want and what a single-player world on localhost can
/// afford.
#[derive(Clone, Debug)]
pub struct Admission {
    mutations: Option<Arc<Semaphore>>,
    streams: Option<Arc<Semaphore>>,
    max_mutations: usize,
    max_streams: usize,
}

impl Admission {
    /// A bound of `mutations` concurrent changes and `streams` open
    /// connections. `0` for either leaves that one unbounded.
    #[must_use]
    pub fn new(mutations: usize, streams: usize) -> Self {
        Self {
            mutations: (mutations > 0).then(|| Arc::new(Semaphore::new(mutations))),
            streams: (streams > 0).then(|| Arc::new(Semaphore::new(streams))),
            max_mutations: mutations,
            max_streams: streams,
        }
    }

    /// Take a place for a mutating request, or `None` if the server is full.
    ///
    /// Never waits: the whole point is to answer at once rather than join a
    /// queue whose length nothing bounds.
    #[must_use]
    pub fn mutation(&self) -> Option<Place> {
        Self::take(self.mutations.as_ref())
    }

    /// Take a place for a stream connection, or `None` if there is no room.
    #[must_use]
    pub fn stream(&self) -> Option<Place> {
        Self::take(self.streams.as_ref())
    }

    fn take(gate: Option<&Arc<Semaphore>>) -> Option<Place> {
        match gate {
            None => Some(Place(None)),
            Some(gate) => Arc::clone(gate)
                .try_acquire_owned()
                .ok()
                .map(|permit| Place(Some(permit))),
        }
    }

    /// Mutating requests in flight right now.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.mutations
            .as_ref()
            .map_or(0, |g| self.max_mutations - g.available_permits())
    }

    /// Stream connections holding a place right now.
    #[must_use]
    pub fn streams_open(&self) -> usize {
        self.streams
            .as_ref()
            .map_or(0, |g| self.max_streams - g.available_permits())
    }

    /// The bound on mutations in flight; `0` when there is none.
    #[must_use]
    pub fn max_in_flight(&self) -> usize {
        self.max_mutations
    }

    /// The bound on open streams; `0` when there is none.
    #[must_use]
    pub fn max_streams(&self) -> usize {
        self.max_streams
    }
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

    #[test]
    fn a_place_is_held_until_it_is_dropped_and_then_reused() {
        let admission = Admission::new(2, 1);
        let first = admission.mutation().expect("room for the first");
        let second = admission.mutation().expect("room for the second");
        assert_eq!(admission.in_flight(), 2);
        assert!(
            admission.mutation().is_none(),
            "the third is turned away rather than queued"
        );
        drop(first);
        assert_eq!(admission.in_flight(), 1);
        let third = admission.mutation().expect("the place came back");
        assert_eq!(admission.in_flight(), 2);
        drop((second, third));
        assert_eq!(admission.in_flight(), 0);
    }

    #[test]
    fn streams_and_mutations_are_bounded_separately() {
        let admission = Admission::new(1, 1);
        let request = admission.mutation().expect("room for a request");
        let connection = admission.stream().expect("and for a connection");
        assert!(admission.mutation().is_none());
        assert!(admission.stream().is_none());
        assert_eq!(admission.streams_open(), 1);
        drop(connection);
        assert!(
            admission.stream().is_some(),
            "a closed connection gives its place back"
        );
        drop(request);
    }

    #[test]
    fn a_zero_bound_admits_everything() {
        let admission = Admission::new(0, 0);
        let places: Vec<_> = (0..10_000).filter_map(|_| admission.mutation()).collect();
        assert_eq!(places.len(), 10_000);
        assert_eq!(admission.in_flight(), 0, "nothing is being counted");
        assert_eq!(admission.max_in_flight(), 0);
        assert!(admission.stream().is_some());
    }
}
