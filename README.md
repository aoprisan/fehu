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
- **Sample app:** [`webapp/`](webapp) is an axum server around four seeded
  symbols, with a TypeScript UI in [`webapp/ui/`](webapp/ui).

## Sample web app

[`webapp/`](webapp) is a small axum backend that shows the crate used the way
a game server would: four hardcoded, seeded symbols tick in wall-clock time,
the game pushes events over HTTP, traders send orders into each symbol's
book, and a browser UI draws the OHLC bars, the book, the tape and the
player's account live.

```text
cargo run --release -p fehu-webapp     # then open http://localhost:3000
```

The UI is TypeScript ([`webapp/ui/`](webapp/ui/README.md)) built with Vite.
Its output is committed to `webapp/static/` and embedded into the binary, so
the command above needs no Node toolchain; `just ui` rebuilds it after a
change and `just ui-dev` serves it with hot reload against a running backend.

Market data — quotes, bars, the book, the tape, the event log — is open to
anyone. Everything that belongs to a player needs the API key they were
issued when they signed up, as `Authorization: Bearer <key>` (or
`X-Api-Key`), and a key only ever speaks for its own user: nobody else can
read that portfolio, cancel those orders or move that money. The key is shown
**once**, in the response that created the user — `POST /api/users` or
`POST /api/traders` — and is `null` in every response after.

