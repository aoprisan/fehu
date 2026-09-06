use core::time::Duration;
use fehu::{Config, Simulator, Timestamp};

#[test]
fn ticks_advance_time_and_stay_positive() {
    let mut sim = Simulator::new(Config::default(), 1).unwrap();
    let ticks: Vec<_> = sim.advance(Duration::from_secs(600)).collect();
    assert_eq!(ticks.len(), 601, "ticks at t=0..=600 inclusive");
    assert_eq!(ticks[0].ts, Timestamp(0));
    assert_eq!(ticks[600].ts, Timestamp(600_000));
    assert!(ticks.iter().all(|t| t.price_cents >= 1));
    let next = sim.step();
    assert_eq!(next.ts, Timestamp(601_000));
}

#[test]
fn same_seed_same_series() {
    let mut a = Simulator::new(Config::default(), 99).unwrap();
    let mut b = Simulator::new(Config::default(), 99).unwrap();
    for _ in 0..10_000 {
        assert_eq!(a.step(), b.step());
    }
    let mut c = Simulator::new(Config::default(), 100).unwrap();
    assert!((0..100).any(|_| a.step() != c.step()));
}

#[test]
fn price_tracks_fundamental_input() {
    let cfg = Config {
        volatility: 0.0,
        tick: Duration::from_secs(3600),
        ..Config::default()
    };
    let mut sim = Simulator::new(cfg, 0).unwrap();
    sim.set_fundamental_input(20_000);
    // 365 days of hourly ticks: many half-lives of both relaxations.
    let last = sim
        .advance(Duration::from_secs(365 * 86_400))
        .last()
        .unwrap();
    // Fundamental drift of 5 %/yr on top of the doubling.
    let expected = 20_000.0 * (0.05f64).exp();
    let rel = (last.price_cents as f64 - expected).abs() / expected;
    assert!(
        rel < 0.002,
        "price {} expected {expected}",
        last.price_cents
    );
}
