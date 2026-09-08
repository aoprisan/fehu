# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A cargo workspace with two crates and a TypeScript UI:

- **`fehu`** (repo root, `src/`) — a `no_std + alloc` deterministic synthetic
  stock price simulator, plus an optional limit order book and exchange on
  top of it. This is the published library; it is the thing that must stay
  portable and bit-reproducible.
- **`fehu-webapp`** (`webapp/`) — a sample axum server that runs several
  `fehu` symbols in wall-clock time behind an HTTP/SSE API. `std`, tokio,
  `publish = false`.
- **`webapp/ui/`** — Vite + TypeScript, no framework. Its build output is
  **committed** to `webapp/static/` and embedded into the Rust binary.

`README.md` is included as the crate-level rustdoc (`#![doc = include_str!]`),
so its Rust snippets are compiled as doctests — a broken example fails
`cargo test`. It is also the reference for the whole HTTP surface and every
`FEHU_*` environment variable; read it before touching an endpoint.

## Commands

`just` (see `justfile`) is the entry point; `just ci` is everything CI runs.

```sh
just test            # cargo test --all-features, then --no-default-features, then -p fehu-webapp
just lint            # fmt --check + clippy -D warnings over every feature set and the webapp
just build           # std, no_std, and no_std+serde
just wasm            # wasm32-unknown-unknown build (no_std, with and without serde)
just bench           # criterion, benches/ticks.rs
just serve           # run the webapp on :3000
just ui              # npm ci && vite build -> webapp/static (commit the result)
just ui-dev          # Vite on :5173 with HMR, proxying /api to a running `just serve`
just ui-check        # rebuild the bundle and fail if webapp/static is stale
just dump out 42     # a year of ticks + daily candles to CSV via examples/dump.rs
```

Single test / single file:

```sh
cargo test --all-features --test determinism            # one integration test file
cargo test --all-features golden_hash_default_config    # one test by name
cargo test -p fehu-webapp --test contract
cargo test --no-default-features --tests                # the no_std path (skips doctests)
```

Feature combinations matter: `std` (default), `serde`, and bare `no_std +
alloc` are all exercised separately, and clippy runs with `-D warnings` on
each. A change that only compiles under `--all-features` is broken.

## Determinism is the core invariant

Same seed + same events ⇒ byte-identical ticks on native and wasm. Everything
below follows from that:

- **All transcendental math and every RNG-to-float conversion goes through
  `src/math.rs`.** Only `libm` for `exp`/`ln`/`sqrt`/`pow`/`cos`/`tanh`/
  `round`; only `next_u64` from the RNG; never `mul_add` or `powi` (they lower
  to LLVM intrinsics that differ per target). Do not call `f64::exp()` etc.
  directly anywhere in `src/`.
- **RNG draw order is part of the format.** `tests/determinism.rs` pins golden
  FNV-1a hashes of 10 000 ticks for three configs. If you deliberately change
  the model or the draw order, bump `STATE_VERSION` (`src/sim.rs`) and update
  the constants in that test — never update the constants alone.
- Separate concerns get separate RNG streams (the exchange's synthetic flow
  uses a `long_jump`ed stream), so adding trading does not perturb the bare
  price series. `tests/trading.rs` holds that invariant.
- Version constants that gate save compatibility: `fehu::STATE_VERSION`
  (simulator), `fehu::EXCHANGE_VERSION`, `fehu_webapp::save::STATE_VERSION`
  (the whole market file, currently 12) and
  `fehu_webapp::journal::JOURNAL_VERSION` (the command journal beside it,
  currently 2). Loading a mismatched version is refused rather than guessed
  at.
- JSON round-trips need `serde_json`'s `float_roundtrip` feature; without it a
  parsed `f64` can be one ulp off. Binary formats (postcard) are always exact.

## Library architecture (`src/`)

`lib.rs` is only re-exports; the module docs are the real reference.

