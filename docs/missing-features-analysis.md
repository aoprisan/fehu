# Missing features, weighed

A complexity-against-usefulness reading of every item in
[`missing-features.md`](missing-features.md), taken at the same head of
`main`. That document says what is absent and where the evidence is; this
one says what each absence costs to close, what closing it buys, and in what
order the work should go. Each claim in the survey was re-checked against
the code before it was scored here; every one holds.

## How the scores are made

**Usefulness** is measured against the project's own stated goal — the
acceptance scenario in [`economy-engine-plan.md`](economy-engine-plan.md):
a game backend onboarding players, paying rewards from budgets, players
buying from NPC merchants, running jobs, listing and trading the output,
and an audit that sums to the genesis supply after every step, every retry
and every restart. Tier one is one host and a trusted backend on the same
network; tier two is a live service. An item scores high when the
acceptance scenario, or the first real game to be run on this server, hits
it early, and low when nothing measured has asked for it.

**Complexity** is the cost to land the item to this repository's standard,
which is stricter than the line count suggests. The things that make an
item expensive here, in roughly descending order:

1. **A new `journal::Command` variant.** Every mutation is a variant, an
   `apply` arm that validates fully before touching anything, a handler that
   resolves everything replay cannot (clock, RNG, digests) and journals it,
   and a contract-test row. Cheap in lines, but each one is a promise about
   replay.
2. **A save-format bump.** `fehu_economy::save::STATE_VERSION` is refused on
   mismatch, so any new persisted field means a bump and, for anyone with a
   world, the importer the survey says does not exist.
3. **A library API change.** `crates/fehu` is `no_std`, published, and gated
   by golden hashes. Anything that touches RNG draw order bumps
   `fehu::STATE_VERSION` and the constants in `tests/determinism.rs`.
   Anything serialised bumps `EXCHANGE_VERSION` or the book format.
4. **A UI change.** `types.ts` is pinned to the Rust DTOs by `tests/contract.rs`,
   and the built bundle in `static/` is committed, so the smallest UI edit
   drags a rebuild and a bundle diff into the commit.
5. **Ordinary code.** A new read route, a new query parameter, a doc.

Scores are S / M / L / XL:

| | Roughly | Typically involves |
|---|---|---|
| **S** | an hour or two | one function, a doc line, a query param, a read route |
| **M** | half a day to a day | a command variant or a UI panel, tests, docs |
| **L** | a few days | several commands, a save bump, a library format bump, or a design choice with more than one defensible answer |
| **XL** | a milestone | a new subsystem, or anything that changes what a replay means |

Usefulness is 1 to 5: 5 blocks the acceptance scenario or the first real
game; 4 is hit within the first week of running one; 3 is hit eventually by
anyone running one; 2 is a rounding-off that the code already half-promises;
1 is speculative until something measures a need.

The leverage column is usefulness divided by cost, which is where the
ordering at the end comes from. Ties break towards the item that unblocks
another.

## Economy server

### Named by the plan or the docs, not built

| Item | Use | Cost | Leverage | Reading |
|---|---|---|---|---|
| Takings never come home | 5 | S | **very high** | One `Sweep` command: move a named issuer or venue wallet's balance to treasury (or a budget). No new state, no save bump. Without it the loop `world.html` animates does not close and every budget is funded by minting, so supply-in-circulation drifts up with no operator action. |
| Services share the anonymous bucket | 3 | S | **high** | `limit.rs` keys by user id; keying by service id is the "small change" the milestone-6 note names. Do it when the outbox poller starts spending the anonymous allowance, which is the first day a backend runs. |
| Job catch-up after downtime | 4 | M | high | One policy, configurable: on `App::resume`, jobs whose due instant passed while the server was down complete on the first `Step`, or are pushed forward by the gap. The step already carries the instant, so the arm is small; the design choice is the cost. Pick "complete on first step" as the default and journal nothing new. |
| Key rotation and revocation for players | 3 | M | medium | `auth.rs` stores digests, so rotation is "issue a new digest, retire the old one in the same command". One variant, one save-format field (the retired digest, or none if revocation is immediate). The plan defers it; a game with any churn wants it. |
| NPC producers | 4 | L | medium | The plan's producer "runs recipes and sells the output". `npc.rs` has the wallet, inventory and quoting; what is missing is a policy that starts a job when stock is low and the job path already exists. It runs inside the step, so it is journaled for free. The cost is the policy's shape (target inventory, restock threshold) and a save bump for the new policy fields. Worth doing after the sweep, since a producer that never sends takings home just accumulates. |
| Quotas beyond the limiter | 2 | S–M | medium | A cap on resting orders, stops and running jobs per trader is a counter and a refusal in three `apply` arms. Cheap, but the plan is right that nothing has measured a need. Do it when the first load test with real players runs. |
| An importer for older worlds | 1 | M | low | Exactly the "day's work when there is such a world". There is no such world. Not before the first save bump that somebody cares about. |
| v1 spellings for admin routes | 2 | S | medium | Aliases in the router; the handlers exist. Cheap and closes a documented gap, but a backend already talks to the un-versioned routes. Bundle into the next API commit. |
| Push delivery for the outbox | 3 | L | low | Webhooks need retry, backoff, a signed body, a dead-letter path and a new kind of outbound task in a server that has none. The pull outbox with a cursor is correct for tier one, and a WAN backend can poll every second at no real cost. Defer to tier two. |
| The SSE credential in the query string | 3 | M | low–medium | A short-lived stream ticket: one route that mints a ticket from a key, `stream` accepts either. The cost is the UI (`stream.ts`), the bundle rebuild and the ticket's own expiry. A real weakness across a proxy; a non-issue on localhost. Its own milestone, as the plan says, when tier two starts. |

