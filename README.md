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
  replacing it, and `webapp/src/market.rs` for how the server runs.
- **Tooling:** `just build | test | lint | bench | wasm | dump | serve`.
- **Sample app:** [`webapp/`](webapp) is an axum server around four seeded
  symbols — listed and delisted at runtime — with a TypeScript UI in
  [`webapp/ui/`](webapp/ui).

## Sample web app

[`webapp/`](webapp) is a small axum backend that shows the crate used the way
a game server would: four seeded symbols tick in wall-clock time — and the
game master lists and delists more of them as it goes — the game pushes
events over HTTP, traders send orders into each symbol's book, and a browser
UI draws the OHLC bars, the book, the tape and the player's account live.

```text
cargo run --release -p fehu-webapp     # then open http://localhost:3000
```

The server has no locks. Each symbol — its simulator, its book, its bars,
its tape — is a tokio task of its own, and so is the market that holds
everybody's money and every order ever sent; anything that wants to read or
change one sends it a job and, if it needs an answer, waits for the reply.
Calls go one way: the market calls the symbols, the symbols call nobody, so
there is no cycle to deadlock on. Every change to money or to a book is one
job on the market, which calls the symbol for the book operation and books
the money side before it runs anything else; the engine step is one such
job that fans the advance out to every symbol at once and joins them. Reads
of a quote, a book or the bars go straight to the symbol and wait for
nothing else. `webapp/src/market.rs` has the whole account.

[`docs/architecture.html`](docs/architecture.html) draws that: open it in a
browser and watch an order travel the actors — the middleware, the market's
mailbox, the book, the fills and the stream — one step at a time, alongside an
engine step, a cancel, a refusal and a read that never touches the market.

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
`POST /api/traders` — and is `null` in every response after. That response is
the only copy there will ever be: the server keeps a hash of it and nothing
more.

