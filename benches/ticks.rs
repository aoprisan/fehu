//! Ticks per second for the default config, with and without market hours.

use core::time::Duration;
use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use fehu::{Candles, Config, Interval, MarketHours, Simulator};
use std::hint::black_box;

const N: u64 = 100_000;

fn bench_ticks(c: &mut Criterion) {
    let mut g = c.benchmark_group("ticks");
    g.throughput(Throughput::Elements(N));

    g.bench_function("default", |b| {
        let mut sim = Simulator::new(Config::default(), 1).unwrap();
        b.iter(|| {
            for _ in 0..N {
                black_box(sim.step());
            }
        });
    });

    g.bench_function("market_hours", |b| {
        let cfg = Config {
            market_hours: Some(MarketHours::default()),
            ..Config::default()
        };
        let mut sim = Simulator::new(cfg, 1).unwrap();
        b.iter(|| {
            for _ in 0..N {
                black_box(sim.step());
            }
        });
    });

    g.bench_function("advance_with_candles", |b| {
        let mut sim = Simulator::new(Config::default(), 1).unwrap();
        let mut candles = Candles::new(1000);
        b.iter(|| {
            for t in sim.advance(Duration::from_secs(N - 1)) {
                black_box(candles.push(&t));
            }
        });
    });

    g.finish();

    let mut g = c.benchmark_group("coarse");
    let days: u64 = 10 * 365;
    g.throughput(Throughput::Elements(days));
    g.bench_function("daily_10y", |b| {
        let mut sim = Simulator::new(Config::default(), 1).unwrap();
        b.iter(|| {
            for c in sim.coarse_candles(Interval::D1).take(days as usize) {
                black_box(c);
            }
        });
    });
    g.finish();
}

criterion_group!(benches, bench_ticks);
criterion_main!(benches);