- `sim.rs` — the price process. A slow **fundamental** driven only by game
  inputs; the **price** mean-reverts (OU) toward it with GARCH(1,1) volatility
  clustering and Poisson jumps. `step()` documents the numbered per-tick
  algorithm, and `evolve()` is steps 2–9 shared by the fine and coarse paths.
  Coarse mode (`coarse_candles`) takes one latent step per candle and draws
  high/low from the Brownian-bridge extremum distribution — that is how the
  webapp generates a year of daily history at start-up.
- `event.rs` / `config.rs` — external events with decaying effects, and config
  validation plus the per-tick derived quantities cached from it.
- `book.rs` — a standalone price–time-priority limit order book. Whole cents,
  integer shares, sequential ids. It knows nothing about the price process.
- `ledger.rs` — the conserved currency ledger: wallets, balanced
  transactions, supply, and a flow meter counting what each `Reason` has
  moved. Pure integer arithmetic, no clock and no globals.
- `exchange.rs` — composes the two: trader net flow becomes a square-root
  price-impact event on the simulator, the simulator steps, the tick volume
  prints as synthetic flow against the book, and a synthetic maker ladder is
  re-quoted around the new reference. With no trader orders the series is
  bit-identical to the bare simulator's.
- `time.rs`, `candles.rs` — timestamps and the optional market-hours calendar;
  OHLCV aggregation.

## Webapp architecture (`webapp/src/`)

**There are no locks in the server.** Every piece of state belongs to exactly
one tokio task and is reached by sending it a closure (`actor.rs`). Read
`market.rs`'s module docs before changing anything here; `docs/architecture.html`
animates the same thing.

The rule that keeps it deadlock-free: **calls go one way — the market calls the
symbols; the symbols call nobody.** A handler calls the market, or a symbol
directly for a read, never one from inside the other.

| Actor | Owns | Who sends it jobs |
|---|---|---|
| one `SymbolState` per symbol (`symbol.rs`) | simulator, book, bars, tape, halt, stops | anyone for reads; **only the market** for anything that changes the book |
| `Market` (`market.rs`) | users, accounts, traders, order log, event log, symbol table, order-id counter | request handlers and the engine |
| `Stream` | sequence counter and replay buffer | anyone to publish, connections to subscribe |
| rate limiter (`limit.rs`) | token buckets | the rate-limit middleware |

