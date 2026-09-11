//! Save-game round trips (requires the `serde` feature).
//!
//! Note: `serde_json` needs its `float_roundtrip` feature for a bit-exact
//! round trip; without it a parsed `f64` may be one ulp off. Binary formats
//! such as `postcard` are always exact.
#![cfg(feature = "serde")]

use core::time::Duration;
use fehu::{
    Candles, Config, Event, EventKind, Interval, LoadError, MarketHours, STATE_VERSION, Simulator,
    Timestamp,
};

fn busy() -> Simulator {
    let cfg = Config {
        market_hours: Some(MarketHours::default()),
        ..Config::default()
    };
    let mut sim = Simulator::new(cfg, 9).unwrap();
    sim.advance(Duration::from_secs(20 * 3600)).for_each(drop);
    sim.push_event(Event {
        at: Timestamp(30 * 3600 * 1000),
        kind: EventKind::Jump(0.03),
    })
    .unwrap();
    sim.push_event(Event {
        at: sim.clock(),
        kind: EventKind::VolShift {
            delta: 0.3,
            half_life: Duration::from_secs(7200),
        },
    })
    .unwrap();
    sim.push_event(Event {
        at: sim.clock(),
        kind: EventKind::DriftShift {
            delta: 2.0,
            half_life: Duration::from_secs(7200),
        },
    })
    .unwrap();
    sim.step();
    // Leave a partial candle in the stream cursor.
    sim.candles(Interval::M5).next();
    sim.step();
    sim
}

fn continue_same(mut a: Simulator, mut b: Simulator) {
    for _ in 0..30_000 {
        assert_eq!(a.step(), b.step());
    }
    let ca: Vec<_> = a
        .advance_candles(Duration::from_secs(3600), Interval::M5)
        .collect();
    let cb: Vec<_> = b
        .advance_candles(Duration::from_secs(3600), Interval::M5)
        .collect();
    assert_eq!(ca, cb);
    assert_eq!(a.snapshot(), b.snapshot());
}

#[test]
fn json_round_trip() {
    let a = busy();
    let json = serde_json::to_string(&a).unwrap();
    let b: Simulator = serde_json::from_str(&json).unwrap();
    assert_eq!(a.snapshot(), b.snapshot());
    continue_same(a, b);
}

#[test]
fn postcard_round_trip() {
    let a = busy();
    let bytes = postcard::to_allocvec(&a).unwrap();
    let b: Simulator = postcard::from_bytes(&bytes).unwrap();
    continue_same(a, b);
}

#[test]
fn a_standalone_book_keeps_its_rules_and_a_snapshot_serialises() {
    let mut book = fehu::OrderBook::new();
    book.set_rules(fehu::MarketRules {
        tick_cents: 5,
        lot: 10,
    });
    let json = serde_json::to_string(&book).unwrap();
    let back: fehu::OrderBook = serde_json::from_str(&json).unwrap();
    assert_eq!(
        back.rules(),
        book.rules(),
        "the tick and lot survive: {json}"
    );
    // A file written before the rules were saved reads back with the
    // default rather than being refused.
    let without: serde_json::Value = {
        let mut v: serde_json::Value = serde_json::from_str(&json).unwrap();
        v.as_object_mut().unwrap().remove("rules");
        v
    };
    let old: fehu::OrderBook = serde_json::from_value(without).unwrap();
    assert_eq!(old.rules(), fehu::MarketRules::default());

    let snapshot = busy().snapshot();
    let back: fehu::Snapshot =
        serde_json::from_str(&serde_json::to_string(&snapshot).unwrap()).unwrap();
    assert_eq!(back, snapshot);
}

#[test]
fn candles_round_trip() {
    let mut sim = Simulator::new(Config::default(), 1).unwrap();
    let mut c = Candles::new(50);
    for t in sim.advance(Duration::from_secs(2 * 3600 + 90)) {
        c.push(&t);
    }
    let json = serde_json::to_string(&c).unwrap();
    let d: Candles = serde_json::from_str(&json).unwrap();
    assert_eq!(c, d);
    assert_eq!(d.completed(Interval::M1).len(), 50);
    assert_eq!(d.completed(Interval::H1).len(), 2);
    assert!(d.current(Interval::M1).is_some());
}

#[test]
fn wrong_version_and_bad_config_are_rejected() {
    let a = busy();
    let mut v: serde_json::Value = serde_json::to_value(&a).unwrap();
    v["version"] = serde_json::json!(STATE_VERSION + 1);
    let err = serde_json::from_value::<Simulator>(v.clone()).unwrap_err();
    assert!(err.to_string().contains("version"), "{err}");

    let mut v: serde_json::Value = serde_json::to_value(&a).unwrap();
    v["config"]["volatility"] = serde_json::json!(99.0);
    let err = serde_json::from_value::<Simulator>(v).unwrap_err();
    assert!(err.to_string().contains("volatility"), "{err}");

    let repr: fehu::SimulatorRepr = a.into();
    assert_eq!(repr.version, STATE_VERSION);
    let _ = LoadError::Corrupt; // exported
}
