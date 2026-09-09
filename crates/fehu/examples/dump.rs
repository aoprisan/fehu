//! Write one year of 1-second ticks and the daily candles to CSV.
//!
//! ```text
//! cargo run --release --example dump -- [OUT_DIR] [SEED] [--market-hours]
//! ```
//!
//! Produces `OUT_DIR/ticks.csv` (`ts_ms,price_cents,volume`, ≈ 31.5 M rows
//! around the clock or ≈ 6.1 M with market hours) and `OUT_DIR/daily.csv`
//! (`open_ts_ms,open,high,low,close,volume,ticks`).

use std::fs::File;
use std::io::{BufWriter, Write};
use std::time::{Duration, Instant};

use fehu::{Candles, Config, Interval, MarketHours, Simulator};

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let market_hours = args.iter().any(|a| a == "--market-hours");
    let positional: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
    let out_dir = positional.first().map_or(".", |s| s.as_str());
    let seed: u64 = positional
        .get(1)
        .map_or(42, |s| s.parse().expect("seed must be a u64"));

    std::fs::create_dir_all(out_dir)?;
    let mut ticks =
        BufWriter::with_capacity(1 << 20, File::create(format!("{out_dir}/ticks.csv"))?);
    let mut daily = BufWriter::new(File::create(format!("{out_dir}/daily.csv"))?);
    writeln!(ticks, "ts_ms,price_cents,volume")?;
    writeln!(daily, "open_ts_ms,open,high,low,close,volume,ticks")?;

    let cfg = Config {
        market_hours: market_hours.then(MarketHours::default),
        ..Config::default()
    };
    let mut sim = Simulator::new(cfg, seed).expect("default config is valid");
    let mut candles = Candles::new(400);

    let start = Instant::now();
    let mut n = 0u64;
    for t in sim.advance(Duration::from_secs(365 * 86_400)) {
        writeln!(ticks, "{},{},{}", t.ts.0, t.price_cents, t.volume)?;
        candles.push(&t);
        n += 1;
    }
    // Close the last day so it is written too.
    let last_day = candles.current(Interval::D1).copied();
    for c in candles.completed(Interval::D1).chain(last_day.iter()) {
        writeln!(
            daily,
            "{},{},{},{},{},{},{}",
            c.open_ts.0, c.open, c.high, c.low, c.close, c.volume, c.ticks
        )?;
    }
    ticks.flush()?;
    daily.flush()?;

    let secs = start.elapsed().as_secs_f64();
    let snap = sim.snapshot();
    eprintln!(
        "wrote {n} ticks and {} daily candles to {out_dir} in {secs:.1}s ({:.0} ticks/s incl. I/O); \
         final price {} cents, fundamental {} cents",
        candles.completed(Interval::D1).len() + usize::from(last_day.is_some()),
        n as f64 / secs,
        snap.price_cents,
        snap.fundamental_cents,
    );
    Ok(())
}
