# fehu-economy

The economy server: an axum backend that runs [`fehu`](../fehu) the way a game
server would. Four seeded symbols tick in wall-clock time — and the game
master lists and delists more of them as it goes — the game pushes events
over HTTP, traders send orders into each symbol's book, and a browser UI
draws the OHLC bars, the book, the tape and the player's account live. On
top of that sits the economy proper: a currency whose supply is conserved
and auditable, goods that players produce and consume, jobs, rewards, and a
command journal so nothing acknowledged is lost across a restart.

```text
cargo run --release -p fehu-economy     # then open http://localhost:3000
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
nothing else. `src/market.rs` has the whole account.

[`docs/architecture.html`](../../docs/architecture.html) draws that: open it
in a browser and watch an order travel the actors — the middleware, the market's
mailbox, the book, the fills and the stream — one step at a time, alongside an
engine step, a cancel, a refusal and a read that never touches the market.

[`docs/demo.html`](../../docs/demo.html) goes the other way: instead of one
request through the map, a working miniature of this server runs in the page —
the same command journal, ledger, books, jobs and world modifiers, driven by
its own engine loop. Push it around with the buttons, then crash it and watch
a snapshot plus the entries after it replay through the same `apply` and come
back to an identical state hash.

The UI is TypeScript ([`ui/`](ui/README.md)) built with Vite. Its output is
committed to `static/` and embedded into the binary, so
the command above needs no Node toolchain; `just ui` rebuilds it after a
change and `just ui-dev` serves it with hot reload against a running backend.

**Economy** in the header — or `http://localhost:3000/#economy` — opens the
operator's dashboard over it: where the currency sits and what has been
moving it, every wallet and whose it is, the budgets, the merchants, the
jobs, the world's effects, and the server's own latency and admission
counters; and the levers beside them — mint and burn, freeze and unfreeze,
open and fund a budget, price a reward, start and stop a merchant, halt and
resume a symbol. It reads `/api/overview` and `/api/health` while it is open
and nothing at all while it is closed, and it keeps its own readings, since
the server counts but does not remember. The audit (`/api/reconcile`) is a
button rather than a poll: it takes a snapshot of the whole market. On a
server with `FEHU_ADMIN_KEY` set, paste that key into the field in its bar.

Market data — quotes, bars, the book, the tape, the event log — is open to
anyone. Everything that belongs to a player needs the API key they were
issued when they signed up, as `Authorization: Bearer <key>` (or
`X-Api-Key`), and a key only ever speaks for its own user: nobody else can
read that portfolio, cancel those orders or move that money. The key is shown
**once**, in the response that created the user — `POST /api/users` or
`POST /api/traders` — and is `null` in every response after. That response is
the only copy there will ever be: the server keeps a hash of it and nothing
more.

There are three kinds of caller. A **player** is a user key, as above. The
**operator** is `FEHU_ADMIN_KEY`, and may do anything. A **service** is the
game backend: a key issued by `POST /api/v1/economy/admin/services` that
carries a fixed set of *scopes* and nothing else —

| Scope | Opens |
|---|---|
| `provision` | `POST /api/v1/economy/players`, and the roster that reads it back |
| `reward` | `POST /api/v1/economy/rewards`, and `GET /api/budgets` to see what is left |
| `inventory` | `POST /api/v1/economy/purchases` and `.../consume`, and `GET .../players/{id}/inventory`, for any player |
| `events` | `POST /api/game/events` |
| any of `provision`, `reward`, `inventory` | `GET /api/wallets/{id}` and its `/transactions`: whatever moves money may read the wallet it moved it in |
| any scope at all | `GET /api/outbox` and `POST /api/outbox/ack`: a service is the game backend whatever it has been narrowed to, and the outbox is what that backend reads instead of the stream |

A scope opens the read side of its own writes — a backend that just changed
an inventory can see what it did without holding a second key — and nothing
else: `/api/overview` names every wallet in the world and stays the
operator's.

So a backend that pays quest rewards need not hold the key that can also mint
currency, freeze accounts and rewrite the catalogue. A scope **narrows a
credential; it does not narrow the operator** — every route above stays open
to the operator on exactly the terms it always was, so issuing a service
takes nothing away from a world that never issues one, and a server with no
services behaves as it did before there were any. Issuing and revoking are
the operator's alone and reachable through no scope: a credential that could
grant one could grant itself a wider one. A service key is judged as that
service wherever it is presented, so a scope it does not carry is refused
with `missing_scope` even on a server that has no `FEHU_ADMIN_KEY` set.
Raw simulator events (`POST /api/symbols/{sym}/events`) are deliberately not
a scope: they are a lever on the price process rather than a fact about the
game.

