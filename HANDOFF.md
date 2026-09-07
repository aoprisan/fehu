# Where this branch got to, and what is left

Branch: `claude/remaining-work-2eywl4`, on top of `main` at `ec824d5`.

The job was DESIGN.md §15 — "Not built yet". Ten passes landed on the
previous branch and are summarised below; this one adds runtime listing and
delisting, which was the second of the three items that branch left behind.
Two are left, and the shape of both has changed: one is now smaller than it
was, and the other is still the one I would not do.

## What landed on this branch

| Commit | §15 item | What it does |
|---|---|---|
| this one | 15.3 | Listing and delisting symbols while the server runs |

`webapp/src/symbols.rs` is the new piece. The obstacle was never the
endpoints: it was that `save::Symbol` is `&'static str`, which the ledger,
fills, order records, positions, reservations, stops and the event log are all
keyed on, and the only `&'static str`s in a program are the ones its author
wrote. Rather than replace that representation everywhere — a refactor of a
core type with a lot of ways to be subtly wrong — the module supplies the half
that was missing: a process-wide registry that validates a ticker, upper-cases
it and leaks it, so every later mention of it resolves to the same pointer.
The representation is untouched, the JSON contract is untouched, and the
symbol set is no longer the build's.

The rest follows from that:

- `SymbolInfo` is now the listing's own — owned strings, `Serialize` and
  `Deserialize` — and travels in the save file. **Save format 5.** A version-4
  file migrates by taking its symbols' metadata from the build, which is where
  it lived; a version-4 file naming a symbol this build does not seed cannot
  be migrated and is refused. From 5 on, the file is the symbol table and a
  restored market lists what was saved.
- `POST /api/symbols` lists, `POST /api/symbols/{s}/delist` delists, both
  game-master endpoints, both recorded in the event log
  (`corporate:listing`, `corporate:delisting`) and published to the stream
  (`listed`, `delisted`).
- Delisting cancels the resting orders (releasing exactly what they reserved),
  drops the untriggered stops, and buys every holder out at `cents_per_share`
  — the last price by default, `0` for a company that turned out to be worth
  nothing — as its own `LedgerKind::Delisting` entry. The history stays and
  still names the ticker; the position goes, because nothing can mark it.
- `Market::order_id_floor`, saved with the market. Order ids are allocated
  above every book's counter, which was the whole story only while every book
  ever created was still there. A delisted book leaves with its counter and
  its orders stay in the log, so the floor keeps an id from coming round
  twice.

Two knobs: `FEHU_MAX_SYMBOLS` (32) caps the listings, and the ticker registry
is capped at 1024 names of at most 8 characters — nothing there is ever freed,
by design, so it is bounded on purpose.

Verification, all green:

```
cargo test --all-features
cargo test --no-default-features --tests
cargo test -p fehu-webapp
cargo clippy --all-targets --all-features -- -D warnings
cargo clippy -p fehu-webapp --all-targets -- -D warnings
cargo fmt --all --check
(cd webapp/ui && npm ci && npm run build)   # types.ts and webapp/static rebuilt
```

## What landed before it

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

Everything those added is off by default (`FEHU_TICK_CENTS`, `FEHU_LOT`,
`FEHU_TAKER_FEE_BPS`, `FEHU_MAKER_FEE_BPS` are 1/1/0/0), except rate limiting,
which is on at 20/s with a burst of 40. The test suites set
`rate_per_sec: 0.0`, because a test sends requests as fast as the runtime will
carry them and wall-clock timing has no place in an assertion.

## What is left

### 1. Splits (§15.3)

Unchanged, and still the item with real work in it. A split has to rewrite,
atomically:

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

Suggested order: give the crate `Simulator::rescale`, `Candles::rescale` and
`OrderBook::rescale` with their own tests first, then wire the web app's
`POST /api/symbols/{s}/split` on top. The web app half is the easy half.

### 2. A buyback that retires stock (§15.3)

This one is now unblocked and small. It was waiting on `shares_outstanding`
being build metadata that the save file did not carry; it is now the listing's
own, saved and changed at runtime. What is left is the buyback itself: buy the
shares (from the book, or from the holders at a stated price), then lower
`shares_outstanding` by exactly what was retired, under the same lock, and
record it as a `corporate:buyback` event. The invariant to keep is the one
reconciliation already checks — held plus resting bids ≤ outstanding — so a
buyback that retires more than it bought would fail the check rather than pass
quietly, which is the right way round.

### 3. Splitting the market lock per symbol (§15.4)

I would still think twice about this one. `Mutex<Market>` is not only a
bottleneck; it is what currently makes four things correct:

- **reconciliation** sees one consistent market-wide snapshot;
- **share supply** (`held + resting bids ≤ shares_outstanding`) is checked
  and acted on without anything moving in between;
- **order ids** are allocated above every book's counter, and above the floor
  a delisted book left, under one lock;
- **listing and delisting** add and remove books while orders are being
  placed against the others.

Splitting it per symbol, with accounts still shared, means writing down an
ordering discipline for those and a lock hierarchy that cannot deadlock — and
the list grew by one this branch, which is the point: it is a design pass with
a performance target attached, not a refactor to do speculatively. A game
server with a handful of symbols and one engine step a second is nowhere near
needing it.

## Smaller things noticed along the way

- `cargo test --no-default-features` **including doctests** fails, and did
  before any of this: the README example the crate includes as its doc test
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
- **Re-listing a ticker reuses its name**, deliberately: the registry never
  frees one, so the old company's fills and ledger entries and the new
  company's sit under the same ticker in a trader's history. For a game that
  is the honest answer — `WDGT` is `WDGT` — but a venue that wanted them
  distinguishable would need a listing id beside the ticker, and that *is* the
  representation change this branch avoided.
