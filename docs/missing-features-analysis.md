# Missing features, weighed

A complexity-against-usefulness reading of every item in
[`missing-features.md`](missing-features.md). This is the **second** reading:
the first was taken at the head that finished milestone 7, and the work it
ranked first has since landed. Every claim the survey still makes was
re-checked against the code at this head before it was scored again. All of
them hold but one — the quota row, which overstated what is uncapped and is
corrected in both documents — and the re-check added one row the survey had
missed (`main.rs`'s own environment list).

That document says what is absent and where the evidence is; this one says
what each absence costs to close, what closing it buys, and in what order the
work should go.

## What has landed since the first reading

Every item in the first block, and four of the second:

- a committed CI workflow; the three undocumented `FEHU_*` variables;
  read access for service scopes; a `Sweep` command; every money movement in
  the outbox; the rate limiter keyed by service; `before` cursors on the five
  `limit`-only reads; `stop_triggered` and `order_expired` in `stream.ts`,
  `Idempotency-Key` on UI writes and structured errors; the operator's user
  directory; `Simulator::events()` and `retract`, the dead `seed_priority`
  deleted and the four serialisation gaps closed;
- then **job catch-up on resume** and a **batch quantity on `StartJob`**
  (`FEHU_RESUME`, `runs`), **setters on `Exchange::params` and
  `Simulator::config`** with the server's `Reconfigure` command on top, and
  **NPC producers** (`npc::Production`: a recipe, a restock line and a
  `max_running` bound).

Three of those change what should be ranked next, and the tables below say so
where it matters:

1. **Producers make goods the browser cannot buy.** The world now
   manufactures inventory on its own, and the UI still has no purchase or
   consume path. That was the largest UI gap before; it is now the one that
   makes the running world illegible.
2. **`Sweep` exists but only for takings.** `Market::sweep` refuses any
   wallet that is not `Issuer` or `Venue`, so draining a budget — which the
   survey lists in the same breath as removing an NPC — is now the widening
   of one match arm rather than a new command.
3. **Producers are self-bounding.** `max_running` caps a producer's jobs, so
   the quota item still has nothing measured behind it — and the existing
   caps on stops and on running jobs make it smaller than the survey said.

## How the scores are made

**Usefulness** is measured against the project's own stated goal — the
acceptance scenario in [`economy-engine-plan.md`](economy-engine-plan.md):
a game backend onboarding players, paying rewards from budgets, players
buying from NPC merchants, running jobs, listing and trading the output, and
an audit that sums to the genesis supply after every step, every retry and
every restart. Tier one is one host and a trusted backend on the same
network; tier two is a live service. An item scores high when the acceptance
scenario, or the first real game to be run on this server, hits it early, and
low when nothing measured has asked for it.

**Complexity** is the cost to land the item to this repository's standard,
which is stricter than the line count suggests. The things that make an item
expensive here, in roughly descending order:

1. **A new `journal::Command` variant.** Every mutation is a variant, an
   `apply` arm that validates fully before touching anything, a handler that
   resolves everything replay cannot (clock, RNG, digests) and journals it,
   and a contract-test row. Cheap in lines, but each one is a promise about
   replay.
2. **A save-format bump.** `fehu_economy::save::STATE_VERSION` is 12 and is
   refused on mismatch, so any new persisted field means a bump and, for
   anyone with a world, the importer the survey says does not exist. Several
   remaining items each want one; they should share a single bump.
3. **A library API change.** `crates/fehu` is `no_std`, published, and gated
   by golden hashes. Anything that touches RNG draw order bumps
   `fehu::STATE_VERSION` and the constants in `tests/determinism.rs`.
   Anything serialised bumps `EXCHANGE_VERSION` or the book format.
4. **A UI change.** `types.ts` is pinned to the Rust DTOs by
   `tests/contract.rs`, and the built bundle in `static/` is committed, so the
   smallest UI edit drags a rebuild and a bundle diff into the commit.
5. **Ordinary code.** A new read route, a route alias, a query parameter, a
   doc.

Scores are S / M / L / XL:

| | Roughly | Typically involves |
|---|---|---|
| **S** | an hour or two | one function, a doc line, a query param, a route alias |
| **M** | half a day to a day | a command variant or a UI panel, tests, docs |
| **L** | a few days | several commands, a save bump, a library format bump, or a design choice with more than one defensible answer |
| **XL** | a milestone | a new subsystem, or anything that changes what a replay means |

Usefulness is 1 to 5: 5 blocks the acceptance scenario or the first real
game; 4 is hit within the first week of running one; 3 is hit eventually by
anyone running one; 2 is a rounding-off that the code already half-promises;
1 is speculative until something measures a need.

The leverage column is usefulness divided by cost. Ties break towards the
item that unblocks another.

## Economy server

### Named by the plan or the docs, not built

| Item | Use | Cost | Leverage | Reading |
|---|---|---|---|---|
| v1 spellings for admin routes | 3 | S | **very high** | The last unrouted piece of the documented surface, and the cheapest thing left. `Mint` and `Burn` already carry the memo the plan's table asks for, freeze and unfreeze are `SetAccountStatus`, and `markets/{symbol}/orders` and `game-events` are pure aliases of handlers that exist. Only `/admin/mint` and `/admin/burn` need a body-shaped wrapper, for which `purchase_body` and `consume_body` are the precedent. A backend restricted to `/api/v1/economy` cannot mint, freeze, push an event or trade today. |
| Key rotation and revocation for players | 3 | M | medium | Worth more than at the first reading: `ProvisionPlayer` means a backend mints player identities in bulk, and a leaked player key can be neither rotated nor revoked — only the account frozen. `auth.rs` stores digests, so it is "issue a new digest, retire the old one in the same command": one variant and one persisted field. Bundle its save bump with the reward-rule fields below. |
| Quotas beyond the limiter | 2 | S | medium | Less missing than the survey first said: stops are capped per trader at 100 and running jobs world-wide at 1 024, so what is open is a cap on resting orders per trader, and a per-trader share of the job capacity — a counter and a refusal in two `apply` arms. Producers cap themselves with `max_running`, so the only unbounded actor left is a player, and nothing has measured one. Do it with the first load test against real players. |
| The SSE credential in the query string | 3 | M | low–medium | A short-lived stream ticket: one route that mints a ticket from a key, `stream` accepts either. The cost is `stream.ts`, the bundle rebuild and the ticket's own expiry. A real weakness across a proxy; a non-issue on localhost. Its own milestone, as the plan says, when tier two starts. |
| Push delivery for the outbox | 3 | L | low | Webhooks need retry, backoff, a signed body, a dead-letter path and a kind of outbound task the server does not have. The pull outbox with a cursor is correct for tier one, and a WAN backend can poll every second at no real cost. Defer to tier two. |
| An importer for older worlds | 1 | M | low | Still exactly the "day's work when there is such a world", and there is still no such world. The argument for writing it early is that the next save bump makes one — but a demo world regenerated from seeds is not a world worth importing. |

### Gaps a game backend or an operator hits

| Item | Use | Cost | Leverage | Reading |
|---|---|---|---|---|
| Close or drain a budget | 3 | S | **very high** | `Market::sweep` already moves a wallet's balance to treasury; it refuses anything that is not `Issuer` or `Venue`. Draining a budget is that match arm plus a decision already taken elsewhere (takings go to treasury). Closing one — refusing further funding and payment — is a flag on the budget and a refusal in `PayReward`, which does want the save bump. Do the drain now, the close with the batch. |
| Remove an NPC | 3 | M | medium | One command that composes arms that exist: cancel its orders, sweep its wallet, stop its production, drop it from the table. Worth more now that a producer can sit on inventory and a wallet indefinitely; deactivating one leaves both stranded. The only design choice is what happens to its unsold stock — burn it, or leave it in the holdings table as an orphan. Burn it, and journal the burn. |
| Reward rules: cooldown, per-player cap, expiry | 3 | M | medium | Three optional fields on `RewardRule`, a per-player last-paid map (save bump), and refusals in the `PayReward` arm. With producers and the sweep landed, rewards are the last money tap bounded only by the size of the budget behind them. The backend holding the `reward` scope can enforce cadence itself, so the value is in refusing what the backend gets wrong — real, but second-order. |
| No market-wide halt | 2 | S | medium | A loop over symbols inside one command, or `symbol: None` on the existing `Halt`. Cheap; rarely wanted until an incident, and an incident is exactly when nobody wants to send N requests. |
| Day orders refused without a calendar | 2 | S | medium | Smaller than the survey makes it sound: `SymbolStatus` already implies the answer — `market_open` true with `next_open_ms` null means there is no calendar — but implying is not saying. One boolean on the status DTO, one `types.ts` row, one contract-test row. |

### Tooling and documentation drift

| Item | Use | Cost | Leverage | Reading |
|---|---|---|---|---|
| `main.rs`'s environment list is half the truth | 3 | S | **very high** | Its module doc names 17 `FEHU_*` variables and the code reads 35; `FEHU_NOW_MS`, `FEHU_RESUME`, `FEHU_MARKET_HOURS`, the fee and band knobs and both admission bounds are missing. The README's table is complete and was fixed at the first reading, so this is the same drift one file over, and the fix is to point at the README rather than to duplicate it a second time. |
| No offline tooling | 3 | M | medium | A `fehu-economy verify <state> <journal>` subcommand that loads and replays without serving is mostly `App::resume` behind a flag; `main.rs` takes no arguments at all today, so a three-arm `match` on `args()` is part of the cost. Restoring from a backup by pointing `FEHU_STATE_FILE` at it is acceptable; verifying one before trusting it is not possible, and a world that now survives downtime by replaying jobs is a world worth verifying. |
| Metrics inside `/api/health` | 2 | S | medium | A `/metrics` route in Prometheus text format over `metrics.rs`'s counters. Half an hour; tier two wants it, tier one does not. |

## Utilities library

Everything here is gated by determinism and by a published, `no_std` API, so
the same feature costs more than its server twin. The setters item from the
first reading has landed, which removed the one library row the server was
blocked on. What is left divides cleanly: one item that would simplify the
server, and five that wait for a second consumer of the crate.

| Item | Use | Cost | Leverage | Reading |
|---|---|---|---|---|
| Candles fixed to four intervals; no VWAP; no coarse ingest | 3 | M–L | medium | The coarse ingest is the only part with a concrete win: `symbol.rs` keeps a parallel `coarse_daily` vector and joins it to the aggregator at read time, and `Candles` accepting a finished candle would delete both. That is a storage change and a format bump. Notional per bar is one field. Arbitrary intervals are a different, larger change and nothing has asked for 15-minute bars. |
| Version mismatch is a dead end | 2 | S–M | low–medium | With `seed_priority` gone, what remains is the real question: `Simulator` and `Exchange` refuse every version but the current one, and there is no one-version-back path. Cheap to add for the book, pointless until a saved world from an older version exists — which is the server's importer question wearing a library hat. |
| Stop orders, post-only, self-trade prevention, expiry, amend in the book | 2 | L–XL | low | Pulling the server's `check_post_only`, `self_crossing`, `sweep_expired` and cancel-then-resubmit into `book.rs` is a format change and a draw-order change for the exchange's synthetic flow, so a golden-hash bump and an `EXCHANGE_VERSION` bump. The server has them and works. Duplication with one copy is not a bug; wait for the second consumer. |
| Calendar: UTC only, no holidays, half days, breaks; weekend gap unweighted | 2 | M | low | A holiday list is a `Vec<u32>` of day numbers and a lookup; half days and breaks change what "session" means and touch the coarse path. A game's exchange does not observe Thanksgiving. Only the weekend-gap weighting is a modelling question, and it changes every golden hash. |
| Fee and rebate reasons with no fee mechanism | 1 | M | low | The server's maker/taker fees are the fees. A `TradingParams` fee schedule the exchange books would be a second implementation with no consumer. Leave `Reason::Fee` as the vocabulary it is. |
| Multi-currency half-built | 1 | XL | very low | Issuance, supply, flows and every audit sum are single-currency by design. Finishing it is a rewrite of `ledger.rs`'s invariants, and which currencies a game has is a product decision the plan has deliberately not taken. |

## Browser UI

Every item here costs a bundle rebuild and a committed `static/` diff, and
nothing here blocks the acceptance scenario, which is played by a backend,
not a browser. The UI's job is to let an operator watch and an evaluator
play — and since the producers landed, the thing an evaluator most wants to
watch is a loop the browser cannot join.

Two rows this reading listed here have since been built — the stop ticket
(place, list and withdraw) and the five order options the form could not
reach — so what is below is what is left.

| Item | Use | Cost | Leverage | Reading |
|---|---|---|---|---|
| Players cannot buy or consume goods | 5 | M | **very high** | Promoted from 4. NPC producers now run recipes and quote the output, so the world manufactures goods continuously and a browser player can neither buy one nor consume one; the workshop panel shows recipes whose inputs are unobtainable. A catalogue section with a buy button and consume on the inventory row, against `POST /api/traders/{id}/purchases` and `/consume` — two calls `api.ts` does not yet have, one panel that does. This is the whole demo. |
| Identity controls: paste a key, see it, sign out, second account | 4 | M | high | `actions.ts` silently creates a trader named `player` and keeps the key in `localStorage` with no way to read it back, replace it or drop it. Without this an evaluator cannot play two players from one browser, which is what the acceptance scenario needs and what buying from another player requires. |
| Amend, cancel-all, order history, fills, ledger, transactions, transfer | 3 | M–L | medium | `api.ts` still holds `amendOrder`, `traderOrders`, `order`, `ledger`, `validateAccount` and `holdings` with no callers — the client half is written. Order history and the ledger view are the two an operator asks for first; do those and leave the rest until something wants them. |
| Operator dashboard has readings, few levers | 3 | L | medium | Wider than at the first reading, because the server grew levers while the dashboard did not: sweep a wallet, reconfigure a listed symbol and manage a producer's `Production` are all operator-only and all reachable only by `curl`. Each lever is a form against an existing route, an hour apiece. Order: list a symbol, sweep, reconfigure, merchant and production, issue and revoke a service key, pay a reward, download a backup. Replace the `window.prompt` for funding while there. |
| Typed data nobody renders; no deep links; no theme | 1–2 | S–M | low | Symbol status — next open and close, band, halt reason — is the one worth rendering, and it is the same DTO the calendar flag above would land in. Deep links, the theme control and the hard-coded book depth and tape length are polish. |

## Where the leverage is now

Ranked by usefulness over cost. The first block is about two days of work and
is what a person picking this up should do next; the items are independent of
each other.

1. **Point `main.rs`'s module doc at the README's table.** S. The same drift
   the first reading closed, one file over.
2. **v1 spellings for `/admin/mint`, `/burn`, `/freeze`, `/unfreeze`,
   `/game-events` and `/markets/{symbol}/orders`.** S. Aliases and two body
   wrappers; closes the documented surface.
3. **Let `Sweep` drain a budget**, and add the explicit `calendar` flag and
   the market-wide halt while in the same neighbourhood. S each.
4. **Buy and consume in the UI.** M. The producers have nobody to sell to.
5. **Identity controls in the UI.** M. (The stop ticket, which stood here,
   is built.)

The second block is one save bump's worth. Land them together as version 13,
so a world is refused once rather than three times:

6. **Player key rotation and revocation.** M.
7. **Reward rule cooldown, per-player cap and expiry**, and a budget that can
   be closed as well as drained. M.
8. **Per-trader quotas** on resting orders, held stops and running jobs — if
   the load test that motivates them has been run by then. S–M.

Then, each on its own merit:

9. **Remove an NPC**, burning its stock and journaling the burn. M.
10. **An offline `verify` subcommand.** M.
11. **Order history and a ledger view in the UI**, then the operator's
    levers in the order listed above. M, then L.
12. **Coarse ingest into `Candles`**, so `symbol.rs` can drop its parallel
    vector and its read-time join. M–L, library format bump.

Not now, and the reason:

- **Webhooks, stream tickets, a `/metrics` route.** Tier two. Pull with a
  cursor, a query-string key on localhost and counters in `/api/health` are
  all correct for tier one.
- **Book-level stops, post-only, STP, expiry, amend.** Duplication with one
  copy is not a bug. Wait for a second consumer of the crate.
- **Multi-currency, a fee schedule in the library, holidays and intraday
  breaks, arbitrary candle intervals.** Each is speculative, and two of them
  change every golden hash.
- **An importer for older worlds, and a one-version-back path in the
  library.** The day one is needed is the day to write it. The demo world is
  regenerated from seeds.

## What this reading changes, and what it confirms

Three rows moved, and each moved because something landed rather than because
the first reading was wrong:

- **Buy and consume in the UI, 4 → 5.** A world that manufactures goods
  nobody in the browser can buy is worse than a world that manufactures
  nothing.
- **Drain a budget, M → S.** `Sweep` did the hard half and stopped one match
  arm short.
- **Remove an NPC, held at M but wanted sooner.** A producer accumulates; a
  deactivated one accumulates quietly.

And two things the survey is still right to list and right to rank low: the
quotas, because `max_running` means the only unbounded actor is a player and
no player has been measured, and the importer, because the world it would
import does not exist.

Determinism is untouched by everything in the first two blocks. The aliases,
the sweep widening, the calendar flag, the save-version-13 fields and every
UI edit draw no randomness and the price process never sees them; the golden
hashes in `tests/determinism.rs` need no change until item 12.
