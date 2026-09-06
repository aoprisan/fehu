//! Same seed + same events ⇒ identical output, pinned by a golden hash that
//! must match under every feature combination and on every platform.

use core::time::Duration;
use fehu::{Config, Event, EventKind, MarketHours, Simulator, Tick, Timestamp};

fn fnv1a(ticks: impl IntoIterator<Item = Tick>) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |bytes: &[u8]| {
        for b in bytes {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    };
    for t in ticks {
        feed(&t.ts.0.to_le_bytes());
        feed(&t.price_cents.to_le_bytes());
        feed(&t.volume.to_le_bytes());
    }
    h
}

fn with_events(seed: u64) -> Simulator {
    let mut sim = Simulator::new(Config::default(), seed).unwrap();
    sim.push_event(Event {
        at: Timestamp(1_000_000),
        kind: EventKind::Jump(0.05),
    })
    .unwrap();
    sim.push_event(Event {
        at: Timestamp(2_000_000),
        kind: EventKind::DriftShift {
            delta: 3.0,
            half_life: Duration::from_secs(1800),
        },
    })
    .unwrap();
    sim.push_event(Event {
        at: Timestamp(3_000_000),
        kind: EventKind::VolShift {
            delta: 0.5,
            half_life: Duration::from_secs(900),
        },
    })
    .unwrap();
    sim.push_event(Event {
        at: Timestamp(4_000_000),
        kind: EventKind::FundamentalShift(0.1),
    })
    .unwrap();
    sim
}

#[test]
fn identical_series_for_identical_inputs() {
    let mut a = with_events(7);
    let mut b = with_events(7);
    for _ in 0..100_000 {
        assert_eq!(a.step(), b.step());
    }
    assert_eq!(a.snapshot(), b.snapshot());
}

#[test]
fn golden_hash_default_config() {
    let mut sim = Simulator::new(Config::default(), 42).unwrap();
    let h = fnv1a((0..10_000).map(|_| sim.step()));
    assert_eq!(h, GOLDEN_DEFAULT, "got {h:#018x}");
}

#[test]
fn golden_hash_with_events_and_market_hours() {
    let mut sim = with_events(42);
    let h = fnv1a((0..10_000).map(|_| sim.step()));
    assert_eq!(h, GOLDEN_EVENTS, "got {h:#018x}");

    let cfg = Config {
        market_hours: Some(MarketHours::default()),
        ..Config::default()
    };
    let mut sim = Simulator::new(cfg, 42).unwrap();
    let h = fnv1a(sim.advance(Duration::from_secs(3 * 86_400)));
    assert_eq!(h, GOLDEN_MARKET_HOURS, "got {h:#018x}");
}

// Pinned outputs. If a deliberate change to the draw order or model alters
// these, bump `STATE_VERSION` and update them.
const GOLDEN_DEFAULT: u64 = 0x8872_ac21_7ef8_77a8;
const GOLDEN_EVENTS: u64 = 0x3d1d_5d8c_77e8_b2c0;
const GOLDEN_MARKET_HOURS: u64 = 0x2503_7189_602e_037f;
