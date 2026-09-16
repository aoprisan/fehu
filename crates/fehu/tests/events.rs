//! Each event kind moves the series in the expected direction and magnitude.

use core::time::Duration;
use fehu::{Config, Event, EventError, EventKind, GarchParams, JumpParams, Simulator, Timestamp};

const YEAR_SECS: f64 = 365.25 * 86_400.0;

/// Noise-free config: no diffusion, no random jumps, 1-minute ticks.
fn quiet(theta: f64) -> Config {
    Config {
        volatility: 0.0,
        drift: 0.0,
        mean_reversion_speed: theta,
        jumps: JumpParams {
            intensity: 0.0,
            ..JumpParams::default()
        },
        garch: GarchParams {
            variance_half_life: Duration::from_secs(86_400),
            ..GarchParams::default()
        },
        tick: Duration::from_secs(60),
        ..Config::default()
    }
}

#[test]
fn the_queue_can_be_read_and_an_event_withdrawn() {
    let mut sim = Simulator::new(quiet(50.0), 1).unwrap();
    assert!(sim.events().is_empty());
    let later = Event {
        at: Timestamp(120_000),
        kind: EventKind::Jump(0.50),
    };
    let sooner = Event {
        at: Timestamp(60_000),
        kind: EventKind::Jump(0.10),
    };
    sim.push_event(later).unwrap();
    sim.push_event(sooner).unwrap();

    // Listed in the order they will apply, each named by its push number.
    let queued = sim.events();
    assert_eq!(queued.len(), 2);
    assert_eq!(queued[0], (1, sooner), "pushed second, applies first");
    assert_eq!(queued[1], (0, later));
    assert_eq!(sim.snapshot().pending_events, 2);

    // Withdrawn, the later jump never happens; the sooner one still does.
    assert_eq!(sim.retract(0), Some(later));
    assert_eq!(sim.retract(0), None, "gone is gone");
    assert_eq!(sim.retract(99), None, "never pushed");
    assert_eq!(sim.events(), vec![(1, sooner)]);
    let p0 = sim.step().price_cents;
    let p1 = sim.step().price_cents;
    let expected = (p0 as f64 * 1.10).round() as i64;
    assert!(
        (p1 - expected).abs() <= 1,
        "the sooner jump applied: {p0} -> {p1}"
    );
    assert!(sim.events().is_empty(), "and left the queue");
    let p2 = sim.step().price_cents;
    assert!(
        p2 < (p1 as f64 * 1.4) as i64,
        "the withdrawn jump never applied: {p1} -> {p2}"
    );
}

#[test]
fn jump_moves_price_by_pct_then_reverts() {
    let mut sim = Simulator::new(quiet(50.0), 1).unwrap();
    let p0 = sim.step().price_cents;
    sim.push_event(Event {
        at: Timestamp(60_000),
        kind: EventKind::Jump(0.10),
    })
    .unwrap();
    let t = sim.step();
    assert_eq!(t.ts, Timestamp(60_000));
    let expected = (p0 as f64 * 1.10).round() as i64;
    assert!(
        (t.price_cents - expected).abs() <= 1,
        "{} vs {expected}",
        t.price_cents
    );
    // A negative jump of −25 %.
    sim.push_event(Event {
        at: Timestamp(120_000),
        kind: EventKind::Jump(-0.25),
    })
    .unwrap();
    let t2 = sim.step();
    let expected2 = (t.price_cents as f64 * 0.75).round() as i64;
    assert!(
        (t2.price_cents - expected2).abs() <= 2,
        "{} vs {expected2}",
        t2.price_cents
    );
    // Reverts toward the fundamental afterwards.
    let later = sim
        .advance(Duration::from_secs(30 * 86_400))
        .last()
        .unwrap();
    let rel = (later.price_cents - p0).abs() as f64 / p0 as f64;
    assert!(
        rel < 0.02,
        "price {} did not revert to {p0}",
        later.price_cents
    );
}

