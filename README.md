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
- **Docs:** [`DESIGN.md`](DESIGN.md) has the model, parameter ranges,
  tick-scaling formulas, the per-tick algorithm, and (§14) the trading layer
  and why it sits on top of the price process rather than replacing it.
- **Tooling:** `just build | test | lint | bench | wasm | dump | serve`.

## Sample web app

[`webapp/`](webapp) is a small axum backend that shows the crate used the way
a game server would: four hardcoded, seeded symbols tick in wall-clock time,
the game pushes events over HTTP, traders send orders into each symbol's
book, and a browser UI draws the OHLC bars, the book, the tape and the
player's account live.

```text
cargo run --release -p fehu-webapp     # then open http://localhost:3000
```

| Method | Path | What |
|---|---|---|
| `GET` | `/api/symbols` | Quotes for every symbol |
| `GET` | `/api/symbols/{sym}` | Quote, latent snapshot and config |
| `GET` | `/api/symbols/{sym}/bars?interval=M1\|M5\|H1\|D1&limit=500` | OHLCV bars, oldest first, in-progress bar last |
| `POST` | `/api/symbols/{sym}/events` | Raw simulator event: `{"type":"jump","pct":-0.1}`, `drift_shift`, `drift_for_total_move`, `vol_shift`, `fundamental_shift`, `fundamental_target`; optional `at_ms` / `delay_secs`, `source`, `note` |
| `POST` | `/api/game/events` | Semantic game event: `{"kind":"scandal","symbol":"ACME","magnitude":1.5}`; market-wide kinds (`market_crash`, `rate_hike`, …) need no symbol |
| `GET` | `/api/game/catalog` | Every game-event kind and the simulator events it expands to |
| `GET` | `/api/events` | Audit log of accepted events, newest first (`?symbol=`, `?limit=`) |
| `POST` | `/api/traders` | Create a trader: `{"name":"alice","cash_cents":10000000}` (both optional) |
| `GET` | `/api/traders`, `/api/traders/{id}` | Traders; a portfolio with cash, positions marked to the reference price, open orders and fills |
| `POST` | `/api/traders/{id}/cancel_all` | Cancel every resting order of a trader |
| `POST` | `/api/symbols/{sym}/orders` | `{"trader_id":1,"side":"buy","qty":100,"type":"market"}` or `"type":"limit","price_cents":8400`, optional `"tif":"gtc\|ioc\|fok"`; responds with fills and status |
| `GET` | `/api/symbols/{sym}/orders?trader_id=` | A trader's resting orders on that symbol |
| `GET`/`DELETE` | `/api/symbols/{sym}/orders/{id}` | Look up / cancel (`?trader_id=`) a resting order |
| `GET` | `/api/symbols/{sym}/book?depth=10` | Aggregated bids and asks, reference price, pending trader flow |
| `GET` | `/api/symbols/{sym}/trades?limit=50` | The tape, newest first |
| `GET` | `/api/stream` | Server-sent events: `hello`, then every `tick` (with best bid/ask, top of book and the step's prints), accepted `event`, and `fill` |
| `GET` | `/api/health` | Uptime, simulated time, tick/trade counters |

At start-up each symbol generates a year of daily bars in coarse mode and then
three days of 1 s ticks, so every interval has history before the first
request. `FEHU_TIME_SCALE=60` runs the market at 60 simulated seconds per
wall second; `FEHU_BIND`, `FEHU_HISTORY_DAYS`, `FEHU_WARMUP_HOURS`,
`FEHU_STARTING_CASH_CENTS`, `FEHU_TAPE` and `FEHU_FILL_LOG` are the other
knobs. Same seeds and same events give the same prices on every run; trading
adds impact on top, so a market with no orders replays the bare simulation.
Traders are not persisted: they start with cash, no shares, no margin and
no shorting.

## License

MIT OR Apache-2.0.