### Gaps a game backend or an operator hits

| Item | Use | Cost | Leverage | Reading |
|---|---|---|---|---|
| Service scopes cannot read | 5 | S–M | **very high** | Accept `ServiceCaller` on the wallet, inventory, outbox and command reads under the matching scope. No new command, no save change, four handlers. A backend that holds the `inventory` scope and cannot read the inventory it just changed is the first wall the acceptance scenario hits. |
| The outbox carries only what the stream announces | 5 | M | **very high** | Route `PayReward`, `Purchase`, `Transfer`, `Dividend`, `Mint`, `Burn`, `StartJob` and `CancelJob` through `Market::announce`, or through a sibling that writes to the outbox without the stream. Each is one call at the end of an existing `apply` arm plus a fact DTO. Without it a backend must poll wallets to learn a player was paid, which defeats the outbox's purpose. Pair it with the read-scope item; together they make the service surface usable. |
| Pagination is `limit` only | 4 | M | high | Add `before` (a sequence or id) to ledger, tape, order history, event log and bars. Each store is already ordered, so it is a filter plus a query field, five times. Anything past the limit is unreachable today, which an audit tool hits on day one. |
| Reward rules: cooldown, per-player cap, expiry | 3 | M | medium | Three optional fields on `RewardRule`, a per-player last-paid map (save bump), and refusals in the `PayReward` arm. The plan says to implement cadence with configuration; the backend can enforce it itself today, so the value is in refusing what the backend gets wrong. |
| Jobs run one at a time | 3 | S–M | medium | A `quantity` on `StartJob`, defaulting to one, that multiplies inputs, cost and outputs. One arm, one field. Cheap and stops N journal entries for one batch. |
| Remove an NPC, close or drain a budget, change a listed symbol | 3 | M | medium | Three commands. Draining a budget is a transfer back to treasury and is close to the sweep above; removing an NPC needs its orders cancelled and its wallet drained first, so it composes existing arms. Symbol reconfiguration is the hard one: `Exchange::params` has no setters (see the library section), so it is blocked on that. |
| No market-wide halt | 2 | S | medium | A loop over symbols inside one command, or a `symbol: None` on the existing `Halt`. Cheap; rarely wanted until an incident. |
| The operator has no user directory | 3 | S | high | `list_users` returns the caller; let the operator key list everyone. A read route change. |
| Day orders refused without a calendar | 2 | S | medium | A `calendar` boolean on `/api/health` or the symbol status. Trivial. |

### Tooling and documentation drift

| Item | Use | Cost | Leverage | Reading |
|---|---|---|---|---|
| No CI configuration committed | 4 | S | **very high** | One workflow that runs `just ci`, plus a Node step for `just ui-check`. The `justfile` is already the recipe. The gap between "CI runs `just ci`" and "there is no CI" is the largest doc drift in the repository and the cheapest to close. |
| Three env vars undocumented | 3 | S | **very high** | Three rows in the README's table. `FEHU_NOW_MS` in particular is how a test pins the world's start instant, and nobody can find it. |
| Metrics inside `/api/health` | 2 | S | medium | A `/metrics` route in Prometheus text format over `metrics.rs`'s counters. Half an hour; tier two wants it, tier one does not. |
| No offline tooling | 3 | M | medium | A `fehu-economy verify <state> <journal>` subcommand that loads and replays without serving is mostly `App::resume` behind a flag. Restoring from a backup by pointing `FEHU_STATE_FILE` at it is acceptable; verifying a backup before trusting it is not possible today. |

