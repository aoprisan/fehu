# Missing features

A survey of what the repository does not do, taken at the head of `main` after
milestone 7 of [`economy-engine-plan.md`](economy-engine-plan.md). Three
sources: the plan's own "what is still not here" notes, the routes the plan's
API table and the server README name against the ones `api.rs` registers,
and a read of each crate's public surface against its module docs.

[`missing-features-analysis.md`](missing-features-analysis.md) weighs each
item here by what it costs to close against what closing it buys, and orders
the work.

Every item names where the evidence is. Items are ranked within each section
by how soon somebody running a game on this server would hit them. Nothing
here is a bug report: each is a thing the code, the docs or the plan says it
would like to have and does not.

## Economy server (`crates/fehu-economy`)

### Named by the plan or the docs, not built

- **NPC producers.** The plan's "NPC merchants and producers" section says a
  producer "runs recipes and sells the output". `src/npc.rs` implements
  merchants only: a wallet, an inventory and a quoting policy. Nothing on the
  world's side ever starts a job.
- **Takings never come home.** A purchase credits the symbol's issuer wallet
  (`market.rs`, `Reason::Purchase`) and a job's cost credits the venue
  (`Reason::JobCost`). No command moves either back to treasury or into a
  budget; budgets are funded from treasury alone. The loop
  [`world.html`](world.html) animates — the seam and the furnace sweep their
  takings to the treasury — does not close on the server.
- **Push delivery for the outbox.** The backend polls `GET /api/outbox`. The
  milestone-5 note calls pull "the wrong default across a WAN"; there are no
  webhooks.
- **The SSE credential travels in the query string.** `ui/src/stream.ts`
  appends `api_key=` to the URL because `EventSource` cannot set headers. The
  plan's header-or-stream-ticket design is "its own milestone".
- **Services share the anonymous rate-limit bucket.** `src/limit.rs` keys its
  buckets by user id, so a service credential falls into the shared bucket.
  The milestone-6 note calls giving a service its own bucket "a small change
  and the right one as soon as anything measures it".
- **No key rotation.** A service that loses its key is issued a new service,
  because the registry keeps only a digest. A player's key cannot be rotated
  or revoked at all: `DELETE …/admin/services/{id}` is the only revoke route.
- **Job catch-up after downtime.** Production pauses while the server is
  down; the plan defers the catch-up policy and nothing in `engine.rs` or
  `jobs.rs` implements one.
- **An importer for older worlds.** `src/save.rs` refuses any snapshot whose
  version is not the current one and describes the importer that would bring
  an old world forward — one labelled `Migration` transaction out of
  issuance — as "a day's work when there is such a world".
- **Quotas beyond the limiter.** No cap on one trader's resting orders, held
  stops or running jobs. The milestone-5 note: "nothing measured has asked
  for them yet".
- **v1 admin routes the plan's API table names.** `/api/v1/economy/admin/mint`,
  `/burn`, `/freeze`, `/unfreeze`, `/api/v1/economy/game-events` and
  `/api/v1/economy/markets/{symbol}/orders` have no v1 spelling in
  `api.rs`'s router. A backend that speaks only the documented economy
  surface cannot mint, freeze, push an event or trade.

### Gaps a game backend or an operator hits

- **Service scopes cannot read.** Only `purchase`, `consume` and
  `GET /commands/{key}` accept a `ServiceCaller`. A service holding the
  `inventory` scope cannot read the inventory it just changed, nor a wallet;
  the outbox, supply and overview take the operator key only. The plan's
  table says "owner or service" for wallets and inventory.
- **The outbox carries only what the stream announces.** `Market::announce`
  is the one path into it, so the facts are fills, expiries, status changes,
  listings, delistings, job completions, stop triggers and accepted events.
  A reward paid, a purchase, a transfer, a dividend, a mint or burn, a job
  started or cancelled leaves no fact. A backend cannot learn that a player
  was paid, or paid someone, without polling the wallet.
- **Reward rules are an amount and a budget.** `rewards.rs`'s `RewardRule`
  has no cooldown, per-player cap or expiry. The plan lists reward cadence
  and sizes as open choices to "implement with configuration"; only the size
  is configurable.
- **Jobs run one at a time.** `Command::StartJob` carries a trader and a
  recipe id, no quantity. Batching is N requests and N journal entries.
- **No way to remove an NPC, close or drain a budget, or change a listed
  symbol's configuration.** `journal::Command` has no variant for any of
  them; an NPC can only be deactivated, a budget only funded.
- **No market-wide halt.** `Halt` and `Resume` are per symbol.
- **Pagination is `limit` only.** The ledger, tape, order history, event log
  and bars take a `limit` and nothing else — no cursor, no `before`, no
  `after` — so anything past the limit is unreachable.
- **The operator has no user directory.** `GET /api/users` returns the caller
  alone, operator included. The overview's wallet directory is the only
  enumeration of who exists.
- **Day orders are refused without a calendar** (`trading.rs`,
  `OrderRequest::day`). Documented, but a client has no way to ask whether
  the server has a calendar before the refusal.

### Tooling and documentation drift

- **No CI configuration is committed.** `CLAUDE.md` and the READMEs say
  `just ci` is "everything CI runs"; there is no `.github/` directory and no
  Dockerfile.
- **Three environment variables are read and undocumented.** `FEHU_EVENT_LOG`,
  `FEHU_MAX_BARS` and `FEHU_NOW_MS` are parsed in `market.rs` and named in
  `main.rs`'s module doc, but not in the README's list of knobs.
