//! Property tests: any in-range config runs without panics or non-positive
//! prices; out-of-range configs are rejected naming the field.

use core::time::Duration;
use fehu::{
    Config, ConfigError, Event, EventKind, GarchParams, JumpParams, MarketHours, Simulator,
    Timestamp, VolumeParams,
};
use proptest::prelude::*;

fn tick_strategy() -> impl Strategy<Value = Duration> {
    prop_oneof![
        Just(Duration::from_millis(1)),
        Just(Duration::from_millis(250)),
        Just(Duration::from_secs(1)),
        Just(Duration::from_secs(60)),
        Just(Duration::from_secs(3600)),
    ]
}

fn market_hours_strategy() -> impl Strategy<Value = Option<MarketHours>> {
    prop_oneof![
        Just(None),
        (0u32..80_000, 3600u32..86_400, 1u8..128, 0.0f64..=2.0).prop_map(
            |(open, len, weekdays, gap_weight)| {
                let close = (open + len).min(86_400);
                Some(MarketHours {
                    open_secs: open.min(close - 1),
                    close_secs: close,
                    weekdays,
                    gap_weight,
                })
            }
        ),
    ]
}

fn config_strategy() -> impl Strategy<Value = Config> {
    (
        tick_strategy(),
        market_hours_strategy(),
        1i64..=1_000_000_000_000_000,
        -2.0f64..=2.0,
        0.0f64..=5.0,
        1e-3f64..=1e5,
        0.0f64..=1e5,
    )
        .prop_flat_map(|(tick, mh, start, drift, vol, theta, kappa)| {
            // Variance half-life at least 4 ticks so β ≥ 0, and at most a year.
            let min_hl = (tick.as_secs_f64() * 4.0).max(1.0);
            (
                Just((tick, mh, start, drift, vol, theta, kappa)),
                min_hl..=(365.25 * 86_400.0),
                0.0f64..1.0,
                0.0f64..=1e4,
                -1.0f64..=1.0,
                0.0f64..=1.0,
                1.0f64..=1e15,
                0.0f64..=100.0,
                0.0f64..=4.0,
                0.0f64..=3.0,
            )
        })
        .prop_map(
            |(
                (tick, mh, start, drift, vol, theta, kappa),
                hl,
                r,
                intensity,
                jmean,
                jstd,
                base,
                sens,
                vexp,
                noise,
            )| Config {
                start_price_cents: start,
                drift,
                volatility: vol,
                mean_reversion_speed: theta,
                fundamental_speed: kappa,
                garch: GarchParams {
                    variance_half_life: Duration::from_secs_f64(hl),
                    variance_dispersion: r,
                },
                jumps: JumpParams {
                    intensity,
                    mean: jmean,
                    std: jstd,
                },
                volume: VolumeParams {
                    base_per_day: base,
                    return_sensitivity: sens,
                    vol_exponent: vexp,
                    noise,
                },
                tick,
                start_ts: Timestamp(0),
                market_hours: mh,
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn in_range_configs_run_and_stay_positive(cfg in config_strategy(), seed in any::<u64>()) {
        // A tick longer than the session is the one coupling the strategy
        // does not avoid; that must be reported as a config error, not a panic.
        let sim = match Simulator::new(cfg.clone(), seed) {
            Ok(s) => s,
            Err(ConfigError::OutOfRange { field: "tick", .. }) if cfg.market_hours.is_some() => return Ok(()),
            Err(e) => panic!("valid config rejected: {e} ({cfg:?})"),
        };
        let mut sim = sim;
        sim.push_event(Event { at: Timestamp(0), kind: EventKind::Jump(-0.999) }).unwrap();
        sim.push_event(Event { at: Timestamp(0), kind: EventKind::Jump(50.0) }).unwrap();
        sim.push_event(Event {
            at: Timestamp(0),
            kind: EventKind::DriftShift { delta: -1e4, half_life: Duration::from_secs(1) },
        }).unwrap();
        sim.push_event(Event {
            at: Timestamp(0),
            kind: EventKind::VolShift { delta: 5.0, half_life: Duration::from_secs(60) },
        }).unwrap();
        sim.push_event(Event { at: Timestamp(0), kind: EventKind::FundamentalShift(-3.0) }).unwrap();
        for _ in 0..1000 {
            let t = sim.step();
            prop_assert!(t.price_cents >= 1, "price {} with {cfg:?}", t.price_cents);
            prop_assert!(t.price_cents <= 1_000_000_000_000_000);
            let s = sim.snapshot();
            prop_assert!(s.log_spread.is_finite() && s.annual_vol.is_finite() && s.annual_vol >= 0.0);
        }
    }

    #[test]
    fn out_of_range_fields_are_named(
        which in 0usize..9,
        bad in prop_oneof![Just(f64::NAN), Just(f64::INFINITY), Just(-1e18), Just(1e18)],
    ) {
        let mut cfg = Config::default();
        let field = match which {
            0 => { cfg.drift = bad; "drift" }
            1 => { cfg.volatility = bad; "volatility" }
            2 => { cfg.mean_reversion_speed = bad; "mean_reversion_speed" }
            3 => { cfg.fundamental_speed = bad; "fundamental_speed" }
            4 => { cfg.garch.variance_dispersion = bad; "garch.variance_dispersion" }
            5 => { cfg.jumps.intensity = bad; "jumps.intensity" }
            6 => { cfg.jumps.mean = bad; "jumps.mean" }
            7 => { cfg.volume.base_per_day = bad; "volume.base_per_day" }
            _ => { cfg.volume.noise = bad; "volume.noise" }
        };
        match cfg.validate() {
            Err(ConfigError::NotFinite { field: f }) | Err(ConfigError::OutOfRange { field: f, .. }) => {
                prop_assert_eq!(f, field);
            }
            Err(other) => prop_assert!(false, "unexpected error {other}"),
            Ok(()) => prop_assert!(false, "accepted {field} = {bad}"),
        }
    }
}