Every request that changes something may carry an **`Idempotency-Key`**
header, and should if it moves money: the same key over the same body is
applied once and answered twice. Responses carry `Fehu-Journal-Seq`, and a
replayed one also carries `Fehu-Idempotent-Replay`. See
[The journal](#the-journal).

| Method | Path | What |
|---|---|---|
| `GET` | `/api/symbols` | Quotes for every listed symbol |
| `POST` | `/api/symbols` | Game master: list a new symbol — `{"symbol":"WDGT","name":"Widget Corp","shares_outstanding":1000000,"start_price_cents":5000}`, optional `sector`, `description`, `drift`, `volatility`, `seed`, `history_days` |
| `GET` | `/api/symbols/{sym}` | Quote, latent snapshot, config and share count |
| `GET` | `/api/symbols/{sym}/shares` | The symbol's shares: outstanding, held by traders, bid for, still available, and who holds them |
| `GET` | `/api/symbols/{sym}/status` | Whether the symbol can be traded: session open, halted, the limit band and the next open/close |
| `POST` | `/api/symbols/{sym}/halt`, `/resume` | Game master: stop and start trading in one symbol |
| `POST` | `/api/symbols/{sym}/dividend` | Game master: `{"cents_per_share":50}` — pays every holder and takes the price ex |
| `POST` | `/api/symbols/{sym}/delist` | Game master: take the symbol away — cancels its resting orders and stops, buys every holder out at `{"cents_per_share":60}` (the last price if omitted, `0` for a company worth nothing) |
| `GET` | `/api/symbols/{sym}/bars?interval=M1\|M5\|H1\|D1&limit=500` | OHLCV bars, oldest first, in-progress bar last |
| `POST` | `/api/symbols/{sym}/events` | Raw simulator event: `{"type":"jump","pct":-0.1}`, `drift_shift`, `drift_for_total_move`, `vol_shift`, `fundamental_shift`, `fundamental_target`; optional `at_ms` / `delay_secs`, `source`, `note` |
| `POST` | `/api/game/events` | Semantic game event: `{"kind":"scandal","symbol":"ACME","magnitude":1.5}`; market-wide kinds (`market_crash`, `rate_hike`, …) need no symbol |
| `GET` | `/api/game/catalog` | Every game-event kind and the simulator events it expands to |
| `GET` | `/api/events` | Audit log of accepted events, newest first (`?symbol=`, `?limit=`) |
| `POST` | `/api/users` | Create a user: `{"name":"ada","email":"ada@example.com"}` (both optional). The response carries their `api_key`, once |
| `GET` | `/api/users`, `/api/users/{id}` | The caller themselves: accounts, traders, cash and shares owned |
| `GET` | `/api/users/{id}/holdings` | Shares the user owns per symbol, added up over their traders, with what is reserved and what is still sellable |
| `POST` | `/api/users/{id}/accounts` | Open another account: `{"name":"main","cash_cents":10000000}` — the cash is paid out of treasury, not created |
| `GET` | `/api/users/{id}/accounts`, `/api/accounts`, `/api/accounts/{id}` | Accounts: balance, reserved, available, status, and the `wallet_id` holding the money |
| `POST` | `/api/accounts/{id}/deposit` | Game master: **mint** money into an account, `{"amount_cents":250000,"memo":"week 1"}` |
| `POST` | `/api/accounts/{id}/withdraw` | Game master: **burn** money out of one; only the available balance can leave |
| `POST` | `/api/accounts/{id}/status` | `{"status":"active\|frozen\|closed"}` — freezing and unfreezing are the game master's, closing is the owner's and needs an empty account |
| `GET` | `/api/accounts/{id}/ledger?limit=100` | Every movement of money, newest first |
| `GET` | `/api/accounts/{id}/validate` | Status, what the account may do, and any broken invariant |
| `POST` | `/api/traders` | Create a trader, with a user and a funded account: `{"name":"alice","cash_cents":10000000}` (both optional), and hand over the new user's `api_key`; `user_id` and `account_id` join existing ones, which needs that user's key |
| `GET` | `/api/traders`, `/api/traders/{id}` | Traders; a portfolio with cash, positions marked to the reference price, open orders and fills |
| `POST` | `/api/traders/{id}/deposit` | Game master: mint into the trader's account, `{"amount_cents":250000}` |
| `POST` | `/api/traders/{id}/cancel_all` | Cancel every resting order of a trader |
| `POST` | `/api/symbols/{sym}/orders` | `{"trader_id":1,"side":"buy","qty":100,"type":"market"}` or `"type":"limit","price_cents":8400`, optional `"tif":"gtc\|ioc\|fok"`, `"client_order_id":"abc-1"`, `"post_only":true`, `"display_qty":20` to show only a slice of a resting order at a time, and `"expires_at_ms"` or `"day":true` to have the resting remainder withdrawn later; responds with fills and status |
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
| `GET` | `/api/supply` | How much currency exists and where it sits: minted, burned, outstanding, what the wallets actually hold, and whether the two agree |
| `GET` | `/api/commands/{key}` | The answer a command was given, by the `Idempotency-Key` it was sent under, for a client that lost the response |
| `GET` | `/api/reconcile` | Game master: check ownership, reservations, share supply, retained cash ledgers **and that the currency adds up**; returns `valid` and `issues` |
| `GET` | `/api/health` | Uptime, simulated time, tick/trade counters, orders placed and refused, fills booked and any that failed to settle, stream and rate-limit state, and how long requests and engine steps are taking |

At start-up each symbol generates a year of daily bars in coarse mode and then
three days of 1 s ticks, so every interval has history before the first
request. `FEHU_TIME_SCALE=60` runs the market at 60 simulated seconds per
wall second; `FEHU_BIND`, `FEHU_HISTORY_DAYS`, `FEHU_WARMUP_HOURS`,
`FEHU_STARTING_CASH_CENTS`, `FEHU_TAPE`, `FEHU_FILL_LOG`, `FEHU_ORDER_LOG`
and `FEHU_LEDGER_LOG` are the other knobs. `FEHU_GENESIS_CENTS` (10^14, i.e.
$1 trillion) is the world's whole opening supply, minted into treasury at
start-up; `FEHU_STARTING_CASH_CENTS` is paid to each new account *out of*
that, so a treasury that runs dry refuses to open more rather than printing
the difference. `FEHU_ISSUER_FLOAT_CENTS` (10^11) is what each symbol's
issuer wallet is given to pay dividends and buyouts from, and
`FEHU_SYNTHETIC_FLOAT_CENTS` (5×10^13) is what the stand-in for the
simulator's unfunded liquidity starts with — see **The currency** below. `FEHU_MARKET_HOURS=09:30-16:00`
gives the market a UTC weekday session (unset, it never closes);
`FEHU_PRICE_LIMIT_PCT` (0.10) and `FEHU_HALT_SECS` (300) set the limit move
that halts a symbol and how long the halt lasts. `FEHU_RATE_PER_SEC` (20) and
`FEHU_RATE_BURST` (40) set how fast one client may change things, and
`FEHU_STREAM_REPLAY` (1024) how many stream messages are kept for `?since=`.
`FEHU_MAX_SYMBOLS` (32) caps how many symbols may be listed at once — every
one of them is a simulator stepped on every engine tick.
`FEHU_TICK_CENTS` (1) and `FEHU_LOT` (1) make every symbol quote in a coarser
price step and trade in lots: the synthetic ladder and its prints obey them
too, and an order off the grid is refused by the book rather than by the
server. `FEHU_TAKER_FEE_BPS` (0) charges whoever takes liquidity, in basis
points of the fill, and `FEHU_MAKER_FEE_BPS` (0, negative) pays whoever
provided it; each fee is its own ledger entry beside the trade, and a buy has
to be able to afford the fee as well as the shares. A rebate is paid out of
what the venue has taken in fees and no further: what it has not collected,
it does not pay.
`FEHU_STATE_FILE` keeps the market
across restarts (`FEHU_SAVE_SECS`, 30 by default, sets how often the snapshot
is written; the command journal beside it is written before every change is
acknowledged, so the interval costs nothing that was promised).
`FEHU_COMMAND_LOG` (10 000) is how many `Idempotency-Key`s are remembered,
which is how late a retry may arrive and still be free. `FEHU_ADMIN_KEY` locks the
game-master endpoints behind a key of your choosing: the ones that move
prices (`POST /api/game/events`, `POST /api/symbols/{sym}/events`), the ones
that list, halt and delist symbols, and — since currency became conserved —
the only two that change how much of it there is, `POST
/api/accounts/{id}/deposit` and `.../withdraw`. Unset, they stay open, which
is what a single-player game on localhost wants and a shared server does
not. Same seeds and same events give the
same prices on every run; trading adds impact on top, so a market with no
orders replays the bare simulation.

A **user** is the player, an **account** holds their money, and a **trader**
is the market-facing identity that trades on one account (several traders may
share one). All money is an integer count of cents — never a float — and
every amount is checked: amounts must be positive, a withdrawal cannot touch
the cash a resting buy order has reserved, and an order is validated against
its account before it reaches the exchange (the account must be active and
its available balance must cover the worst-case cost). Fills settle through
the account, so every cent that moves is on its ledger. New accounts start
with cash, no shares, no margin and no shorting. Persistence is optional, as
described below.

### The currency

An account's money is a **wallet** in one world-wide ledger, and every
movement through it is a set of signed postings that **sums to zero**. So the
currency is conserved by construction rather than by care, and the audit is a
sum anyone can do:

```text
Σ balances (every wallet but issuance) = minted − burned
```

`GET /api/supply` reports both sides of it and whether they agree;
`GET /api/reconcile` fails if they do not. Only two operations move those
numbers — minting and burning — and both are the game master's. **No route a
player can reach changes the supply.** Opening an account with `cash_cents`
pays it out of treasury; a fee goes to a venue wallet instead of leaving the
world; a dividend or a delisting buyout is funded from that symbol's issuer
wallet, and one it cannot fund is refused with the shortfall rather than
paid to some holders and not others, or clipped at a balance cap.

Freezing an account is the game master's and withdraws its resting orders in
the same job, since an order that outlived a freeze would fill against a
wallet that could no longer pay for it. Closing one is the owner's, and needs
the wallet empty, so closing can never strand currency where nothing can
reach it again.

One wallet is allowed to owe: the stand-in for the liquidity the simulator
quotes, which nobody funds. Currency it hands a player is real currency, so
its debt is carried inside the sum above and reported as
`synthetic_debt_cents` rather than quietly minted. Funding both sides of
every fill is what retires it.

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
every user, account, wallet, position, resting order and API key digest, with
the currency ledger and the supply behind it — every `FEHU_SAVE_SECS`
seconds and once more on a clean shutdown, and read back at start-up in place
of the warm-up, continuing from the simulated time it had reached. The write
goes through a temporary file and a rename, so an interrupted save cannot
destroy the last good one; a file from an unsupported format version, or one
listing different symbols, stops the server rather than starting a market
without its accounts. Files written before the currency ledger are among
them: they carry a balance on each account and no supply behind it, so there
is no way to carry one forward without inventing where its money came from.
The reader also refuses inconsistent account balances, retained ledgers,
reservations, ownership links, identity counters and key ownership, malformed
book indexes, price queues or quantities, and a ledger whose wallets do not
add up to what has been minted less what has been burned.
Without the variable nothing is kept and every start warms up a
fresh market.

### The journal

A snapshot every thirty seconds would lose up to thirty seconds of trading,
which is not a thing to lose. So the snapshot is only a checkpoint: beside it
is an append-only **command journal**, and every change is written there and
flushed *before* the response goes out. Nothing this server has acknowledged
is lost, whatever stops it.

It journals the **commands**, not their effects, which the simulator's
determinism makes exact: the checkpoint plus the commands after it, applied
in order, is the state that was acknowledged. Each entry carries the request
with everything the server chose already resolved — the simulated instant it
runs at, the wall clock it arrived on, the seed of a listing — so a replay
reads no clock and rolls no dice. The engine tick is a command too, which is
what makes an order that filled against the price at 12:04:31 fill against it
again. On start-up the snapshot is loaded, the entries after it are applied,
and the journal is rewritten from the sequence the *next* snapshot includes.

Every mutation takes an optional **`Idempotency-Key`** header. The first
request under a key is applied and its response written down; the same key
with the same body is answered with that response instead of doing the work
again (`Fehu-Idempotent-Replay: true`), and the same key with a *different*
body is refused with `409 idempotency_conflict` rather than quietly obeyed.
Every response says where it landed in the journal, as `Fehu-Journal-Seq`,
and `GET /api/commands/{key}` gives back the answer a command was given, for
a client that lost it. `client_order_id` on an order is the same idea one
level down, and still works on its own.

If the journal cannot be written the server stops accepting changes —
`503 journal_unavailable` — and goes on serving reads. Memory would otherwise
be ahead of the disk, and a market that cannot promise to remember a change
should not accept one.

What this is not is a database: one writer, one file, no history to query and
no outbox to deliver from. That is deliberate — see `docs/economy-engine-plan.md`
for what a live service would need instead.

Keys are the only credential: they are 128 bits of operating-system entropy,
generated once at sign-up. The server keeps only a domain-separated SHA-256
digest of one — in memory, in the journal and in the save file alike — so
none of them is a list of live credentials and a stored digest cannot be used
as a bearer key. The consequence is worth knowing: the response that creates
a user is the only copy of its key that will ever exist. A *replayed* sign-up
comes back without one, and a client that loses that response has to create
another user.

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

What is still not built — stock splits, and a buyback that retires stock —
is written up in [`HANDOFF.md`](HANDOFF.md).

## License

MIT OR Apache-2.0.