| Method | Path | What |
|---|---|---|
| `GET` | `/api/symbols` | Quotes for every symbol |
| `GET` | `/api/symbols/{sym}` | Quote, latent snapshot, config and share count |
| `GET` | `/api/symbols/{sym}/shares` | The symbol's shares: outstanding, held by traders, bid for, still available, and who holds them |
| `GET` | `/api/symbols/{sym}/status` | Whether the symbol can be traded: session open, halted, the limit band and the next open/close |
| `POST` | `/api/symbols/{sym}/halt`, `/resume` | Game master: stop and start trading in one symbol |
| `GET` | `/api/symbols/{sym}/bars?interval=M1\|M5\|H1\|D1&limit=500` | OHLCV bars, oldest first, in-progress bar last |
| `POST` | `/api/symbols/{sym}/events` | Raw simulator event: `{"type":"jump","pct":-0.1}`, `drift_shift`, `drift_for_total_move`, `vol_shift`, `fundamental_shift`, `fundamental_target`; optional `at_ms` / `delay_secs`, `source`, `note` |
| `POST` | `/api/game/events` | Semantic game event: `{"kind":"scandal","symbol":"ACME","magnitude":1.5}`; market-wide kinds (`market_crash`, `rate_hike`, …) need no symbol |
| `GET` | `/api/game/catalog` | Every game-event kind and the simulator events it expands to |
| `GET` | `/api/events` | Audit log of accepted events, newest first (`?symbol=`, `?limit=`) |
| `POST` | `/api/users` | Create a user: `{"name":"ada","email":"ada@example.com"}` (both optional). The response carries their `api_key`, once |
| `GET` | `/api/users`, `/api/users/{id}` | The caller themselves: accounts, traders, cash and shares owned |
| `GET` | `/api/users/{id}/holdings` | Shares the user owns per symbol, added up over their traders, with what is reserved and what is still sellable |
| `POST` | `/api/users/{id}/accounts` | Open another account: `{"name":"main","cash_cents":10000000}` |
| `GET` | `/api/users/{id}/accounts`, `/api/accounts`, `/api/accounts/{id}` | Accounts: balance, reserved, available, status |
| `POST` | `/api/accounts/{id}/deposit` | Add money: `{"amount_cents":250000,"memo":"week 1"}` |
| `POST` | `/api/accounts/{id}/withdraw` | Take money out; only the available balance can leave |
| `POST` | `/api/accounts/{id}/status` | `{"status":"active\|frozen\|closed"}` |
| `GET` | `/api/accounts/{id}/ledger?limit=100` | Every movement of money, newest first |
| `GET` | `/api/accounts/{id}/validate` | Status, what the account may do, and any broken invariant |
| `POST` | `/api/traders` | Create a trader, with a user and a funded account: `{"name":"alice","cash_cents":10000000}` (both optional), and hand over the new user's `api_key`; `user_id` and `account_id` join existing ones, which needs that user's key |
| `GET` | `/api/traders`, `/api/traders/{id}` | Traders; a portfolio with cash, positions marked to the reference price, open orders and fills |
| `POST` | `/api/traders/{id}/deposit` | Add money to the trader's account: `{"amount_cents":250000}` |
| `POST` | `/api/traders/{id}/cancel_all` | Cancel every resting order of a trader |
| `POST` | `/api/symbols/{sym}/orders` | `{"trader_id":1,"side":"buy","qty":100,"type":"market"}` or `"type":"limit","price_cents":8400`, optional `"tif":"gtc\|ioc\|fok"`, `"client_order_id":"abc-1"` and `"post_only":true`; responds with fills and status |
| `GET` | `/api/symbols/{sym}/orders?trader_id=` | A trader's resting orders on that symbol |
| `GET`/`DELETE` | `/api/symbols/{sym}/orders/{id}` | Look up / cancel (`?trader_id=`) a resting order |
| `PATCH` | `/api/symbols/{sym}/orders/{id}` | Amend a resting order: `{"trader_id":1,"price_cents":8500,"qty":50}` — a cancel and a fresh order, so it loses queue position |
| `GET` | `/api/orders/{id}` | One order and what became of it — filled and cancelled ones included |
| `GET` | `/api/traders/{id}/orders?status=resting\|filled\|cancelled&limit=100` | A trader's orders, newest first |
| `POST` | `/api/symbols/{sym}/stops` | Arm a stop: `{"trader_id":1,"side":"sell","qty":100,"stop_price_cents":8000}`, plus `"limit_price_cents"` for a stop-limit. The trigger must be on the far side of the market |
| `GET` | `/api/symbols/{sym}/stops?trader_id=`, `/api/traders/{id}/stops` | A trader's held stops, on one symbol or all of them |
| `DELETE` | `/api/symbols/{sym}/stops/{id}?trader_id=` | Withdraw a stop before it fires |
| `GET` | `/api/symbols/{sym}/book?depth=10` | Aggregated bids and asks, reference price, pending trader flow |
| `GET` | `/api/symbols/{sym}/trades?limit=50` | The tape, newest first |
| `GET` | `/api/stream` | Server-sent events: `hello`, then every `tick` (with best bid/ask, top of book and the step's prints), accepted `event`, and — for `?api_key=`, since `EventSource` cannot set headers — that player's `fill`s |
| `GET` | `/api/reconcile` | Game master: check ownership, reservations, share supply and retained cash ledgers; returns `valid` and `issues` |
| `GET` | `/api/health` | Uptime, simulated time, tick/trade counters |

At start-up each symbol generates a year of daily bars in coarse mode and then
three days of 1 s ticks, so every interval has history before the first
request. `FEHU_TIME_SCALE=60` runs the market at 60 simulated seconds per
wall second; `FEHU_BIND`, `FEHU_HISTORY_DAYS`, `FEHU_WARMUP_HOURS`,
`FEHU_STARTING_CASH_CENTS`, `FEHU_TAPE`, `FEHU_FILL_LOG`, `FEHU_ORDER_LOG`
and `FEHU_LEDGER_LOG` are the other knobs. `FEHU_MARKET_HOURS=09:30-16:00`
gives the market a UTC weekday session (unset, it never closes);
`FEHU_PRICE_LIMIT_PCT` (0.10) and `FEHU_HALT_SECS` (300) set the limit move
that halts a symbol and how long the halt lasts. `FEHU_RATE_PER_SEC` (20) and
`FEHU_RATE_BURST` (40) set how fast one client may change things, and
`FEHU_STREAM_REPLAY` (1024) how many stream messages are kept for `?since=`.
`FEHU_STATE_FILE` keeps the market
across restarts (`FEHU_SAVE_SECS`, 30 by default, sets how often it is
written). `FEHU_ADMIN_KEY` locks the
game-master endpoints (`POST /api/game/events` and
`POST /api/symbols/{sym}/events`, which move prices) behind a key of your
choosing; unset, they stay open, which is what a single-player game on
localhost wants and a shared server does not. Same seeds and same events give the
same prices on every run; trading adds impact on top, so a market with no
orders replays the bare simulation.

A **user** is the player, an **account** holds their money, and a **trader**
is the market-facing identity that trades on one account (several traders may
share one). All money is an integer count of cents — never a float — and
every amount is checked: deposits and withdrawals must be positive, a
withdrawal cannot touch the cash a resting buy order has reserved, and an
order is validated against its account before it reaches the exchange (the
account must be active and its available balance must cover the worst-case
cost). Fills settle through the account, so every cent that moves is on its
ledger. New accounts start with cash, no shares, no margin and no shorting.
Persistence is optional, as described below.

The market can stop. Give it `FEHU_MARKET_HOURS` and orders outside the
session are refused with `409 market_closed`; leave it unset and it trades
around the clock, which is what a game whose players log in at all hours
wants. Separately, a symbol whose price moves more than `FEHU_PRICE_LIMIT_PCT`
from where its day opened is **halted**: no new orders (`409 symbol_halted`)
for `FEHU_HALT_SECS` of simulated time, after which it starts again with the
band measured afresh. The game master can halt and resume a symbol by hand,
and a manual halt has no end until they lift it. Resting orders are left
alone through all of this and can always be cancelled — a player must be able
to pull an order out of a market that has stopped. `GET
/api/symbols/{sym}/status` reports all of it, quotes carry `market_open` and
`halted`, and the stream sends a `status` message whenever it changes.

Set `FEHU_STATE_FILE` and the market survives a restart. The whole thing is
written there — every symbol's simulator, book, bars and held stops, and
every user, account, ledger, position, resting order and API key — every `FEHU_SAVE_SECS`
seconds and once more on a clean shutdown, and read back at start-up in place
of the warm-up, continuing from the simulated time it had reached. The write
goes through a temporary file and a rename, so an interrupted save cannot
destroy the last good one; a file from an unsupported format version, or one listing
different symbols, stops the server rather than starting a market without its
accounts. The reader also refuses inconsistent account balances, retained
ledgers, reservations, ownership links, identity counters and key ownership,
and malformed book indexes, price queues or quantities.
Without the variable nothing is kept and every start warms up a
fresh market.

Keys are the only credential: they are 128 bits of operating-system entropy,
issued once at sign-up. Authentication and saves retain domain-separated
SHA-256 hashes; a stored hash cannot be used as a bearer key. Save format 3
reads version-2 files by hashing their original keys, so players keep using
the same credentials. The next save writes only hashes; any older backups
still contain the original keys.

Three rules shape what the book will take. A **post-only** order (`"post_only":true`)
must rest: if its price would trade on arrival it is refused rather than
crossing, so a maker cannot become a taker by accident. A trader may not
**trade with itself**: an order that would reach one of that trader's own
resting orders is refused with `409 self_trade`, naming the orders in the way,
because a self-trade moves nothing but still prints on the tape and moves the
price. And an order can be **amended** (`PATCH`) to a new price or quantity —
implemented as a cancel and a fresh order under one lock, so the replacement
starts at the back of the queue for its price and the response says which
order was withdrawn and how much of it had already filled.

Orders are remembered. The book only knows an order while it rests, so every
submission is also written to an order log (`GET /api/orders/{id}`,
`GET /api/traders/{id}/orders`) that follows it through its fills to `filled`
or `cancelled`. Passing a `client_order_id` makes the submission idempotent:
the same order sent twice — a retry after a timeout — is placed once, the
first response is replayed with `200` instead of `201`, and re-using that id
for a *different* order is refused with `409` rather than quietly obeyed.

Shares are counted the same way. Every symbol has a fixed number of them
(`shares_outstanding`: 240 M of ACME, 85 M of NBLA, 610 M of HLIO, 150 M of
PXCO), and a buy can only be filled from the ones no trader holds or is
already bidding for — `GET /api/symbols/{sym}/shares` shows the split. A sell
is bounded from the other side: **a trader can only sell shares it holds**.
The quantity must fit in its position less whatever earlier resting sells
already promised away, so there is no shorting and no selling of shares a fill
has not yet delivered; a resting sell reserves the shares exactly as a resting
buy reserves cash, and a cancel gives them back. `GET /api/users/{id}/holdings`
adds a user's positions up per symbol — owned, reserved and sellable — across
every trader of theirs, and shares belong to the trader that bought them: one
trader cannot sell another's, even under the same user.

A **stop** is a line drawn on the price rather than an order: it rests
nowhere, holds no queue position and reserves nothing, and the book has never
heard of it. Arm one on the far side of the market — a buy above, a sell
below — and when the last price reaches it the engine sends the order it
becomes through the same checks as anything else, which is also where the
account is checked for the second time, because the money may have moved
since. A halted or closed symbol holds its triggers and fires them when
trading resumes, and the owner is told either way with a `stop_triggered`
message carrying the order it became or the reason it could not be placed.

One client cannot flood the market. Every request that *changes* something —
an order, an amendment, a cancel, a stop, money, an event — spends a token
from a bucket kept per API key, refilling at `FEHU_RATE_PER_SEC` with a burst
of `FEHU_RATE_BURST`; requests with no key share one bucket. Over the limit is
`429 rate_limited` with a `Retry-After`. Reading is never limited, and
`FEHU_RATE_PER_SEC=0` turns the whole thing off.

Every stream message is numbered. `GET /api/stream` opens with a `hello`
saying which sequence the connection joins at and how far back the server can
still reach; `GET /api/stream?since=N` replays everything published after `N`
that its buffer (`FEHU_STREAM_REPLAY`) still holds, before the live feed. A
client that falls behind is disconnected rather than handed later messages as
if nothing were missing, and reconnects with the sequence it got to. When the
buffer cannot reach that far back the `hello` says `gap: true`, which is when
— and only when — reloading the snapshots is the only recovery. The bundled
UI does all of this.

What is deliberately *not* built — fees, corporate actions, tick and lot
sizes — and the decisions behind what is, are listed in
[DESIGN.md §15](DESIGN.md#15-not-built-yet).

## License

MIT OR Apache-2.0.