#[test]
fn mean_reversion_half_life_matches_config() {
    let theta = 50.0;
    let mut sim = Simulator::new(quiet(theta), 1).unwrap();
    sim.push_event(Event {
        at: Timestamp(0),
        kind: EventKind::Jump(0.10),
    })
    .unwrap();
    let tick_secs = 60.0;
    let target = (1.10f64).ln() / 2.0;
    let mut n = 0u64;
    loop {
        sim.step();
        if sim.snapshot().log_spread <= target {
            break;
        }
        n += 1;
        assert!(n < 1_000_000, "spread never halved");
    }
    let measured = n as f64 * tick_secs;
    let expected = core::f64::consts::LN_2 / theta * YEAR_SECS;
    assert!(
        (measured - expected).abs() <= tick_secs,
        "half-life {measured}s vs expected {expected}s"
    );
}

#[test]
fn fundamental_shift_is_permanent() {
    let mut sim = Simulator::new(quiet(50.0), 1).unwrap();
    let p0 = sim.step().price_cents;
    sim.push_event(Event {
        at: Timestamp(0),
        kind: EventKind::FundamentalShift(0.2),
    })
    .unwrap();
    // Well over 100 half-lives of both relaxations.
    let last = sim
        .advance(Duration::from_secs(400 * 86_400))
        .last()
        .unwrap();
    let expected = p0 as f64 * (0.2f64).exp();
    let rel = (last.price_cents as f64 - expected).abs() / expected;
    assert!(rel < 1e-3, "{} vs {expected}", last.price_cents);
    let snap = sim.snapshot();
    assert!(snap.log_spread.abs() < 1e-6);
    assert!(snap.pending_events == 0);
}

#[test]
fn drift_shift_matches_closed_form() {
    // s(t) = δ (e^{−λt} − e^{−θt}) / (θ − λ), t in years, with s(0) = 0.
    let theta = 50.0;
    let half_life = Duration::from_secs(6 * 3600);
    let delta = 20.0; // annualised drift
    let lambda = core::f64::consts::LN_2 / (half_life.as_secs_f64() / YEAR_SECS);
    let mut sim = Simulator::new(quiet(theta), 1).unwrap();
    sim.push_event(Event {
        at: Timestamp(0),
        kind: EventKind::DriftShift { delta, half_life },
    })
    .unwrap();
    let mut max_err: f64 = 0.0;
    let mut peak = 0.0f64;
    for k in 1..=(2 * 24 * 60) {
        sim.step();
        let t = k as f64 * 60.0 / YEAR_SECS;
        let expected = delta * ((-lambda * t).exp() - (-theta * t).exp()) / (theta - lambda);
        let got = sim.snapshot().log_spread;
        max_err = max_err.max((got - expected).abs());
        peak = peak.max(got);
    }
    assert!(max_err < 1e-9, "max error {max_err}");
    assert!(
        peak > 0.005,
        "drift shift had no visible effect, peak {peak}"
    );
}

#[test]
fn drift_for_total_move_helper() {
    // With negligible reversion the total log move equals `total`.
    let total = 0.05;
    let half_life = Duration::from_secs(3600);
    let mut sim = Simulator::new(quiet(1e-3), 1).unwrap();
    sim.push_event(Event {
        at: Timestamp(0),
        kind: EventKind::drift_for_total_move(total, half_life),
    })
    .unwrap();
    sim.advance(Duration::from_secs(48 * 3600)).for_each(drop);
    let got = sim.snapshot().log_spread;
    assert!((got - total).abs() < 1e-4, "{got} vs {total}");
}