## Utilities library

Everything here is gated by determinism and by a published, `no_std` API,
so the same feature costs more than its server twin. The survey's central
observation is right: the server reimplements four order features on top
of the book, and every other consumer would too. The question is whether
there is another consumer.

| Item | Use | Cost | Leverage | Reading |
|---|---|---|---|---|
| Stop orders, post-only, self-trade prevention, expiry, amend in the book | 2 | L–XL | low | Pulling the server's `check_post_only`, `self_crossing`, `sweep_expired` and cancel-then-resubmit into `book.rs` is a format change and a draw-order change for the exchange's synthetic flow, so a golden-hash bump and `EXCHANGE_VERSION` bump. The server already has them and works. Worth it only when a second consumer of the crate exists. Until then it is duplication with one copy. |
| The event queue is write-only | 3 | S | high | `Simulator::events()` returning a slice, and `retract(id)`. No draw-order change, no format change (the queue is already serialised). A cheap and obviously right addition. |
| Candles fixed to four intervals; no VWAP; no coarse ingest | 3 | M–L | medium | Making `Interval` carry a duration and `Candles` a `Vec` keyed by it is a storage change, so a format bump. Ingesting coarse candles into `Candles` would let `symbol.rs` drop its parallel `coarse_daily` vector and the read-time join, which is the concrete win. Notional per bar is one field. Do the ingest first; it simplifies the server. Fifteen-minute bars can wait. |
| Calendar: UTC only, no holidays, half days, breaks; weekend gap unweighted | 2 | M | low | A holiday list is a `Vec<u32>` of day numbers and a lookup; half days and breaks change what "session" means and touch the coarse path. A game's exchange does not observe Thanksgiving. Only the weekend-gap weighting is a modelling question, and it changes every golden hash. |
| Multi-currency half-built | 1 | XL | very low | The plan chose one currency deliberately; issuance, supply, flows and every audit sum are single-currency. Finishing it is a rewrite of `ledger.rs`'s invariants. Not until a game needs two currencies, which is a product decision the plan has explicitly not taken. |
| Fee reasons with no fee mechanism | 1 | M | low | The server's maker/taker fees are the fees. A `TradingParams` fee schedule that the exchange books would be a second implementation with no consumer. Leave `Reason::Fee` as the vocabulary it is. |
| Version mismatch is a dead end; dead `seed_priority` | 2 | S | medium | Delete `seed_priority` and the branch that can never run, or wire a one-version-back upgrade for the book. Deleting is the honest choice: the survey's own reading is that it cannot execute. |
| Serialisation gaps | 2 | S | medium | `rules` on `OrderBook` should round-trip (drop the `serde(skip)`, bump the book format); `Snapshot` and `Draft` get derives; `Ledger` gets `PartialEq`; `core::error::Error` replaces the `std`-gated impl. Four small independent edits; the `rules` one is the only format change. |
| Configuration immutable after construction | 3 | M | medium | Setters on `Exchange::params` and `Simulator::config` that re-run validation and re-derive the cached per-tick quantities. No draw-order change if the setter takes effect at the next step. This unblocks the server's "change a listed symbol's configuration" item. Re-export `MAX_PRICE_CENTS` in the same commit. |

## Browser UI

Every item here costs a bundle rebuild and a committed `static/` diff, and
nothing here blocks the acceptance scenario, which is played by a backend,
not a browser. The UI's job is to let an operator watch and an evaluator
play. Score usefulness against that.

| Item | Use | Cost | Leverage | Reading |
|---|---|---|---|---|
| Stop orders absent; `stop_triggered` and `order_expired` dropped | 4 | S–M | **high** | Two `case` arms in `stream.ts` that refresh the portfolio and post a notification are trivial and fix a real silence: a stop fires and the screen does not change. A stop ticket is a second form against a typed request that already exists. |
| Players cannot buy or consume goods | 4 | M | **high** | A catalogue view with a buy button, and consume on the inventory row. `api.ts` needs two calls. This is the gap that makes the workshop panel unusable: a fresh player has inputs for nothing. |
| Identity controls: paste a key, see it, sign out, second account | 4 | M | high | A key field and a sign-out button in the account panel; `actions.ts` already keeps the key in `localStorage`. Without it an evaluator cannot play two players from one browser, which the acceptance scenario requires. |
| Errors are one string; `Retry-After` discarded | 3 | S–M | high | Keep `code` and the header on `ApiError`; render `rate_limited` and `overloaded` with a countdown, `post_only_would_cross` and `self_trade` with their names. Small and it makes every other UI failure legible. |
| No `Idempotency-Key` on writes | 3 | S | high | A UUID per submit in `send`, retried with the same key. One function. A double-applied deposit from a retried click is a real bug in waiting. |
| Amend, cancel-all, order history, fills, ledger, transactions, transfer | 3 | M–L | medium | `api.ts` has the calls; each is a panel. Order history and the ledger view are the two an operator wants first; do those and leave the rest. |
| Operator dashboard has readings, few levers | 3 | L | medium | Each lever is a form against an existing route. Prioritise in this order: list a symbol, pay a reward, create a merchant, download a backup, issue and revoke a service key. Each is an hour; all of them are a few days. Replace the `window.prompt` for funding while there. |
| Ticket exposes three of eight options | 2 | S | medium | Five fields in one form. `post_only` and `expires_at_ms` are the useful two; `client_order_id` matters only once the idempotency item lands. |
| Typed data nobody renders; no deep links; no theme | 1–2 | S–M | low | Symbol status (next open, halt reason) is the one worth rendering. Deep links and theme are polish. |

