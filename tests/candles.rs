use core::time::Duration;
use fehu::{Candles, Config, Interval, Simulator, Tick, Timestamp};

fn tick(secs: i64, price: i64, volume: u64) -> Tick {
    Tick {
        ts: Timestamp(secs * 1000),
        price_cents: price,
        volume,
    }
}

#[test]
fn ohlcv_and_bucket_alignment() {
    let mut c = Candles::new(10);
    // Start mid-minute so the first bucket is aligned down to 0.
    let ticks = [
        tick(30, 100, 5),
        tick(45, 110, 5),
        tick(59, 90, 5),
        tick(60, 95, 1), // closes the first 1m bar
        tick(119, 97, 1),
        tick(300, 120, 2), // closes 1m and 5m bars
    ];
    let mut closed_m1 = Vec::new();
    let mut closed_m5 = Vec::new();
    for t in &ticks {
        let closed = c.push(t);
        closed_m1.extend(closed[0]);
        closed_m5.extend(closed[1]);
    }
    assert_eq!(closed_m1.len(), 2);
    let first = closed_m1[0];
    assert_eq!(first.open_ts, Timestamp(0));
    assert_eq!(
        (first.open, first.high, first.low, first.close),
        (100, 110, 90, 90)
    );
    assert_eq!(first.volume, 15);
    assert_eq!(first.ticks, 3);
    let second = closed_m1[1];
    assert_eq!(second.open_ts, Timestamp(60_000));
    assert_eq!(
        (second.open, second.high, second.low, second.close),
        (95, 97, 95, 97)
    );
    assert_eq!(closed_m5.len(), 1);
    assert_eq!(closed_m5[0].open_ts, Timestamp(0));
    assert_eq!(
        (closed_m5[0].high, closed_m5[0].low, closed_m5[0].close),
        (110, 90, 97)
    );
    assert_eq!(closed_m5[0].volume, 17);
    // The in-progress bars.
    let cur = c.current(Interval::M1).unwrap();
    assert_eq!(
        (cur.open_ts, cur.open, cur.ticks),
        (Timestamp(300_000), 120, 1)
    );
    assert_eq!(c.current(Interval::D1).unwrap().ticks, 6);
    assert_eq!(c.completed(Interval::H1).len(), 0);
    assert_eq!(c.completed(Interval::M1).len(), 2);
}

#[test]
fn history_is_bounded() {
    let mut c = Candles::new(3);
    for i in 0..10 {
        c.push(&tick(i * 60, 100 + i, 1));
    }
    let kept: Vec<_> = c.completed(Interval::M1).map(|k| k.open_ts).collect();
    assert_eq!(
        kept,
        vec![
            Timestamp(6 * 60_000),
            Timestamp(7 * 60_000),
            Timestamp(8 * 60_000)
        ]
    );
    let mut none = Candles::new(0);
    for i in 0..5 {
        none.push(&tick(i * 60, 1, 1));
    }
    assert_eq!(none.completed(Interval::M1).len(), 0);
}

#[test]
fn negative_timestamps_bucket_correctly() {
    assert_eq!(Interval::M1.bucket(Timestamp(-1)), Timestamp(-60_000));
    assert_eq!(
        Interval::D1.bucket(Timestamp(-86_400_000)),
        Timestamp(-86_400_000)
    );
    assert_eq!(Interval::H1.bucket(Timestamp(3_599_999)), Timestamp(0));
}

#[test]
fn candle_stream_matches_manual_aggregation() {
    let mut a = Simulator::new(Config::default(), 3).unwrap();
    let mut b = Simulator::new(Config::default(), 3).unwrap();
    let streamed: Vec<_> = a.candles(Interval::M5).take(12).collect();
    let mut agg = Candles::new(100);
    let mut manual = Vec::new();
    while manual.len() < 12 {
        if let Some(c) = agg.push(&b.step())[1] {
            manual.push(c);
        }
    }
    assert_eq!(streamed, manual);
    assert_eq!(a.clock(), b.clock());
    // Continuing after the stream is dropped keeps the partial bar.
    let more: Vec<_> = a
        .advance_candles(Duration::from_secs(600), Interval::M5)
        .collect();
    let mut manual_more = Vec::new();
    for t in b.advance(Duration::from_secs(600)) {
        manual_more.extend(agg.push(&t)[1]);
    }
    assert_eq!(more, manual_more);
    assert_eq!(more.len(), 2);
}

#[test]
fn volume_is_positive_and_scales_with_moves() {
    let mut sim = Simulator::new(Config::default(), 8).unwrap();
    let ticks: Vec<_> = sim.advance(Duration::from_secs(3 * 3600)).collect();
    let mean = ticks.iter().map(|t| t.volume as f64).sum::<f64>() / ticks.len() as f64;
    // base_per_day / 86_400 ≈ 11.6 shares per second, boosted by |r| and regime.
    assert!(mean > 11.0 && mean < 200.0, "mean volume {mean}");
    assert!(ticks.iter().filter(|t| t.volume == 0).count() < ticks.len() / 20);
    // Larger absolute returns carry larger volume on average.
    let mut by_move: Vec<(f64, u64)> = ticks
        .windows(2)
        .map(|w| {
            let r = (w[1].price_cents as f64 / w[0].price_cents as f64)
                .ln()
                .abs();
            (r, w[1].volume)
        })
        .collect();
    by_move.sort_by(|a, b| a.0.total_cmp(&b.0));
    let n = by_move.len();
    let low: f64 = by_move[..n / 4].iter().map(|x| x.1 as f64).sum::<f64>() / (n / 4) as f64;
    let high: f64 =
        by_move[3 * n / 4..].iter().map(|x| x.1 as f64).sum::<f64>() / (n - 3 * n / 4) as f64;
    assert!(
        high > 1.5 * low,
        "low-move volume {low}, high-move volume {high}"
    );
}
