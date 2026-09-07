# Where this branch got to, and what is left

Branch: `claude/finish-the-trading-layer`, on top of `main` at `c003ac0`.

The job was DESIGN.md §15 — "Not built yet" — worked top to bottom. Ten
passes landed, each its own commit with regression coverage and the design
document updated in the same breath. Three items are left, and none of them
is simply unwritten: each is blocked on a decision that wants making
deliberately rather than in passing. That is what this file is for.

## What landed

| Commit | §15 item | What it does |
|---|---|---|
| `b109624` | 15.4, 15.5 | Reconciliation endpoint, hashed API keys, validated saves, validated books, cross-symbol order-id collisions, stream-gap disconnect, halts that freeze the book |
| `fdcf980` | — | `AGENTS.md` |
| `d21249f` | 15.1 | Stop and stop-limit orders |
| `d82fdf8` | 15.2 | Sequence numbers on every stream message, `?since=` replay |
| `293543a` | 15.4 | Rate limits |
| `6a60b6e` | 15.4 | Metrics on `/api/health` |
| `147880e` | 15.1 | Tick and lot size |
| `a96b7e1` | 15.3 | Fees |
| `1237af1` | 15.1 | GTD and day orders |
| `12a2cd5` | 15.3 | Dividends |
| `e75ea92` | 15.1 | Icebergs, and queue priority split from the order id |

Verification, all green on every commit:

```
cargo test --all-features
cargo test --no-default-features --tests
cargo test -p fehu-webapp
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check
(cd webapp/ui && npm run build)      # types.ts and webapp/static rebuilt
```

Two things to know before picking this up:

- **The save format is now version 4.** Version 2 migrates by hashing its
  plaintext keys, version 3 by starting the stop store empty. `Resting`
  gained `hidden`, `display` and `seq` within version 4 as `serde(default)`
  fields; a book with `next_seq == 0` is seeded from its ids on load
  (`OrderBook::seed_priority`), so a file written by an earlier commit *on
  this branch* still loads.
- **Everything new is off by default.** `FEHU_TICK_CENTS`, `FEHU_LOT`,
  `FEHU_TAKER_FEE_BPS` and `FEHU_MAKER_FEE_BPS` are all 1/1/0/0, so the
  market behaves exactly as it did and the determinism tests pass bit for
  bit. Rate limiting is the one exception: it is on at 20/s with a burst of
  40. The test suites set `rate_per_sec: 0.0`, because a test sends requests
  as fast as the runtime will carry them and wall-clock timing has no place
  in an assertion.

## What is left

### 1. Splits, and buybacks that retire stock (§15.3)

A split is not hard to describe and is a lot of work to do honestly. It has
to rewrite, atomically:

- every position's quantity and average cost;
- every resting order's price and size — **and the reservation behind it**,
  because a buy iceberg or an ordinary resting buy reserves `price × qty` and
  reconciliation checks exactly that;
- the simulator's price *and* fundamental, or the price mean-reverts back to
  where it was before the split;
- every bar in `Candles`, the `coarse_daily` history and the tape, or the
  chart lies.

Three of those are inside the crate, and `Simulator`, `Candles` and
`OrderBook` have no way to rescale anything. Two details make it more than
plumbing:

- dividing prices makes book levels **collide** — two levels can round to one
  — and the merged queue has to stay ordered by `seq`, which is now the
  priority (see `e75ea92`);
- after dividing a resting buy's price and multiplying its size, the
  reservation no longer equals `price × qty`, so either the split releases the
  difference or reconciliation starts failing. Restricting to integer n-for-1
  splits and rounding the reservation down, releasing the remainder, is
  probably the answer, but it is a decision.

A buyback that retires stock is blocked on something else entirely:
`shares_outstanding` is `SymbolInfo`, which comes from the build and which
the save file deliberately does not carry. That is the same question as (2)
below, and it should be answered once for both.

Suggested order: give the crate `Simulator::rescale`, `Candles::rescale` and
`OrderBook::rescale` with their own tests first, then wire the web app's
`POST /api/symbols/{s}/split` on top. The web app half is the easy half.

### 2. Listing and delisting at runtime (§15.3)

`save::Symbol` is `&'static str` and `TICKERS` is a build-time array, and
both assumptions run through the whole web app — the ledger, fills, order
records, positions and reservations are all keyed on an interned static
ticker, and `serde` is told as much (`webapp/src/save.rs`, the `symbol`
modules). Making symbols dynamic means replacing that representation
everywhere, teaching the save file a symbol table, and answering what happens
to a delisted symbol's positions and to the money they were worth.

It is a refactor of a core representation rather than a feature, and it is
the thing to do *before* a retiring buyback, not after.

### 3. Splitting the market lock per symbol (§15.4)

I would think twice about this one. `Mutex<Market>` is not only a bottleneck;
it is what currently makes three things correct:

- **reconciliation** sees one consistent market-wide snapshot;
- **share supply** (`held + resting bids ≤ shares_outstanding`) is checked
  and acted on without anything moving in between;
- **order ids** are allocated above every book's counter under one lock,
  which is what fixed the cross-symbol collisions in `b109624`.

Splitting it per symbol, with accounts still shared, means writing down an
ordering discipline for those three and a lock hierarchy that cannot
deadlock. That is a design pass with a performance target attached, not a
refactor to do speculatively — a game server with four symbols and one
engine step a second is nowhere near needing it.

## Smaller things noticed along the way

- `cargo test --no-default-features` **including doctests** fails, and did
  before this branch: the README example the crate includes as its doc test
  uses `?` with `Box<dyn Error>`, and the error types only implement
  `std::error::Error` under the `std` feature. The `just test` recipe passes
  `--tests`, which skips doctests, so nothing is red in practice. Fixing it
  means rewriting the README example not to use `?`.
- A **positive maker fee** is refused on purpose (`webapp/src/trading.rs`,
  `Fees`). A taker's fee is checked with the order, in the same moment it
  fills; a maker's would have to be reserved when the order is accepted and
  released exactly across every partial fill and cancel, and the rounding in
  that is a piece of work of its own. A maker *rebate* is allowed, because it
  only ever credits.
- The **stop store holds untriggered stops only** (§14.13). A stop that fires
  lives on as its order record and its `stop_triggered` message, not as a
  stop with a terminal status. If players want a history of triggers that
  never became orders, that is a small bounded log and a listing endpoint.
- The **replay buffer is memory, not a log**. A restart begins the sequence
  at 1 again, so every `?since=` after one is a gap. Persisting it would mean
  putting the last sequence in the save file, which is easy; persisting the
  messages would not be.
