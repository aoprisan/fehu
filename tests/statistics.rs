//! Distributional properties of the default model.

use core::time::Duration;
use fehu::{Candles, Config, GarchParams, Interval, JumpParams, Simulator};

/// Log returns of 1-minute closes over `days` of 1-second ticks.
fn one_minute_returns(seed: u64, days: u64) -> Vec<f64> {
    let cfg = Config {
        start_price_cents: 1_000_000_000,
        ..Config::default()
    };
    let mut sim = Simulator::new(cfg, seed).unwrap();
    let closes: Vec<i64> = sim
        .advance_candles(Duration::from_secs(days * 86_400), Interval::M1)
        .map(|c| c.close)
        .collect();
    closes
        .windows(2)
        .map(|w| (w[1] as f64 / w[0] as f64).ln())
        .collect()
}

fn kurtosis(x: &[f64]) -> f64 {
    let n = x.len() as f64;
    let mean = x.iter().sum::<f64>() / n;
    let m2 = x.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n;
    let m4 = x.iter().map(|v| (v - mean).powi(4)).sum::<f64>() / n;
    m4 / (m2 * m2)
}

fn autocorr(x: &[f64], lag: usize) -> f64 {
    let n = x.len();
    let mean = x.iter().sum::<f64>() / n as f64;
    let var = x.iter().map(|v| (v - mean).powi(2)).sum::<f64>();
    let cov: f64 = (0..n - lag)
        .map(|i| (x[i] - mean) * (x[i + lag] - mean))
        .sum();
    cov / var
}

#[test]
fn one_minute_returns_have_fat_tails() {
    let r = one_minute_returns(1, 10);
    assert!(r.len() > 14_000);
    let k = kurtosis(&r);
    assert!(k > 3.5, "kurtosis {k}");
}

#[test]
fn absolute_returns_cluster() {
    let r = one_minute_returns(2, 10);
    let abs: Vec<f64> = r.iter().map(|v| v.abs()).collect();
    for lag in 1..=10 {
        let ac = autocorr(&abs, lag);
        assert!(ac > 0.05, "autocorrelation of |r| at lag {lag} is {ac}");
    }
    // Signed returns themselves are close to white noise.
    let ac1 = autocorr(&r, 1);
    assert!(ac1.abs() < 0.1, "return autocorrelation {ac1}");
}

#[test]
fn garch_dispersion_zero_is_gaussian_without_jumps() {
    let cfg = Config {
        start_price_cents: 1_000_000_000,
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
    let mut sim = Simulator::new(cfg, 3).unwrap();
    let mut agg = Candles::new(20_000);
    for t in sim.advance(Duration::from_secs(5 * 86_400)) {
        agg.push(&t);
    }
    let closes: Vec<f64> = agg
        .completed(Interval::M1)
        .map(|c| c.close as f64)
        .collect();
    let r: Vec<f64> = closes.windows(2).map(|w| (w[1] / w[0]).ln()).collect();
    let k = kurtosis(&r);
    assert!((k - 3.0).abs() < 0.3, "kurtosis {k}");
}

#[test]
fn stochastic_half_life_from_ar1_fit() {
    // Hourly ticks, θ = 500/yr (half-life ≈ 12 h): φ = e^{−θ dt}; estimate θ
    // from the AR(1) coefficient of the log-spread.
    let theta = 500.0;
    let cfg = Config {
        mean_reversion_speed: theta,
        jumps: JumpParams {
            intensity: 0.0,
            ..JumpParams::default()
        },
        garch: GarchParams {
            variance_half_life: Duration::from_secs(30 * 86_400),
            variance_dispersion: 0.0,
        },
        tick: Duration::from_secs(3600),
        ..Config::default()
    };
    let mut sim = Simulator::new(cfg, 5).unwrap();
    let n = 20_000;
    let s: Vec<f64> = (0..n)
        .map(|_| {
            sim.step();
            sim.snapshot().log_spread
        })
        .collect();
    let (mut xy, mut xx) = (0.0, 0.0);
    for w in s.windows(2) {
        xy += w[0] * w[1];
        xx += w[0] * w[0];
    }
    let phi = xy / xx;
    let dt = 3600.0 / (365.25 * 86_400.0);
    let theta_hat = -phi.ln() / dt;
    let rel = (theta_hat - theta).abs() / theta;
    assert!(rel < 0.25, "θ̂ = {theta_hat} vs {theta} (φ = {phi})");
}