## Where the leverage is

Ranked by usefulness over cost. The first block is under a week of work in
total and closes every gap the acceptance scenario would hit; each item is
independent of the others.

1. **Commit a CI workflow.** S. Every doc claims it exists.
2. **Document `FEHU_EVENT_LOG`, `FEHU_MAX_BARS`, `FEHU_NOW_MS`.** S.
3. **Let service scopes read** what they can write. S–M, no new command.
4. **A `Sweep` command** from issuer and venue wallets to treasury. S–M,
   one variant. Closes the economic loop.
5. **Put every money movement in the outbox**: rewards, purchases,
   transfers, dividends, mints, burns, job start and cancel. M.
6. **Key the rate limiter by service id.** S.
7. **`before` cursors** on the five `limit`-only reads. M.
8. **`stream.ts` handles `stop_triggered` and `order_expired`**; an
   `Idempotency-Key` on every UI write; structured errors with
   `Retry-After`. S–M together.
9. **The operator can list every user.** S.
10. **Delete `seed_priority`**, add `Simulator::events()` and `retract`,
    fix the four serialisation gaps. S each.

The second block is the next milestone's worth, each one a design choice
with a defensible default:

11. **Job catch-up policy** on resume, defaulting to "complete on the first
    step". M.
12. **A quantity on `StartJob`.** S–M.
13. **Setters on `Exchange::params` and `Simulator::config`**, then a
    server command to reconfigure a listed symbol. M + M.
14. **NPC producers** as a restock policy on the existing NPC. L; after the
    sweep so takings return.
15. **Player key rotation.** M, one save bump.
16. **Buy, consume and identity in the UI**, then order history and a
    ledger view. M each.
17. **Coarse ingest into `Candles`** so `symbol.rs` can drop its parallel
    vector. M–L, library format bump.
18. **An offline `verify` subcommand.** M.

Not now, and the reason:

- **Book-level stops, post-only, STP, expiry, amend.** Duplication with one
  copy is not a bug. Wait for a second consumer of the crate.
- **Webhooks, stream tickets, a `/metrics` route, quotas.** Tier two.
  Pull, a query-string key on localhost, counters in `/api/health` and the
  limiter are all correct for tier one, and the plan says tier two "is not
  on the path to a playable economy".
- **Multi-currency, a fee schedule in the library, holidays and intraday
  breaks, arbitrary candle intervals.** Each is speculative, and two of them
  change every golden hash. Nothing measured has asked.
- **An importer for older worlds.** The day it is needed is the day to write
  it; that day has not come.
- **Reward cooldowns and caps.** The backend that holds the `reward` scope
  can enforce cadence itself; build it in the server only once a backend
  gets it wrong.

## What the survey undercounts

Two things the survey lists as small are larger than they look, and one it
lists at all is not a feature.

- **"Change a listed symbol's configuration"** is scored as a missing command
  but is blocked on the library: with no setters on `Exchange::params` it is
  a serde round trip of a live exchange inside a command, which is exactly
  the kind of thing `apply` should not do. Item 13 above lands the library
  half first.
- **"NPC producers"** reads as a policy but is also a supply question: a
  producer that sells output and never sweeps takings is a wallet that grows
  forever. Item 4 is its prerequisite.
- **"The outbox carries only what the stream announces"** is not a gap in
  the outbox; it is a gap in which `apply` arms announce. That is why it is
  M rather than L: the plumbing exists, the calls do not.

Determinism is untouched by everything in the first block. The sweep, the
outbox facts, the read scopes, the cursors and the UI edits draw no
randomness and the price process never sees them; the golden hashes in
`tests/determinism.rs` need no change until item 17.
