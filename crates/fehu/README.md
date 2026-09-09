# fehu

Deterministic synthetic stock price simulator for games. `no_std + alloc`,
bit-identical across native and WebAssembly.

A slow *fundamental value* is driven only by your game's inputs. The *price*
mean-reverts toward it with GARCH(1,1) volatility clustering and Poisson jumps,
so the series has fat tails, calm and panic regimes, shocks that fade, and
optional overnight gaps under a market-hours calendar. An optional *exchange*
wraps the price with a limit order book: synthetic makers quote around it,
the tick volume prints as synthetic flow, and traders' orders match against
both and move the price through square-root impact.

```rust
use core::time::Duration;
use fehu::{Candles, Config, Event, EventKind, Interval, Simulator, Timestamp};

let mut sim = Simulator::new(Config::default(), 42)?;

// The company just shipped: +8 % of fundamental value, plus a hype burst.
sim.push_event(Event { at: sim.clock(), kind: EventKind::FundamentalShift(0.08) })?;
sim.push_event(Event {
    at: sim.clock(),
    kind: EventKind::drift_for_total_move(0.05, Duration::from_secs(3600)),
})?;

// One-second ticks for the next game minute…
for tick in sim.advance(Duration::from_secs(60)) {
    println!("{} {} {}", tick.ts.0, tick.price_cents, tick.volume);
}
// …or 5-minute candles directly…
for candle in sim.advance_candles(Duration::from_secs(3600), Interval::M5) {
    println!("{:?}", candle);
}
// …or ten years of daily bars in one step per day (coarse mode: high/low are
// drawn from the Brownian-bridge extremum distribution instead of simulated).
let history: Vec<_> = sim.coarse_candles(Interval::D1).take(10 * 365).collect();
assert_eq!(history.len(), 3650);

// Trading: an exchange around a simulator. With no orders its ticks equal
// the bare simulator's; a trader's market buy sweeps the synthetic ladder
// and pushes the reference price up on the next tick, then the push fades.
use fehu::{Exchange, Order, Owner, Side, TraderId, TradingParams};
let mut ex = Exchange::new(Config::default(), TradingParams::default(), 42)?;
let fill = ex.submit(Order::market(Owner::Trader(TraderId(1)), Side::Buy, 5_000))?;
println!("bought {} at avg {:?}", fill.filled, fill.avg_price_cents());
let report = ex.step();
println!("{} prints, impact {:+.5}", report.trades.len(), report.impact);
# Ok::<(), Box<dyn std::error::Error>>(())
```

- **Features:** `std` (default, error trait impls and the example), `serde`
  (save-game round trip of the full simulator state).
- **Determinism:** `xoshiro256++` seeded by you, `libm` for all transcendental
  math, no clocks or globals. Same seed + same events ⇒ identical ticks on
  every platform.
- **Docs:** the module docs are the reference — `sim.rs` for the price
  process, `exchange.rs` for why the book sits on top of it rather than
  replacing it.
- **Tooling:** `just build | test | lint | bench | wasm | dump | serve`.
- **Server:** [`fehu-economy`](../fehu-economy) is the sample economy server
  built on this crate — an axum backend around seeded symbols, a conserved
  currency, goods and jobs, with a TypeScript UI.

## License

MIT OR Apache-2.0.