#[test]
fn vol_shift_raises_realised_vol() {
    // Stochastic: realised vol in the first half-life after the shift vs the
    // same seed without it. Expected ratio is the root of the analytic average
    // of σ_t² = (σ + δ 2^{−t/h})² over that window:
    //   mean(2^{−x}) = 1/(2 ln2), mean(4^{−x}) = 3/(8 ln2) for x ∈ [0, 1].
    // A large start price keeps cent rounding out of the 1-second returns.
    let sigma = 0.2;
    let delta = 0.6;
    let half_life = Duration::from_secs(3600);
    let cfg = Config {
        start_price_cents: 1_000_000_000,
        volatility: sigma,
        jumps: JumpParams {
            intensity: 0.0,
            ..JumpParams::default()
        },
        garch: GarchParams {
            variance_dispersion: 0.0,
            ..GarchParams::default()
        },
        ..Config::default()
    };
    let realised = |with_shift: bool| {
        let mut sim = Simulator::new(cfg.clone(), 11).unwrap();
        if with_shift {
            sim.push_event(Event {
                at: Timestamp(0),
                kind: EventKind::VolShift { delta, half_life },
            })
            .unwrap();
        }
        let mut prev = None;
        let mut s2 = 0.0;
        let mut n = 0;
        for t in sim.advance(half_life) {
            if let Some(p) = prev {
                let r = (t.price_cents as f64 / p as f64).ln();
                s2 += r * r;
                n += 1;
            }
            prev = Some(t.price_cents);
        }
        (s2 / n as f64).sqrt()
    };
    let ratio = realised(true) / realised(false);
    let ln2 = core::f64::consts::LN_2;
    let mean_var =
        sigma * sigma + 2.0 * sigma * delta / (2.0 * ln2) + delta * delta * 3.0 / (8.0 * ln2);
    let expected = mean_var.sqrt() / sigma;
    assert!(
        (ratio - expected).abs() / expected < 0.05,
        "ratio {ratio} vs expected {expected}"
    );
}

#[test]
fn effects_superpose_and_decay_away() {
    let mut sim = Simulator::new(quiet(50.0), 1).unwrap();
    for i in 0..3 {
        sim.push_event(Event {
            at: Timestamp(0),
            kind: EventKind::VolShift {
                delta: 0.1 * (i + 1) as f64,
                half_life: Duration::from_secs(600 * (i + 1) as u64),
            },
        })
        .unwrap();
    }
    sim.step();
    let snap = sim.snapshot();
    // Values after one 60 s tick of decay.
    let expected: f64 = (0..3)
        .map(|i| 0.1 * (i + 1) as f64 * 0.5f64.powf(60.0 / (600.0 * (i + 1) as f64)))
        .sum();
    assert!((snap.vol_effect - expected).abs() < 1e-12);
    sim.advance(Duration::from_secs(86_400)).for_each(drop);
    assert_eq!(sim.snapshot().vol_effect, 0.0, "effects should be pruned");
}

#[test]
fn events_apply_in_order_and_past_events_apply_next_tick() {
    let mut sim = Simulator::new(quiet(50.0), 1).unwrap();
    sim.advance(Duration::from_secs(600)).for_each(drop);
    // Two events in the past at the same timestamp: +10 % then −10 %.
    let p0 = sim.snapshot().price_cents;
    sim.push_event(Event {
        at: Timestamp(0),
        kind: EventKind::Jump(0.10),
    })
    .unwrap();
    sim.push_event(Event {
        at: Timestamp(0),
        kind: EventKind::Jump(-0.10),
    })
    .unwrap();
    let t = sim.step();
    let expected = (p0 as f64 * 0.99).round() as i64;
    assert!((t.price_cents - expected).abs() <= 1);
    assert_eq!(sim.snapshot().pending_events, 0);
}

#[test]
fn invalid_events_are_rejected() {
    let mut sim = Simulator::new(quiet(50.0), 1).unwrap();
    let at = Timestamp(0);
    assert_eq!(
        sim.push_event(Event {
            at,
            kind: EventKind::Jump(-1.0)
        }),
        Err(EventError::JumpBelowMinusOne)
    );
    assert_eq!(
        sim.push_event(Event {
            at,
            kind: EventKind::FundamentalShift(f64::NAN)
        }),
        Err(EventError::NotFinite)
    );
    assert_eq!(
        sim.push_event(Event {
            at,
            kind: EventKind::DriftShift {
                delta: 1.0,
                half_life: Duration::ZERO
            }
        }),
        Err(EventError::ZeroHalfLife)
    );
    assert_eq!(sim.snapshot().pending_events, 0);
}
