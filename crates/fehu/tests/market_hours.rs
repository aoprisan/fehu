use core::time::Duration;
use fehu::{Config, GarchParams, JumpParams, MarketHours, Simulator, Timestamp};

const DAY_MS: i64 = 86_400_000;

fn cfg() -> Config {
    Config {
        market_hours: Some(MarketHours::default()),
        ..Config::default()
    }
}

#[test]
fn ticks_only_inside_sessions() {
    let mut sim = Simulator::new(cfg(), 1).unwrap();
    let mh = MarketHours::default();
    // Start (Thursday 00:00) is outside the session: first tick at 09:30.
    assert_eq!(sim.next_tick_ts(), Timestamp(34_200_000));
    // Ends at 21:30 on day 13 (a Wednesday), inside a closed period.
    let ticks: Vec<_> = sim
        .advance(Duration::from_secs(13 * 86_400 + 12 * 3600))
        .collect();
    assert!(ticks.iter().all(|t| mh.contains(t.ts)));
    // 10 trading days (Thu, Fri, Mon–Fri, Mon–Wed) × 6.5 h × 3600 ticks.
    assert_eq!(ticks.len(), 10 * 23_400);
    // Ticks are strictly increasing and spaced by the tick or a session gap.
    for w in ticks.windows(2) {
        let d = w[1].ts - w[0].ts;
        assert!(d >= 1000);
    }
    // Weekend: Friday's last tick at 15:59:59, Monday's first at 09:30.
    let fri_last = ticks.iter().rev().find(|t| t.ts.0 < 4 * DAY_MS).unwrap();
    assert_eq!(fri_last.ts, Timestamp(DAY_MS + 57_599_000));
    let mon_first = ticks.iter().find(|t| t.ts.0 >= 4 * DAY_MS).unwrap();
    assert_eq!(mon_first.ts, Timestamp(4 * DAY_MS + 34_200_000));
}

#[test]
fn opening_tick_carries_the_gap() {
    // Quiet market except for the OU noise; compare the size of the opening
    // move with a regular tick over many sessions.
    let c = Config {
        jumps: JumpParams {
            intensity: 0.0,
            ..JumpParams::default()
        },
        garch: GarchParams {
            variance_dispersion: 0.0,
            ..GarchParams::default()
        },
        start_price_cents: 1_000_000_000,
        ..cfg()
    };
    let mut sim = Simulator::new(c, 2).unwrap();
    let ticks: Vec<_> = sim.advance(Duration::from_secs(60 * 86_400)).collect();
    let (mut gap_s2, mut gap_n, mut reg_s2, mut reg_n) = (0.0, 0, 0.0, 0);
    for w in ticks.windows(2) {
        let r = (w[1].price_cents as f64 / w[0].price_cents as f64).ln();
        if w[1].ts - w[0].ts > 1000 {
            gap_s2 += r * r;
            gap_n += 1;
        } else {
            reg_s2 += r * r;
            reg_n += 1;
        }
    }
    let gap_var = gap_s2 / gap_n as f64;
    let reg_var = reg_s2 / reg_n as f64;
    // Gap covers g·S + tick = 0.3 · 23 400 + 1 model-seconds vs 1 for a tick.
    let expected_ratio = 0.3 * 23_400.0 + 1.0;
    let ratio = gap_var / reg_var;
    assert!(
        (ratio / expected_ratio - 1.0).abs() < 0.4,
        "gap/regular variance ratio {ratio}, expected ≈ {expected_ratio} (n={gap_n})"
    );
}

#[test]
fn realised_annual_vol_matches_config_including_gaps() {
    let c = Config {
        jumps: JumpParams {
            intensity: 0.0,
            ..JumpParams::default()
        },
        garch: GarchParams {
            variance_dispersion: 0.0,
            ..GarchParams::default()
        },
        // Very slow reversion so the OU noise is essentially a random walk.
        mean_reversion_speed: 1e-3,
        start_price_cents: 1_000_000_000,
        ..cfg()
    };
    let mut sim = Simulator::new(c, 4).unwrap();
    // One year of sessions ≈ 261 trading days.
    let ticks: Vec<_> = sim.advance(Duration::from_secs(365 * 86_400)).collect();
    let s2: f64 = ticks
        .windows(2)
        .map(|w| {
            let r = (w[1].price_cents as f64 / w[0].price_cents as f64).ln();
            r * r
        })
        .sum();
    // Sum of squared returns over one calendar year ≈ σ² · (days/365.25).
    let days = ticks.len() as f64 / 23_400.0;
    let annual_var = s2 / (days / MarketHours::default().trading_days_per_year());
    let realised = annual_var.sqrt();
    assert!(
        (realised - 0.40).abs() < 0.06,
        "realised annual vol {realised} vs configured 0.40 over {days} sessions"
    );
}

#[test]
fn custom_calendar_and_validation() {
    let mh = MarketHours {
        open_secs: 0,
        close_secs: 3600,
        weekdays: 0b100_0000, // Sundays only
        gap_weight: 0.0,
    };
    let c = Config {
        market_hours: Some(mh),
        ..Config::default()
    };
    let mut sim = Simulator::new(c, 1).unwrap();
    // Day 3 is the first Sunday after the epoch.
    assert_eq!(sim.next_tick_ts(), Timestamp(3 * DAY_MS));
    // Three Sundays, ending midweek.
    let n = sim
        .advance(Duration::from_secs(20 * 86_400 + 12 * 3600))
        .count();
    assert_eq!(n, 3 * 3600);

    let bad = Config {
        market_hours: Some(MarketHours {
            weekdays: 0,
            ..MarketHours::default()
        }),
        ..Config::default()
    };
    assert!(Simulator::new(bad, 1).is_err());
    let bad = Config {
        market_hours: Some(MarketHours {
            open_secs: 50_000,
            close_secs: 40_000,
            ..MarketHours::default()
        }),
        ..Config::default()
    };
    assert!(Simulator::new(bad, 1).is_err());
    let bad = Config {
        tick: Duration::from_secs(7 * 3600),
        garch: GarchParams {
            variance_half_life: Duration::from_secs(30 * 86_400),
            ..GarchParams::default()
        },
        ..cfg()
    };
    assert!(
        Simulator::new(bad, 1).is_err(),
        "tick longer than the session"
    );
}
