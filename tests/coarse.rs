//! Coarse mode: one latent step per candle, high/low from the Brownian-bridge
//! extremum distribution.

use core::time::Duration;
use fehu::{
    Candle, Candles, Config, Event, EventKind, GarchParams, Interval, JumpParams, MarketHours,
    Simulator, Timestamp,
};

const DAY_MS: i64 = 86_400_000;
const YEAR_SECS: f64 = 365.25 * 86_400.0;

fn check_invariants(bars: &[Candle], iv: Interval) {
    for w in bars.windows(2) {
        assert_eq!(w[1].open, w[0].close, "bars must chain");
        assert!(w[1].open_ts > w[0].open_ts);
    }
    for b in bars {
        assert_eq!(b.open_ts, iv.bucket(b.open_ts), "bucket aligned");
        assert!(b.high >= b.open.max(b.close), "{b:?}");
        assert!(b.low <= b.open.min(b.close), "{b:?}");
        assert!(b.low >= 1);
        assert_eq!(b.ticks, 0);
    }
}

#[test]
fn bars_are_well_formed_for_every_interval() {
    for iv in Interval::ALL {
        let mut sim = Simulator::new(Config::default(), 1).unwrap();
        let bars: Vec<_> = sim.coarse_candles(iv).take(2000).collect();
        check_invariants(&bars, iv);
        assert_eq!(bars[1].open_ts - bars[0].open_ts, iv.millis());
        let mut sim = Simulator::new(
            Config {
                market_hours: Some(MarketHours::default()),
                ..Config::default()
            },
            1,
        )
        .unwrap();
        let bars: Vec<_> = sim.coarse_candles(iv).take(2000).collect();
        check_invariants(&bars, iv);
    }
}