Every request that changes something may carry an **`Idempotency-Key`**
header, and should if it moves money: the same key over the same body is
applied once and answered twice. Responses carry `Fehu-Journal-Seq`, and a
replayed one also carries `Fehu-Idempotent-Replay`. See
[The journal](#the-journal).

| Method | Path | What |
|---|---|---|
| `GET` | `/api/symbols` | Quotes for every listed symbol |
| `POST` | `/api/symbols` | Game master: list a new symbol — `{"symbol":"WDGT","name":"Widget Corp","shares_outstanding":1000000,"start_price_cents":5000}`, optional `sector`, `description`, `drift`, `volatility`, `seed`, `history_days`. `{"kind":"good","unit":"kg"}` lists a good instead: no float, no ladder, no dividend |
| `GET` | `/api/symbols/{sym}` | Quote, latent snapshot, config and share count |
| `GET` | `/api/symbols/{sym}/shares` | The symbol's shares: outstanding, held by traders, bid for, still available, and who holds them |
| `GET` | `/api/symbols/{sym}/status` | Whether the symbol can be traded: session open, halted, the limit band and the next open/close |
| `POST` | `/api/symbols/{sym}/halt`, `/resume` | Game master: stop and start trading in one symbol |
| `POST` | `/api/symbols/{sym}/dividend` | Game master: `{"cents_per_share":50}` — pays every holder and takes the price ex |
| `POST` | `/api/symbols/{sym}/delist` | Game master: take the symbol away — cancels its resting orders and stops, buys every holder out at `{"cents_per_share":60}` (the last price if omitted, `0` for a company worth nothing) |
| `GET` | `/api/symbols/{sym}/bars?interval=M1\|M5\|H1\|D1&limit=500` | OHLCV bars, oldest first, in-progress bar last; `&before=<open_ts>` reads the page before the oldest bar you have |
| `POST` | `/api/symbols/{sym}/events` | Raw simulator event: `{"type":"jump","pct":-0.1}`, `drift_shift`, `drift_for_total_move`, `vol_shift`, `fundamental_shift`, `fundamental_target`; optional `at_ms` / `delay_secs`, `source`, `note` |
| `POST` | `/api/game/events` | Semantic game event: `{"kind":"scandal","symbol":"ACME","magnitude":1.5}`; market-wide kinds (`market_crash`, `rate_hike`, …) need no symbol |
| `GET` | `/api/game/catalog` | Every game-event kind and the simulator events it expands to |
| `GET` | `/api/events` | Audit log of accepted events, newest first (`?symbol=`, `?limit=`, `?before=<event id>` for the page before) |
| `POST` | `/api/users` | Create a user: `{"name":"ada","email":"ada@example.com"}` (both optional). The response carries their `api_key`, once |
| `GET` | `/api/users`, `/api/users/{id}` | The caller themselves: accounts, traders, cash and shares owned. With the operator's key, everyone — the directory of who exists. Literally the key: a server with no `FEHU_ADMIN_KEY` has nobody to show the directory to |
| `GET` | `/api/users/{id}/holdings` | Shares the user owns per symbol, added up over their traders, with what is reserved and what is still sellable |
| `POST` | `/api/users/{id}/accounts` | Open another account: `{"name":"main","cash_cents":10000000}` — the cash is paid out of treasury, not created |
| `GET` | `/api/users/{id}/accounts`, `/api/accounts`, `/api/accounts/{id}` | Accounts: balance, reserved, available, status, and the `wallet_id` holding the money |
| `POST` | `/api/accounts/{id}/deposit` | Game master: **mint** money into an account, `{"amount_cents":250000,"memo":"week 1"}` |
| `POST` | `/api/accounts/{id}/withdraw` | Game master: **burn** money out of one; only the available balance can leave |
| `POST` | `/api/accounts/{id}/status` | `{"status":"active\|frozen\|closed"}` — freezing and unfreezing are the game master's, closing is the owner's and needs an empty account |
| `GET` | `/api/accounts/{id}/ledger?limit=100` | Every movement of money, newest first; `&before=<entry id>` for the page before |
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
| `GET` | `/api/traders/{id}/orders?status=resting\|filled\|cancelled&limit=100` | A trader's orders, newest first; `&before=<order id>` for the page before |
| `POST` | `/api/symbols/{sym}/stops` | Arm a stop: `{"trader_id":1,"side":"sell","qty":100,"stop_price_cents":8000}`, plus `"limit_price_cents"` for a stop-limit. The trigger must be on the far side of the market |
| `GET` | `/api/symbols/{sym}/stops?trader_id=`, `/api/traders/{id}/stops` | A trader's held stops, on one symbol or all of them |
| `DELETE` | `/api/symbols/{sym}/stops/{id}?trader_id=` | Withdraw a stop before it fires |
| `GET` | `/api/symbols/{sym}/book?depth=10` | Aggregated bids and asks, reference price, pending trader flow |
| `GET` | `/api/symbols/{sym}/trades?limit=50` | The tape, newest first; `&before=<ts_ms>` for the prints before that instant |
| `GET` | `/api/stream` | Server-sent events: `hello`, then every `tick` (with best bid/ask, top of book and the step's prints), accepted `event`, and — for `?api_key=`, since `EventSource` cannot set headers — that player's `fill`s |
| `GET` | `/api/npcs` | The traders the world runs itself: what each quotes, and what it has left |
| `POST` | `/api/npcs` | Game master: put a funded merchant in a symbol — `{"symbol":"ORE","cash_cents":5000000,"inventory":800}`, optional `name`, `size`, `levels`, `half_spread_bps`, `level_step_bps`, `requote_bps` |
| `POST` | `/api/npcs/{trader_id}/active` | Game master: `{"active":false}` — stop it quoting. It keeps its money and its stock |
| `GET` | `/api/catalog` | What the world will make and what it charges: one line per good |
| `POST` | `/api/catalog` | Game master: write or replace a line — `{"symbol":"ORE","price_cents":250,"available":500}`, `available` omitted for a seam that never runs out |
| `DELETE` | `/api/catalog/{sym}` | Game master: stop making a good. What was made stays made |
| `POST` | `/api/traders/{id}/purchases` | `{"symbol":"ORE","qty":100}` — pay the catalogue price and receive units that did not exist |
| `POST` | `/api/traders/{id}/consume` | `{"symbol":"ORE","qty":25}` — use units up. They leave the world and no currency moves |
| `GET` | `/api/traders/{id}/inventory` | The trader's units of the world's goods, with what is reserved |
| `GET` | `/api/recipes` | What the world knows how to make, and what making it takes |
| `POST` | `/api/recipes` | Game master: write or replace a recipe — `{"id":"smelt","inputs":[{"symbol":"ORE","qty":2}],"outputs":[{"symbol":"INGOT","qty":1}],"cost_cents":500,"duration_secs":300}`, optional `refund_bps`, `note`. Every line has to be a listed good |
| `DELETE` | `/api/recipes/{id}` | Game master: stop making a thing. Jobs already running still deliver |
| `POST` | `/api/jobs` | Run one: `{"trader_id":1,"recipe":"smelt"}` — the inputs and the cost go now, the outputs arrive at `due_at_ms`. `"runs":5` runs it five times as one job: inputs, cost and outputs all scale, and it is refused whole if any part cannot be afforded |
| `GET` | `/api/jobs`, `/api/jobs/{id}` | The caller's jobs, or one of them: what it took, what it will deliver and when |
| `POST` | `/api/jobs/{id}/cancel` | Stop a job before it is due. What comes back is `refund_bps` of the cost, nothing by default |
| `GET` | `/api/budgets` | Game master: the pools rewards are paid from, and the rules that price them |
| `POST` | `/api/budgets` | Game master: open one, funded out of treasury — `{"name":"quests","cash_cents":5000000}` |
| `POST` | `/api/budgets/{wallet_id}/fund` | Game master: top one up, `{"amount_cents":100000}` |
| `POST` | `/api/wallets/{wallet_id}/sweep` | Game master: bring an issuer's or the venue's takings home to treasury — everything available, or `{"amount_cents":100000}` of it |
| `POST` | `/api/rewards/rules` | Game master: what a named reward is worth — `{"id":"daily","budget":12,"amount_cents":5000}` |
| `DELETE` | `/api/rewards/rules/{id}` | Game master: take a rule away. What it has paid stays paid |
| `POST` | `/api/rewards` | Game master: pay for something that happened — `{"rule":"daily","trader_id":1,"source":"quest:42"}`. A `source` already paid gets its first receipt back and moves nothing |
| `POST` | `/api/transfers` | `{"from_account_id":1,"to_account_id":2,"amount_cents":25000}` — players paying each other; the sender's owner, or the game master |
| `GET` | `/api/wallets/{id}`, `/api/wallets/{id}/transactions` | One wallet: its kind, balance, reservation and history. The owner's, or the game master's |
| `GET` | `/api/world?at_ms=` | What game events are doing to production and demand, per symbol, now or at an instant |
| `GET` | `/api/supply` | How much currency exists and where it sits: minted, burned, outstanding, what the wallets actually hold, and whether the two agree |
| `GET` | `/api/overview` | Game master: the whole economy in one consistent read — the supply, what every reason has *moved*, every wallet and whose it is, the budgets and their rules, the merchants, the world's effects, the jobs, who is in it and where the outbox has been read to. One market job, so it is one instant, not eight |
| `GET` | `/api/commands/{key}` | The answer a command was given, by the `Idempotency-Key` it was sent under, for a client that lost the response |
| `GET` | `/api/backup` | Game master: a snapshot of the whole market as the response body — `curl … > backup.json`, and restore by pointing `FEHU_STATE_FILE` at it |
| `GET` | `/api/outbox?after=&limit=` | Game master: the facts nobody asked for — fills, jobs coming due, expiries, delistings, accepted events — numbered, retained and replayable from a cursor. Reading does not consume |
| `POST` | `/api/outbox/ack` | Game master: `{"through":128}` — how far the game backend has read. Everything after it comes back on the next read with no `after` |
| `GET` | `/api/reconcile` | Game master: check ownership, reservations, share supply, retained cash ledgers **and that the currency adds up**; returns `valid` and `issues` |
| `POST` | `/api/v1/economy/admin/services` | Operator: issue a credential for the game backend — `{"name":"quests","scopes":["reward"]}`. The key is in that one response and nowhere else |
| `GET` | `/api/v1/economy/admin/services` | Operator: every service credential, revoked ones included. Digests are not in the answer |
| `DELETE` | `/api/v1/economy/admin/services/{id}` | Operator: take a service's key away. What it did stays done |
| `POST` | `/api/v1/economy/players` | `service:provision`: map the game's own id for a player onto a user, an account and a trader — `{"external_id":"steam:42","name":"Ada"}`. Idempotent on `external_id`, so it is safe on every login; the account opens empty |
| `GET` | `/api/v1/economy/players?external_id=` | `service:provision`: who has been provisioned. A player nobody has is an empty list, not a 404 |
| — | `/api/v1/economy/…` | The rest of the economy surface under the paths `docs/economy-engine-plan.md` names: `players/{id}/inventory`, `wallets/{id}`, `transfers`, `rewards`, `purchases`, `consume`, `jobs`, `recipes`, `catalog`, `budgets`, `supply`, `world`, `outbox`, `outbox/ack`, `commands/{key}`, `reconcile`, and `admin/{recipes,catalog,budgets,rewards,backup}`. The same handlers as above, under a second spelling |
| `GET` | `/api/health` | Uptime, simulated time, tick/trade counters, orders placed and refused, fills booked and any that failed to settle, stream and rate-limit state, and how long requests and engine steps are taking |

At start-up each symbol generates a year of daily bars in coarse mode and then
three days of 1 s ticks, so every interval has history before the first
request. `FEHU_TIME_SCALE=60` runs the market at 60 simulated seconds per
wall second; `FEHU_BIND`, `FEHU_HISTORY_DAYS`, `FEHU_WARMUP_HOURS`,
`FEHU_STARTING_CASH_CENTS`, `FEHU_TAPE`, `FEHU_FILL_LOG`, `FEHU_ORDER_LOG`
and `FEHU_LEDGER_LOG` are the other knobs. `FEHU_NOW_MS` pins the simulated
instant the world starts at, in Unix milliseconds, instead of reading the
wall clock — which is how a test, or a demo that should look the same every
time, gets the same history from the same seeds. `FEHU_MAX_BARS` (5 000) is
how many completed bars each interval keeps per symbol and `FEHU_EVENT_LOG`
(500) how many accepted events the audit log at `/api/events` retains.
`FEHU_GENESIS_CENTS` (10^14, i.e.
$1 trillion) is the world's whole opening supply, minted into treasury at
start-up; `FEHU_STARTING_CASH_CENTS` is paid to each new account *out of*
that, so a treasury that runs dry refuses to open more rather than printing
the difference. `FEHU_ISSUER_FLOAT_CENTS` (10^11) is what each symbol's
issuer wallet is given to pay dividends and buyouts from, and
`FEHU_SYNTHETIC_FLOAT_CENTS` (5×10^13) is what the stand-in for the
simulator's unfunded liquidity starts with — see **The currency** below —
and `FEHU_SYNTHETIC=0` switches that liquidity off altogether, leaving the
book to whoever funded what is in it. `FEHU_SEED_MERCHANTS_CENTS` is the
other half of that switch: set it and a world that is being *warmed up* gets
a funded merchant behind every seeded symbol, each with that much currency
out of treasury and about as much stock as it would buy, so
`FEHU_SEED_MERCHANTS_CENTS=10000000 FEHU_SYNTHETIC=0` is a demo in which
every fill has somebody on the other side who paid for what they are
selling. A restored world is left alone: it already has the merchants it
had. `FEHU_MARKET_HOURS=09:30-16:00`
gives the market a UTC weekday session (unset, it never closes);
`FEHU_PRICE_LIMIT_PCT` (0.10) and `FEHU_HALT_SECS` (300) set the limit move
that halts a symbol and how long the halt lasts. `FEHU_RATE_PER_SEC` (20) and
`FEHU_RATE_BURST` (40) set how fast one client may change things;
`FEHU_MAX_INFLIGHT` (128) and `FEHU_MAX_STREAMS` (256) set how many changes
and how many stream connections the server will have going at once, past
which it answers `503 overloaded` at once instead of queueing (`0` for
either takes everything); and
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
`FEHU_RESUME` (`pause`) says what a restart does with the time the server
was away: `pause` carries on from the instant the world had reached, so a
job due in ten minutes is still due in ten minutes of market time;
`catch_up` lets the downtime elapse in the world instead — the clock resumes
ahead by the wall time missed, scaled by `FEHU_TIME_SCALE`, and the first
engine step advances every symbol through the gap and delivers every job
that fell due in it.
`FEHU_COMMAND_LOG` (10 000) is how many `Idempotency-Key`s are remembered,
which is how late a retry may arrive and still be free. `FEHU_JOB_LOG`
(2 000) is how many *finished* jobs are kept with what each delivered — a
running one is never dropped, being a promise the world has taken payment for
— and `FEHU_REWARD_LOG` (10 000) how many game event ids are remembered with
the reward each one paid, which is how late a duplicate quest result may
arrive and still be caught. `FEHU_OUTBOX` (10 000) is how many facts are kept
for the game backend to collect, which is how long it may be away before
what it missed is gone for good; `0` switches the outbox off. `FEHU_ADMIN_KEY` locks the
game-master endpoints behind a key of your choosing: the ones that move
prices (`POST /api/game/events`, `POST /api/symbols/{sym}/events`), the ones
that list, halt and delist symbols, and — since currency became conserved —
the only two that change how much of it there is, `POST
/api/accounts/{id}/deposit` and `.../withdraw`. Unset, they stay open, which
is what a single-player game on localhost wants and a shared server does
not. A shared server that does not want its game backend holding *that* key
issues it a scoped service credential instead; see above. Same seeds and same events give the
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

## The currency

An account's money is a **wallet** in one world-wide ledger, and every
movement through it is a set of signed postings that **sums to zero**. So the
currency is conserved by construction rather than by care, and the audit is a
sum anyone can do:

```text
Σ balances (every wallet but issuance) = minted − burned
```

`GET /api/supply` reports both sides of it and whether they agree;
`GET /api/reconcile` fails if they do not. Balances say where the currency
*is*; the ledger's flow meter says how it got there — a count and a total of
cents for every reason there is, from `genesis` and `mint` through `reward`,
`fee`, `buy` and `job_cost` — and `GET /api/overview` reports it. They are
running totals with no window behind them, exactly like the server's own
metrics: two readings and the time between them are all a rate is made of,
and anything that wants history should keep it. Only two operations move those
numbers — minting and burning — and both are the game master's. **No route a
player can reach changes the supply.** Opening an account with `cash_cents`
pays it out of treasury; a fee goes to a venue wallet instead of leaving the
world; a dividend or a delisting buyout is funded from that symbol's issuer
wallet, and one it cannot fund is refused with the shortfall rather than
paid to some holders and not others, or clipped at a balance cap.

Takings come home the same way. A purchase from the catalogue credits the
good's issuer and a fee credits the venue, and `POST
/api/wallets/{id}/sweep` moves what either has collected back to treasury,
where a budget can be funded from it — so the loop closes without minting:
what players spend on ore pays the next quest reward. It is a transfer like
any other, with a memo naming the wallet it came from, and it refuses any
wallet that is not an issuer's or the venue's: a player's or an NPC's is
somebody's money, and a budget's is set aside on purpose. An issuer swept
bare will refuse its next dividend rather than clip it.

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

## The journal

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

What this is not is a database: one writer, one file, and no history to query
beyond the retained ledger views. That is deliberate — see
`docs/economy-engine-plan.md` for what a live service would need instead.

## How much it will take

Two bounds, answering different questions. `FEHU_RATE_PER_SEC` is *per
client*: how fast one of them may change the market, so that none can crowd
the rest out — over it, `429 rate_limited` with a `Retry-After`. Reads are
never rate limited; a reverse proxy is the right place for that.

`FEHU_MAX_INFLIGHT` is *global*: how many changes may be inside the server at
once. Every mutation is one job on the market actor and its mailbox is
unbounded, so without a bound the answer to a burst is a queue that grows
until memory runs out, with every client in it waiting longer for a reply
that will arrive too late to use. Past the bound the server says `503
overloaded` immediately, with `Retry-After: 1`, which a client can act on in
a way it could not while waiting. `FEHU_MAX_STREAMS` is the same for open SSE
connections, each of which holds a receiver and a task for as long as it
lasts. Reads are not gated by either: `/api/health` is exactly what is worth
reading when the server is full. `GET /api/health` reports
`requests_in_flight` against `max_in_flight`, and `metrics.requests_shed`
counts what has been turned away for room — separately from
`metrics.requests_limited`, because a full server is not a fast client.

`tests/load.rs` declares a load and a target and holds the server to
them: 32 players sending 16 mutations each, all answered, none shed, the
whole burst through in seconds and the tail within twice the median — and
then a restart from the snapshot and the journal that comes back, reconciles
and still has the same facts in its outbox.

## Backup and restore

A snapshot is complete on its own — the journal beside a live state file only
covers the gap since the server's own last one — so a backup is a snapshot
taken out of band:

```sh
curl -sS -H "Authorization: Bearer $FEHU_ADMIN_KEY" \
  http://localhost:3000/api/backup > backup.json
FEHU_STATE_FILE=backup.json cargo run --release -p fehu-economy
```

It is taken from one consistent market job, like `/api/reconcile`, so it is a
real instant and not a smear across one. It truncates nothing: the live
journal still carries everything since the running server's own last
snapshot, because the live state file still needs it. And it is handed back
rather than written to a path the request names — an operator route that
wrote wherever its body said would be an arbitrary file write with a key on
it, and `curl >` is the same drill without one. Restoring a backup that
predates this build's `STATE_VERSION` is refused rather than guessed at, as
any state file is.

## The outbox

A stream is the wrong shape for the service that owns the rest of the game.
`/api/stream` is best-effort, its `?since=` buffer is small and in memory,
and a backend that was restarting when a job came due has no way to find out
that it did. So the same facts go a second way: everything that **moves
currency or units into or out of a player's hands** — a fill, a purchase, a
consumption, a transfer, a reward, a mint, a burn, a dividend, a job started,
cancelled or delivered — and everything the market does on its own — an
order the venue withdrew, a stop that fired, a halt, a listing, a delisting,
an accepted game event — is also appended to a durable **outbox**, numbered
from 1, and handed out against a cursor. Each entry names the journal
sequence of the command that caused it, the same number every committed
response carries in `Fehu-Journal-Seq`, so a backend that would rather not
be told about its own rewards twice matches the two.

Delivery is at-least-once. `GET /api/outbox` returns the facts after a
cursor; reading does not consume them, so a backend that dies between reading
and acting reads them again rather than never. `POST /api/outbox/ack`
`{"through":128}` says how far it got, and is a journaled command like
everything else — a cursor that moved only in memory would fall back to the
snapshot's value on a restart. Both are the operator's or any service's, and
no player's: the log is the whole world's.

What is *not* in it is the world's own bookkeeping — a budget opened or
funded, takings swept, a rule or a recipe rewritten — which the operator did
and knows, and ticks: they are market data, the highest-volume thing the
server produces, and `/api/symbols/{sym}/bars` has them whenever they are
wanted. A lost response to any command is still recovered by its
`Idempotency-Key` through `GET /api/commands/{key}`.

The log is bounded by `FEHU_OUTBOX`, and it is honest about the bound: a fact
evicted before it was acknowledged is counted in `dropped`, and a read that
starts further back than the log reaches comes back with `gap: true` so the
consumer resynchronises from the snapshot endpoints instead of believing its
own state. It is saved with the snapshot and rebuilt by replay, so a restart
returns the same facts under the same numbers and a cursor from before it
still means what it meant.

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

Shares are counted the same way. Every stock has a fixed number of them
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

Not every listing is a company. A symbol has an **asset kind** — `stock` or
`good` — and every quote says which (`asset_kind`, and `unit` for a good).
A good is the same thing everywhere it matters: the same book, the same
positions, the same reservations, the same fills. It differs in three
places. Its units are *issued and consumed* rather than floated, so
`shares_outstanding` is what has been issued less what has been consumed and
it starts at zero. It is quoted **without synthetic liquidity** — no maker
ladder, no printed volume — because a fill against liquidity nobody funded
would be a unit nobody issued; the simulator still runs underneath it as a
reference price, but the book holds only real orders and the tape only real
trades. And it has neither a dividend nor a buyout, so both routes refuse
it: what ends a good's life is consuming it.

That difference is what the audit checks. For a stock, holdings plus resting
bids may not exceed the float — the bids count because the ladder would sell
what nobody holds. For a good there is no ladder, so a bid speaks for
nothing, and the question is stricter: every issued unit that has not been
consumed is held by somebody.

Units come from the **catalogue** and go when they are consumed. A game
master writes a line — `POST /api/catalog` with a ticker and a price, and
optionally how many units the line may still make — and a player who pays
that price gets units that did not exist before
(`POST /api/traders/{id}/purchases`). The currency is not created, only
moved: it is debited from the buyer and credited to the good's issuer wallet
as one balanced transaction, so `GET /api/supply` reads exactly as it did.
`POST /api/traders/{id}/consume` is the other end. Units the trader holds
free of reservations are destroyed, what they cost is realised as a loss, and
no currency moves at all — a thing that has been used up is not a thing that
has been sold. Both are journaled commands, so a retry under the same
`Idempotency-Key` is answered, not re-made, and both survive a restart.

The catalogue is the world selling to a player. An **NPC** is a merchant
selling to one. `POST /api/npcs` funds a trader out of treasury, hands it
inventory — shares of a stock nobody held, or units of a good issued to it —
and gives it a quoting policy: a half-spread and a few levels either side of
the reference price, redrawn when the market moves past a band or when
something it was offering has been taken.

Nothing about an NPC is special, and that is the point. It has a wallet, a
position and reservations; its orders go through the same command path as a
player's, and a fill against it settles, charges fees and reconciles like any
other. It quotes only what it can fund and what it holds, so its bid
disappears when its till is empty and its ask when its stock is — scarcity
shows up in the book rather than as a rule written down somewhere. Its
decisions are taken inside the engine step, which is a journaled command, and
read nothing but the market and the symbol, so a replayed step redraws the
same book. Nobody can sign in as one: an NPC's user is created without a
credential, so there is none to leak.

That is what makes `FEHU_SYNTHETIC=0` worth having. With the ladder off,
every fill has a funded counterparty on both sides, and the wallet that
stands in for liquidity nobody paid for — `synthetic_debt_cents` in
`GET /api/supply` — never has to stand in for anything. The simulator carries
on underneath as the reference price the merchants read, so game events still
move the market, but through somebody's decision rather than through a print
nobody paid for. The cost is that a world with no merchants and no players
has an empty book, which is why the ladder is still on by default.

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
from a bucket kept per API key — a player's or a service's, so the game
backend neither spends nor is starved by anyone else's — refilling at
`FEHU_RATE_PER_SEC` with a burst of `FEHU_RATE_BURST`; requests with no key
share one bucket. Over the limit is
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
is written up in [`HANDOFF.md`](../../HANDOFF.md).

## License

MIT OR Apache-2.0.