Admission (`limit.rs`'s `Admission`) is not an actor: it is a pair of
semaphores, so a request that finds the server full is refused without
waiting on anything. `FEHU_MAX_INFLIGHT` bounds concurrent mutations and
`FEHU_MAX_STREAMS` open SSE connections; past either the answer is
`503 overloaded`. Reads are never gated.

Consequences worth knowing:

- Every change to money or to a book is **one job on the market actor**, which
  calls the symbol for the book operation and books the money side before the
  next job runs. That is what keeps reservations, `held + bids ≤ outstanding`
  and fills consistent without a mutex.
- The engine step (`engine.rs`) is one such job that fans the advance out to
  every symbol at once and joins them; reads of a quote, book or bars go
  straight to the symbol actor and queue behind nothing.
- A consistent snapshot of the whole market — for `save.rs` or `reconcile.rs`
  — is one market job that asks each symbol for a copy of itself.
- Auth and the symbol table are published on `tokio::sync::watch` as immutable
  snapshots, so request handlers read them without sending a job.

### Commands and the journal

**Nothing changes the market except a `journal::Command`.** Every mutating
handler in `api.rs` authorises the request, cleans it, builds a `Command` and
calls `run`/`run_at`; `journal::apply` is the only implementation of what a
mutation does, and the reply is a `Committed` — a JSON body plus
`Fehu-Journal-Seq`. Adding a route that changes something means adding a
variant and an arm, not a new closure on the market actor.

`Market::run_command` is the funnel: it checks the `Idempotency-Key` index,
applies the command, appends the entry and `fsync`s it, and only then
answers. Start-up loads the snapshot and replays the entries after it
(`App::resume`) through that same `apply`, so the live path and the replay
path cannot drift apart.

Two rules follow, and both are easy to break by accident:

- **A command may not read anything a replay cannot.** No clock, no RNG, no
  `wall_now_ms()`. The simulated instant is pinned on the market for the
  duration (`Market::now`, *not* `self.clock.now()`) and the wall clock is a
  field of the entry; anything else the server generates — an API key digest
  — is resolved by the handler and journaled. The engine tick is a command
  (`Command::Step`) for exactly this reason.
- **Refusals are not journaled.** Every apply path validates fully before it
  mutates, so a refused command leaves nothing behind and a retry of one is
  simply re-applied.

The journal file is truncated when a snapshot lands, so `save.rs` and
`journal.rs` are two halves of one thing; the idempotency index lives in the
snapshot, because a snapshot may fall between any two commands. No credential
ever reaches either: a key exists once, in the response that issued it.

Other modules: `catalog.rs` (what the world will make and what it charges;
goods are issued by a purchase and destroyed by consuming them), `jobs.rs`
(recipes, and the jobs run against them: the inputs and the cost go when a
job starts, the outputs arrive on the engine step that reaches its due
instant), `rewards.rs` (budget wallets, what a named reward is worth, and
every game event id already paid — a reward is idempotent on the *game's*
event id, not only on the `Idempotency-Key`), `world.rs` (what a game event
does to production and demand, in integer basis points ramping down in a
straight line), `npc.rs`
(the funded traders the world runs itself, re-quoted inside the engine
step), `outbox.rs` (the durable, cursor-replayable log of the facts nobody
asked for — what the game backend reads instead of the SSE stream),
`account.rs` (users, accounts, cash ledger, and the mapping from the game's
own player id onto them), `trading.rs`
(traders, positions, share reservations, wire DTOs), `auth.rs` (API keys —
issued once, stored as domain-separated SHA-256 digests), `service.rs` (the
third principal: the game backend's own credentials and the scopes each
carries — a scope narrows a credential, never the operator), `events.rs` (raw
simulator events and the semantic game-event catalogue), `symbols.rs`
(tickers registered and `Box::leak`ed to `&'static str`, capped and length-
bounded), `api.rs` (all HTTP handlers and the SSE stream), `journal.rs`
(commands, the append-only journal, replay), `metrics.rs`.

### Money and shares

All money is an **integer count of cents** — no floating point anywhere in
`account.rs`, formatting included. A trader's cash lives in the `Account` it
trades on, so anything touching money takes that account and books the
movement through its ledger. Resting buys reserve cash and resting sells
reserve shares; a cancel gives them back. No margin, no shorting.

## Conventions

- `src/` is `#![no_std]`, `#![forbid(unsafe_code)]`, `#![warn(missing_docs)]`.
  Nothing in the library may reach for `std`, a clock, or a global — a
  `#[cfg(feature = "std")]` island is the only exception.
- Module-level `//!` docs carry the design rationale and are expected to stay
  accurate; when you change behaviour, change the module doc in the same edit.
- `webapp/tests/contract.rs` pins the exact JSON key set of every response
  against `webapp/ui/src/types.ts`. Renaming or adding a serialised field
  fails that test — **change the Rust type and `types.ts` together**.
- `webapp/static/` is committed build output. After any change under
  `webapp/ui/src/`, run `just ui` and commit the regenerated bundle;
  `just ui-check` (part of `just ci`) fails otherwise. Asset names are fixed
  (`assets/app.js`, `assets/app.css`) because `include_str!` cannot name a
  content-hashed file.
- UI data flow is one-way: `actions.ts` is the only writer, it mutates
  `store.ts` and emits topics, panels subscribe and re-render. Nothing renders
  straight from a fetch response.
- The operator's dashboard (`panels/ops.ts`, opened by `#economy`) is the one
  panel that polls. It reads `GET /api/overview` — one market job, assembled
  in `Market::overview`, so the whole reading is one instant — and only while
  it is open. Anything expensive stays off that path: `/api/reconcile`
  snapshots the world and is a button. Rates are the page's own arithmetic
  over the samples it keeps, because neither the ledger's flow meter nor
  `metrics.rs` keeps history, deliberately.