#[test]
fn deterministic_with_golden_hash() {
    let mut a = Simulator::new(Config::default(), 7).unwrap();
    let mut b = Simulator::new(Config::default(), 7).unwrap();
    let x: Vec<_> = a.coarse_candles(Interval::D1).take(5000).collect();
    let y: Vec<_> = b.coarse_candles(Interval::D1).take(5000).collect();
    assert_eq!(x, y);

    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |v: i64| {
        for byte in v.to_le_bytes() {
            h ^= u64::from(byte);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    };
    for c in &x[..1000] {
        for v in [c.open_ts.0, c.open, c.high, c.low, c.close, c.volume as i64] {
            feed(v);
        }
    }
    assert_eq!(h, GOLDEN_COARSE_D1, "got {h:#018x}");
}

const GOLDEN_COARSE_D1: u64 = 0xa396_0c56_9f9c_cb58;

/// Random-walk-like config with no jumps or vol regimes, priced high enough
/// that cent rounding is negligible.
fn smooth(tick: Duration) -> Config {
    Config {
        start_price_cents: 1_000_000_000,
        drift: 0.0,
        volatility: 0.30,
        mean_reversion_speed: 1e-3,
        jumps: JumpParams {
            intensity: 0.0,
            ..JumpParams::default()
        },
        garch: GarchParams {
            variance_half_life: Duration::from_secs(30 * 86_400),
            variance_dispersion: 0.0,
        },
        tick,
        ..Config::default()
    }
}

fn stats(bars: &[Candle]) -> (f64, f64) {
    let r: Vec<f64> = bars
        .iter()
        .map(|b| (b.close as f64 / b.open as f64).ln())
        .collect();
    let mean = r.iter().sum::<f64>() / r.len() as f64;
    let std = (r.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (r.len() - 1) as f64).sqrt();
    let range = bars
        .iter()
        .map(|b| (b.high as f64 / b.low as f64).ln())
        .sum::<f64>()
        / bars.len() as f64;
    (std, range)
}

#[test]
fn daily_bars_match_the_fine_path_statistically() {
    // Coarse: 4 years of daily bars in 1461 steps.
    let mut coarse = Simulator::new(smooth(Duration::from_secs(1)), 3).unwrap();
    let cb: Vec<_> = coarse.coarse_candles(Interval::D1).take(1461).collect();
    // Fine: one year of 1-minute ticks aggregated to daily bars.
    let mut fine = Simulator::new(smooth(Duration::from_secs(60)), 4).unwrap();
    let mut agg = Candles::new(400);
    for t in fine.advance(Duration::from_secs(365 * 86_400)) {
        agg.push(&t);
    }
    let fb: Vec<_> = agg.completed(Interval::D1).copied().collect();

    let sigma_day = 0.30 * (86_400.0 / YEAR_SECS).sqrt();
    let (c_std, c_range) = stats(&cb);
    let (f_std, f_range) = stats(&fb);
    // Daily std: both ≈ σ √(1/365.25).
    assert!(
        (c_std / sigma_day - 1.0).abs() < 0.08,
        "coarse std {c_std} vs {sigma_day}"
    );
    assert!(
        (f_std / sigma_day - 1.0).abs() < 0.12,
        "fine std {f_std} vs {sigma_day}"
    );
    // Expected range of Brownian motion over a day: 2 √(2/π) σ √T ≈ 1.596 σ √T.
    let theory = 2.0 * (2.0 / core::f64::consts::PI).sqrt() * sigma_day;
    assert!(
        (c_range / theory - 1.0).abs() < 0.05,
        "coarse range {c_range} vs {theory}"
    );
    // Minute sampling misses some of the true extremes, so the fine range sits
    // a little below the continuous-time value.
    assert!(
        (f_range / theory - 1.0).abs() < 0.10,
        "fine range {f_range} vs {theory}"
    );
    assert!(
        (c_range / f_range - 1.0).abs() < 0.10,
        "coarse {c_range} vs fine {f_range}"
    );
}

#[test]
fn market_hours_daily_and_hourly_bars() {
    let cfg = Config {
        market_hours: Some(MarketHours::default()),
        ..Config::default()
    };
    let mut sim = Simulator::new(cfg.clone(), 1).unwrap();
    // 4 weeks from Thursday: Thu, Fri + 3×5 + Mon–Wed = 20 trading days.
    let days: Vec<_> = sim
        .advance_coarse(Duration::from_secs(27 * 86_400 + 12 * 3600), Interval::D1)
        .collect();
    assert_eq!(days.len(), 20);
    assert!(days.iter().all(|c| c.open_ts.0 % DAY_MS == 0));
    let mh = MarketHours::default();
    assert!(days.iter().all(|c| mh.is_trading_day(c.open_ts.0 / DAY_MS)));
    check_invariants(&days, Interval::D1);
    // The simulator is parked at the next open.
    assert_eq!(sim.next_tick_ts(), mh.next_open(sim.clock()));

    let mut sim = Simulator::new(cfg, 1).unwrap();
    let hours: Vec<_> = sim.coarse_candles(Interval::H1).take(14).collect();
    // 09:30–16:00 spans the 09:00 … 15:00 buckets: 7 bars per day.
    let expected: Vec<i64> = (9..16)
        .map(|h| h * 3_600_000)
        .chain((9..16).map(|h| DAY_MS + h * 3_600_000))
        .collect();
    assert_eq!(
        hours.iter().map(|c| c.open_ts.0).collect::<Vec<_>>(),
        expected
    );
    check_invariants(&hours, Interval::H1);
}

#[test]
fn partial_first_bucket_then_aligned() {
    let cfg = Config {
        start_ts: Timestamp(30 * 60_000 + 7_000),
        ..Config::default()
    };
    let mut sim = Simulator::new(cfg, 2).unwrap();
    let bars: Vec<_> = sim.coarse_candles(Interval::H1).take(3).collect();
    assert_eq!(bars[0].open_ts, Timestamp(0));
    assert_eq!(bars[1].open_ts, Timestamp(3_600_000));
    assert_eq!(bars[2].open_ts, Timestamp(7_200_000));
    assert_eq!(bars[0].open, 10_000);
}

#[test]
fn advance_coarse_yields_bars_that_close_within_dur() {
    let mut sim = Simulator::new(Config::default(), 2).unwrap();
    let n = sim
        .advance_coarse(Duration::from_secs(10 * 86_400), Interval::D1)
        .count();
    assert_eq!(n, 10);
    // Half a day more: no bar closes, but the clock moved.
    let n = sim
        .advance_coarse(Duration::from_secs(43_200), Interval::D1)
        .count();
    assert_eq!(n, 0);
    assert_eq!(sim.clock(), Timestamp(10 * DAY_MS + 43_200_000));
    let n = sim
        .advance_coarse(Duration::from_secs(43_200), Interval::D1)
        .count();
    assert_eq!(n, 1);
}

#[test]
fn events_apply_in_the_bar_that_contains_them() {
    let mut sim = Simulator::new(
        Config {
            volatility: 0.0,
            drift: 0.0,
            jumps: JumpParams {
                intensity: 0.0,
                ..JumpParams::default()
            },
            ..Config::default()
        },
        1,
    )
    .unwrap();
    // A jump 3.5 days in: bar index 3 (covering day 3) carries it, not bar 2.
    sim.push_event(Event {
        at: Timestamp(3 * DAY_MS + DAY_MS / 2),
        kind: EventKind::Jump(0.10),
    })
    .unwrap();
    // And a permanent +20 % that the series converges to.
    sim.push_event(Event {
        at: Timestamp(10 * DAY_MS),
        kind: EventKind::FundamentalShift(0.2),
    })
    .unwrap();
    let bars: Vec<_> = sim.coarse_candles(Interval::D1).take(400).collect();
    assert_eq!(bars[2].close, 10_000);
    assert_eq!(bars[3].open, 10_000);
    // The bar's close has the jump minus one day of reversion (half-life ≈ 5 d).
    let one_day_decay = (-50.0 * 86_400.0 / YEAR_SECS).exp();
    let expected = (10_000.0 * (1.10f64).ln().mul_add(one_day_decay, 0.0).exp()).round() as i64;
    assert!(
        (bars[3].close - expected).abs() <= 1,
        "{} vs {expected}",
        bars[3].close
    );
    assert_eq!(bars[3].high, bars[3].close);
    let last = bars.last().unwrap().close as f64;
    let target = 10_000.0 * (0.2f64).exp();
    assert!((last - target).abs() / target < 1e-3, "{last} vs {target}");
}

#[test]
fn coarse_and_fine_stepping_can_be_mixed() {
    let mut sim = Simulator::new(Config::default(), 5).unwrap();
    let bar = sim.coarse_candles(Interval::H1).next().unwrap();
    assert_eq!(bar.open_ts, Timestamp(0));
    let t = sim.step();
    assert_eq!(t.ts, Timestamp(3_600_000));
    assert!((t.price_cents - bar.close).abs() < bar.close / 20);
    let next = sim.coarse_candles(Interval::H1).next().unwrap();
    assert_eq!(next.open_ts, Timestamp(3_600_000));
    assert_eq!(next.open, t.price_cents);
    assert_eq!(sim.next_tick_ts(), Timestamp(7_200_000));
}

fn autocorr_abs_returns(bars: &[Candle], lag: usize) -> f64 {
    let x: Vec<f64> = bars
        .iter()
        .map(|b| (b.close as f64 / b.open as f64).ln().abs())
        .collect();
    let n = x.len();
    let mean = x.iter().sum::<f64>() / n as f64;
    let var = x.iter().map(|v| (v - mean).powi(2)).sum::<f64>();
    (0..n - lag)
        .map(|i| (x[i] - mean) * (x[i + lag] - mean))
        .sum::<f64>()
        / var
}

#[test]
fn garch_regime_scales_with_the_step() {
    let base = Config {
        start_price_cents: 1_000_000_000,
        jumps: JumpParams {
            intensity: 0.0,
            ..JumpParams::default()
        },
        ..Config::default()
    };
    // A 1-hour variance half-life averages out within a day: no daily clustering.
    let mut sim = Simulator::new(base.clone(), 6).unwrap();
    let bars: Vec<_> = sim.coarse_candles(Interval::D1).take(4000).collect();
    let ac = autocorr_abs_returns(&bars, 1);
    assert!(ac.abs() < 0.06, "unexpected daily clustering {ac}");
    // A 30-day half-life shows up as clustering of daily |returns|.
    let slow = Config {
        garch: GarchParams {
            variance_half_life: Duration::from_secs(30 * 86_400),
            variance_dispersion: 0.5,
        },
        ..base
    };
    let mut sim = Simulator::new(slow, 6).unwrap();
    let bars: Vec<_> = sim.coarse_candles(Interval::D1).take(4000).collect();
    for lag in 1..=5 {
        let ac = autocorr_abs_returns(&bars, lag);
        assert!(
            ac > 0.05,
            "autocorrelation of daily |r| at lag {lag} is {ac}"
        );
    }
    // The unconditional daily variance is still σ² / 365.25.
    let (std, _) = stats(&bars);
    let sigma_day = 0.40 * (86_400.0 / YEAR_SECS).sqrt();
    assert!(
        (std / sigma_day - 1.0).abs() < 0.15,
        "daily std {std} vs {sigma_day}"
    );
}

#[test]
fn thirty_years_of_daily_history_is_cheap() {
    let mut sim = Simulator::new(Config::default(), 9).unwrap();
    let bars: Vec<_> = sim.coarse_candles(Interval::D1).take(30 * 365).collect();
    check_invariants(&bars, Interval::D1);
    assert_eq!(
        bars.last().unwrap().open_ts,
        Timestamp((30 * 365 - 1) * DAY_MS)
    );
}