- **Metrics are a field of `/api/health`.** `metrics.rs` keeps counters and
  there is no exposition endpoint in a scraper's format.
- **No offline tooling.** `main.rs` takes no arguments; there is no
  subcommand to inspect or verify a journal or snapshot, and the only restore
  path is pointing `FEHU_STATE_FILE` at a backup.

## Utilities library (`crates/fehu`)

- **Stop orders, post-only, self-trade prevention, order expiry and amend in
  place are absent from the book and the exchange.** `book.rs`'s `OrderKind`
  is market or limit; `TimeInForce` is `Gtc`, `Ioc`, `Fok`; `Exchange`
  offers submit and cancel. The server reimplements each on top
  (`symbol.rs`'s `check_post_only` and `self_crossing`, `market.rs`'s
  `sweep_expired` and cancel-then-resubmit amend), which every other
  consumer of the crate must repeat.
- **The event queue is write-only.** `Simulator::push_event` is the only
  mutator and `Snapshot::pending_events` is a count. A queued event cannot be
  listed, inspected or withdrawn.
- **Candles are fixed to four intervals.** `candles.rs`'s `Interval` is `M1`,
  `M5`, `H1`, `D1` and the storage is sized to it; 15m, 4h or weekly bars
  cannot be expressed. A bar carries no notional or VWAP. `Candles` ingests
  ticks only, so the coarse daily history the README advertises cannot be
  loaded into the aggregator that serves history — `symbol.rs` keeps a
  parallel `coarse_daily` vector and joins it at read time. Nothing closes
  the in-progress bar at session close; `examples/dump.rs` does it by hand.
- **The calendar is UTC-only with no holidays, half days or intraday
  breaks** (`time.rs`: "one session per trading weekday, no holidays"), and
  a weekend gap is weighted the same as an overnight gap.
- **Multi-currency is half-built.** `Ledger::open_in` accepts any
  `CurrencyId`, but issuance, supply, flows and transaction validation are
  single-currency, so a wallet in a second currency can never be funded. The
  module doc says the field exists "so that a second one is a change to this
  module"; that change has not been made.
- **Fee and rebate reasons exist with no fee mechanism.** `Reason::Fee` and
  `Reason::Rebate` are defined in `ledger.rs`; `TradingParams` has no fee
  schedule and `Exchange` computes none. The server's maker/taker fees are
  its own.
- **A version mismatch is a dead end.** `Simulator` and `Exchange` refuse any
  saved version other than the current one, while `book.rs` still carries
  pre-iceberg upgrade logic (`seed_priority`) that can therefore never run.
- **Serialisation gaps.** A standalone `OrderBook` loses its tick and lot
  rules on a round trip because `rules` is `serde(skip)`; `sim::Snapshot`
  and `ledger::Draft` are not serialisable while their siblings are;
  `Ledger` has no `PartialEq`, so `tests/ledger.rs` compares a hand-built
  tuple instead; the error types implement `Error` only under `std`, though
  the workspace is edition 2024 and `core::error::Error` is available.
- **Configuration is immutable after construction.** `Exchange::params` and
  `Simulator::config` have no setters, so a spread, an ADV or the
  `synthetic` switch cannot change mid-world without a serde round trip.
  `MAX_PRICE_CENTS` is named by `OrderError::BadPrice` but not re-exported
  from `lib.rs`.

## Browser UI (`crates/fehu-economy/ui`)

- **Stop orders are absent** despite `StopOrder` and `StopRequest` being
  fully typed in `types.ts`, and `stream.ts` drops the `stop_triggered` and
  `order_expired` message kinds, so a stop firing or a day order expiring
  produces no notification and no portfolio refresh.
- **Players cannot buy or consume goods.** No catalogue view, no purchase, no
  consume. The workshop panel shows inventory and recipes, but a fresh player
  has no way to source the inputs for a job.
- **No order amend, cancel-all, order history, fills blotter, ledger view,
  wallet transactions or transfer form.** `api.ts` already has
  `amendOrder`, `traderOrders`, `order`, `ledger`, `validateAccount` and
  `holdings` with no callers.
- **The operator's dashboard has readings and few levers.** It cannot list
  a symbol, pay a dividend, delist, create a merchant, pay a reward, edit a
  recipe, download a backup, issue or revoke a service key, list provisioned
  players, or open a wallet row's history. Funding a budget is a
  `window.prompt`.
- **No identity controls.** `actions.ts` silently creates a trader named
  `player` and keeps the key in `localStorage`. There is no field to paste an
  existing key, no way to see it, and no sign-out; a second account cannot be
  opened.
- **The ticket exposes three of eight order options.** `post_only`,
  `display_qty`, `expires_at_ms`, `day` and `client_order_id` are typed and
  unreachable from the form.
- **Errors are one string.** `api.ts` flattens every refusal to
  `code: message`; `rate_limited` and its `Retry-After`, `overloaded`,
  `revoked_api_key`, `post_only_would_cross` and `self_trade` all render the
  same, and the header is discarded.
- **No `Idempotency-Key` on any write.** A retried mint, deposit or transfer
  from the UI can apply twice.
- **Typed data nobody renders.** Symbol status (next open and close, band,
  halt reason), day OHLC and market cap on a quote, the book's reference
  price and pending flow, NPC policy, most health counters, and a delisting's
  totals. Only one hash route exists (`#economy`); there are no deep links,
  no theme control, and book depth and tape length are constants.
