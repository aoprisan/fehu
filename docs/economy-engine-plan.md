# Game economy engine implementation plan

Assessment date: 2026-09-07. This is a proposed implementation, grounded in the current source; no runtime changes are included.

Fehu can become the authoritative economy server for a game. Its deterministic simulator and exchange are useful subsystems, but it currently models a synthetic financial market rather than a complete economy. The main work is a conserved currency ledger, durable atomic settlement, trusted game commands, and resource production and consumption.

Use a centrally issued, custodial game token as the currency. Give it balances, transfers, an explicit supply, mint/burn authority, and an auditable transaction history. A blockchain is unnecessary for this deployment. Merely listing a symbol named COIN would create a traded asset priced in the existing cash; it would not make that asset the settlement currency.

**Working assumptions:** one persistent game world per deployment; one currency (GAME); server custody; no real-money redemption or external chain; trusted game backend authorizes rewards; players can trade and transfer within permissions. These are implementation defaults, not requirements inferred from a specific game genre. Multiple currencies, external wallets, and cross-world transfers are later extensions.

## Current capabilities and gaps

| Area | Evidence in this checkout | Required change |
|---|---|---|
| Price simulation | `src/sim.rs`, `src/event.rs`, `src/exchange.rs`: seeded reference prices, event effects, synthetic flow and trader impact | Retain as a price reference and NPC strategy input; distinguish reference price from actual traded price |
| Exchange | `src/book.rs`, `webapp/src/market.rs`: orders, fills, reservations, stops, fees, listing and delisting | Settle currency and goods against funded counterparties; generic resource markets need explicit asset identities |
| Accounts | `webapp/src/account.rs`: integer cents, status checks, balances, reservations, capped per-account ledger | Add balanced transactions, global supply accounting, system wallets, atomic transfers and durable history |
| Issuance permissions | `api.rs::open_account`, `deposit`, `trader_deposit`, `withdraw`, and trader creation allow player-controlled funding and defunding; account status changes require only ownership | Remove funding and withdrawal from player commands; separate operator freezes from voluntary account closure |
| Synthetic liquidity | `market.rs::book` only settles owners with trader IDs; `reconcile.rs` explicitly notes synthetic inventory is not independently held | Replace synthetic execution in economy mode with budgeted NPC orders; all fills need currency and goods on both sides |
| Corporate payouts | `account.rs::pay_dividend`, `pay_delisting` credit holders, with balance-cap clipping | Debit an issuer/treasury or record authorized issuance; never silently clip contractual payouts |
| Arithmetic | `account.rs::notional_cents`, settlement and reservations use saturation in several paths | Checked arithmetic and explicit failure before any mutation for authoritative amounts |
| Persistence | `save.rs`: version 5 snapshots, file sync and rename, optional periodic autosave; restore validation | Commit each acknowledged economic mutation durably; snapshots become recovery checkpoints/export |
| Retries | `market.rs::record_order` evicts completed orders and their client IDs; saves include original order responses | Durable command deduplication independent of display history, including rewards and transfers |
| Actor topology | `actor.rs`, `market.rs`: serialized market writes, symbol tasks, unbounded mailboxes | Keep one economy writer; add bounded admission and an explicit durable commit boundary across book and ledger |
| Integration | `api.rs`, `events.rs`: JSON APIs, semantic events mapping to price changes, in-memory SSE replay | Economic commands, durable event delivery, game identity mapping, documented versioned contracts |
| Game economy | Current domain consists of symbols, accounts, traders and simulator events | Add goods, inventories, recipes, timed jobs, NPC demand, rewards and sinks |

Existing save, contract and concurrency tests are useful starting points. They do not establish durable money conservation: reconciliation checks retained ledger entries and shares, not a complete funded counterparty ledger. This assessment is based on source inspection; the existing test suite was not run for this documentation-only change.

## Currency design

GAME is the unit of account and quote currency. Preserve integer cents: 1 GAME = 100 cents. Do not introduce an 18-decimal denomination merely to resemble crypto. Use `CurrencyId` now, but implement one quote currency initially. Preserve signed integer posting deltas, nonnegative wallet balances, and checked wide intermediate multiplication. Keep monetary integers as JSON numbers, matching every existing DTO in `webapp/ui/src/types.ts` and the contract test: `MAX_BALANCE_CENTS` (10^15) is below 2^53, so JavaScript clients lose nothing today. Switch to decimal strings only if the cap is raised past 2^53, and do it for every money field at once under a new API version.

The relevant inspiration from [ERC-20](https://eips.ethereum.org/EIPS/eip-20) is its balance, supply and transfer interface. The proposed HTTP service is not ERC-20 compatible, and bearer API keys are not blockchain wallet keys. Spending allowances and signed player transactions can be added if gameplay requires delegated spending; they are unnecessary for the initial trusted-backend integration.

Create wallets for players, treasury, reward budgets, NPC merchants, companies and fee collection. Minting moves value from an issuance control account into a wallet and increases supply; burning reverses that operation. Ordinary transfers, rewards paid from a budget, trading fees and taxes only redistribute supply. Mark actual burns explicitly. Treasury funds remain part of total supply; report circulating player supply separately.

Start with an operator-configured genesis supply placed in treasury, then allocate finite reward and NPC budgets. Permit further minting only through a separately scoped operator command with a reason and configured cap. Do not choose inflation rates or genesis amounts before game reward/spend rates are specified. Measure faucets and sinks before introducing automated monetary policy.

Invariants enforced on every commit:

- Posting deltas sum to zero per currency, including the issuance control account. Sum of spendable wallet balances equals minted minus burned supply.
- Wallet balance is nonnegative; `0 <= reserved <= balance`; available is balance minus reserved. Holds do not increase supply.
- A transfer debits and credits exactly once. Every fee, rebate, payout and NPC trade has an explicit funding source.
- Inventory is nonnegative; goods held for orders or jobs cannot be transferred or consumed. Production and consumption have explicit quantities and causes.
- A rejected command changes neither authoritative state nor supply. Repeating an accepted command returns its original result.
- Freeze policy blocks new outgoing spending and trading; freeze cancels open orders and stops atomically and releases their holds. Credits remain allowed; only operators may unfreeze an operator freeze. Closure requires no balance, inventory, holds or jobs.

A future on-chain version is a separate project: decide chain, custody, signing, deposits, withdrawal limits, confirmations and reorg recovery. Use an escrow-backed bridge and unique external transfer IDs; never run independently mintable internal and external supplies under one advertised total. Defer this until interoperability is an actual game requirement.

## Authority, storage and transaction boundary

Keep the `fehu` library portable and deterministic. Put game-domain transitions in a new `fehu-economy` workspace crate with no clocks, network or database access; preserve `no_std + alloc` where practical. The Axum application owns authentication, persistence, clock inputs and command sequencing. Initially deploy one writer per world, with read projections for quotes and histories.

Use SQLite on persistent local storage for the first single-server deployment. Its [atomic transactions](https://www.sqlite.org/atomiccommit.html) provide the all-or-nothing storage boundary; it permits [one concurrent writer](https://www.sqlite.org/lang_transaction.html), consistent with the proposed actor model. Durability settings, filesystem behavior and failure recovery must be validated in implementation. Multi-host failover or sustained write load beyond this design requires a separate storage/deployment decision, potentially PostgreSQL; adding HTTP replicas alone does not create safe multiple economy writers.

Proposed persistent records:

| Records | Essential fields and constraints |
|---|---|
| World/currency configuration | world ID, schema/rules version, currency ID, decimals, cap, minted/burned counters, simulation tick |
| Identities/wallets | external player ID unique per world, principal scopes, owner, wallet kind, currency, status, balance/version |
| Credentials | API key digests as `auth.rs` stores them (domain-separated SHA-256), principal, scopes, issued/revoked; keys remain issued once and never stored in clear |
| Transactions/postings | immutable transaction ID, command ID, tick, recorded time, reason/source; signed postings with wallet and currency |
| Commands | unique `(world, principal, idempotency_key)`, canonical payload hash, command sequence, status and original response |
| Reservations | unique hold ID, wallet or inventory owner, amount/quantity, purpose, order/job reference and lifecycle |
| Assets/inventory | asset type, unit/lot size, transferability, owner quantity, issuance/consumption totals |
| Markets/orders/fills | base asset, quote currency, matching sequence, immutable fill ID, order lifecycle and reservation references |
| Recipes/jobs | versioned recipe, inputs, outputs, cost, start/due tick, unique completion ID |
| Checkpoints/outbox | compatible state snapshot and committed sequence; ordered durable domain events and delivery cursor |

Do not add a database call after the existing live book mutation and call that atomic. Implement a staged transition:

1. Admit and authenticate a typed command, resolve world/principal, and check its durable deduplication record. Same key and payload returns the original result; changed payload returns `409 idempotency_conflict`.
2. Sequence it in the economy actor. Evaluate matching and settlement on isolated candidate state using a fixed simulation tick, rule version and recorded RNG state. Initially correctness can use cloned affected state; optimize with transition deltas after profiling.
3. Validate all postings, inventory changes, holds and supply constraints. A failed validation discards the candidate, including all book changes.
4. Commit the command (its canonical payload, clock and tick inputs, rule version and RNG position), its result, economic records and outbox events in one database transaction. The command journal is the single source of truth: the library is deterministic, so a checkpoint plus the ordered journal replays to the exact committed state. Do not also persist state deltas; a second representation can disagree with replay. Revisit only if replay from the last checkpoint is measured to be too slow for recovery, and then persist deltas as a cache validated against replay, never as authority.
5. Install the committed candidate and update read projections, then acknowledge and publish events. If installation fails after commit, stop serving economic mutations and rebuild from durable state. If commit outcome is uncertain, resolve using the command ID before retrying.

Symbol actors may calculate candidate transitions but must not expose them as committed state. Start with the economy actor owning authoritative mutable books and symbol actors serving committed projections; parallelize preparation later if needed. This avoids distributed commit across symbol tasks: today only the market changes a book, but the engine step fans one job out to every symbol task and each mutates its own book concurrently before the join. Engine ticks, stop triggers, expiries, NPC decisions and production completions must use the same pipeline. Publish no fill before its settlement commits.

Checkpoint plus ordered committed commands must restore books, holds, simulator/RNG state, jobs and balances at one sequence. Record policy changes and tick order, so replay never depends on current wall time or a changed recipe. Preserve the original simulation mode and its deterministic native/Wasm tests. Funded mode may skip the synthetic ladder and prints without perturbing the bare price series, because that flow already draws from its own `long_jump`ed stream; any change to the draw order of the shared path must bump `STATE_VERSION` or `EXCHANGE_VERSION` and update the golden hashes in `tests/determinism.rs` in the same change, never the constants alone.

## Game interactions and API

Use `/api/v1/economy` for the authoritative contract; retain legacy APIs only behind an explicit compatibility flag until the funded path passes the acceptance scenario, then remove them. Require authenticated service credentials in economy mode and fail startup if authority/storage configuration is missing. Never put operator credentials in the browser. Scope service principals to provisioning, rewards, inventory grants, events or administration; derive player identity from authentication rather than trusting a body field.

| Endpoint under `/api/v1/economy` | Caller and behavior |
|---|---|
| `POST /players` | Game service; idempotently map external player ID and create zero-balance wallet |
| `GET /wallets/{id}`, `GET /wallets/{id}/transactions?cursor=` | Owner or scoped service; balance, held/available amounts and durable history |
| `POST /transfers` | Owner or authorized service; atomically move available currency to another wallet |
| `POST /rewards` | Reward service; source event ID, configured reward rule and recipient; transfer from budget, never trust client reward amounts |
| `POST /purchases` | Player/service; catalog item and quantity; atomically charge currency and deliver inventory at server-authoritative price |
| `POST /admin/mints`, `/admin/burns`, `/admin/wallet-status` | Restricted operator; policy validation and immutable audit reason |
| `GET /currencies/{id}/supply` | Authorized game observer; total supply, treasury, player holdings and mint/burn totals |
| `GET /players/{id}/inventory`, `POST /inventory/transfers` | Owner/service; query and move available goods under asset rules |
| `POST /production-jobs`, `GET /production-jobs/{id}` | Owner/service; validate recipe, consume/hold inputs and schedule completion |
| `POST /markets/{id}/orders`, `DELETE /markets/{id}/orders/{order_id}` | Owner; funded atomic settlement and reservation lifecycle; preserve existing order features as migrated |
| `POST /game-events` | Scoped game backend; durable unique source event, versioned effects on production/demand/reference price |
| `GET /commands/{id}`, `GET /events?after=` | Authorized caller; recover uncertain responses and replay durable events |
| `GET /admin/reconciliation` | Operator; global supply, ledger, inventory, order and job consistency |

Every mutation requires `Idempotency-Key`; game events also have a unique `(source, event_id)` independent of transport retries. Keep deduplication evidence for the world's lifetime initially. Authorize access before returning any replayed response. Use stable error codes (`insufficient_funds`, `insufficient_inventory`, `budget_exhausted`, `frozen`, `idempotency_conflict`, `overloaded`) and a request ID. Include transaction ID and committed world sequence in success responses.

Example transfer body: `{"from_wallet_id":"w1","to_wallet_id":"w2","currency_id":"GAME","amount_cents":2500}`. The sender must be owned or explicitly delegated to the caller. A retry after a timeout uses the same key; it must not debit again.

Provide an OpenAPI contract and a small TypeScript client for the game backend. Durable events use at-least-once delivery with stable event IDs and authorized cursor replay; consumers deduplicate. Keep high-frequency quote SSE separate from economic events. Use header-based streaming authentication or short-lived stream tickets instead of long-lived keys in URLs. The committed bundle currently sends the key as `?api_key=` from `webapp/ui/src/stream.ts`, so this change regenerates `webapp/static` via `just ui` and must ship with the server change. Add bounded queues, body limits, per-service/player quotas and restrictive configurable CORS.

Fehu should own currency, economic inventory, jobs and market orders for atomic purchases. The game owns combat, movement and quest adjudication and submits trusted results. A quest reward is not proof that a quest occurred: the game backend must validate it. If inventory remains in another service, an outbox-driven delivery/compensation workflow replaces local atomic delivery; specify pending status and recovery before enabling purchases in that configuration.

## Economy simulation beyond trading

Implement fungible goods first: ore, fuel, food and crafted materials, with integer inventory units. Unique items, durability and land ownership are extensions. Stocks may remain a distinct asset type; do not apply dividends, fixed shares outstanding or delisting buyouts to consumable goods.

Add versioned recipes and timed jobs: for example, consume 2 ore and 1 fuel, pay a configured fee, then produce 1 ingot at the due simulation tick. Define cancellation explicitly: before start, release holds; after inputs are consumed, no automatic refund unless the recipe defines one. Completion is durable and occurs once even across restarts. Pause simulation during server downtime initially; catch-up/offline production is an explicit later policy with bounded work and recorded tick advancement.

Add funded NPC merchants and producers. Their demand, inventory targets and recipe costs determine order intentions; the existing simulator may supply a noisy reference signal. Economy markets must bypass both the automatic synthetic ladder and synthetic taker flow, which can currently fill player orders during ticks and requotes. NPC orders use ordinary funded trader identities, fees and holds. Exhausted NPC inventory/currency means reduced liquidity or no quote, not implicit issuance.

Prefer one market mode. Two modes means two code paths through `market.rs` and `exchange.rs`, two contract-test surfaces and two UI behaviours maintained indefinitely. The demo should be economy mode with an operator faucet and generously funded NPC merchants rather than a separate synthetic mode; keep the legacy synthetic path only for the deterministic library tests and the bare `fehu` crate, and remove it from the server once funded markets pass the acceptance scenario. Expose last trade, bid/ask, traded volume and reference separately; charts must not label generated reference volume as executed economic trades. Report per-window mint/burn, reward spending, consumption, player supply, inventory stocks, turnover, spreads and price baskets. Balance these in scripted scenarios before tuning live parameters.

## Implementation sequence and acceptance gates

| Phase | Implementation scope | Exit evidence |
|---|---|---|
| 1. Authority boundary | `api.rs`, `auth.rs`, `market.rs::Options`; economy mode with a legacy-route compatibility flag, scoped service identity, zero-funded onboarding, deny player deposits/withdrawals/mints/unfreezes, inventory authority decision | Contract tests prove players cannot create currency through any legacy or new route; missing production credentials/storage fail closed |
| 2. Domain ledger | New `fehu-economy` crate; wallet/currency/transaction/hold types, checked arithmetic, treasury issuance, transfers, fees and budgets; replace balance mutators | Property tests prove conservation, no overflow/negative balances, exact holds, atomic insufficient-funds rejection and freeze behavior |
| 3. Durable command execution | New `webapp/src/storage/` and migrations, typed commands, transaction boundary, immutable journal, deduplication, checkpoints/outbox; refactor actors and save path | Crash injection before/after commit and before response proves no acknowledged loss, partial transfer or duplicate reward; restart reconstructs exact committed state |
| 4. Funded trading | `src/exchange.rs`, `market.rs`, `trading.rs`, `reconcile.rs`; explicit funded mode, NPC wallets, atomic book/ledger/inventory changes, funded payouts, exact fees/rebates | Player-player and player-NPC partial fills, stops, IOC/FOK, amendments, dividends and delistings conserve currency/assets; failed commits expose no ghost fills |
| 5. Game loop | Asset catalog/inventory, recipes/jobs, reward and purchase services, NPC demand/production, scheduled economic effects | Full scenario below survives retries and restarts with correct goods and supply; scarce inputs/budgets constrain activity |
| 6. Integration and rollout | API DTOs/OpenAPI/client, `webapp/ui/src/types.ts` and panels, durable subscriptions, metrics, backup/restore, migration and bounded load tests | Real game client completes scenario; privacy and overload tests pass; restore drill reconciles; UI assets regenerated with `just ui` |

Deliver phases in dependency order. Phases 1–3 establish the token service; 4–6 complete the requested game economy. Do not call the wallet-only milestone a complete economy engine.

Phase 3 is the riskiest phase and phases 1–2 deliver little on their own, so ship a minimal slice first: phase 1, phase 2, and a command journal appended before each acknowledgement over the existing version-5 snapshot path, with restart replaying the journal from the last snapshot. That slice is a usable custodial token service and validates the journaling design before SQLite, the outbox and checkpoint tables are built. Only then reshape the actor topology.

Rough order of magnitude, to be replaced by measurement after the prototype: phases 1 and 2 are days each; phase 3 and phase 4 are weeks each and dominate; phase 5 is weeks and depends on product decisions listed below; phase 6 is a week plus whatever the game client integration needs. Avoid committing to throughput or dates without measurement.

End-to-end acceptance scenario: initialize treasury supply, onboard two players, pay one verified quest reward, buy ore from a funded NPC, craft an ingot, list and sell it to the second player, pay the configured fee, and consume a purchased item. Verify every wallet, inventory, hold, fee destination and total supply. Repeat requests, overlap transfer and order attempts, and restart at each commit boundary; the economic result must remain the same. Also run depleted-treasury, exhausted-NPC, recipient-cap, freeze-during-order and duplicate-job-completion cases.

## Migration, verification and release

Treat the current snapshot as a baseline, not a full audit history: older entries are intentionally discarded. Stop writes, take a version-5 snapshot through existing validation, and import once with a unique migration ID. Issue the sum of imported cash balances as explicitly labeled migration genesis; credit those wallets with matching postings. Import holdings and reservations with declared asset allocations and match them against open orders. Seed remaining issuer/NPC inventory only from an approved outstanding-supply allocation; cancel synthetic quotes and rebuild funded ones. Preserve users, key hashes and client order responses that still exist. Do not invent evicted transaction history or claim missing client IDs are deduplicated; use a new API/world epoch for post-cutover keys.

Validate cash, holdings, orders, stops and account links before enabling writes. Map legacy freeze states conservatively and require operator review before lifting them. Recompute holds from imported orders and compare; fail migration on disagreement. Archive the original snapshot and migration report. After accepting new economy transactions, rollback requires replay/migration of those transactions into a compatible binary; restoring the old snapshot would discard acknowledged actions.

Implementation validation should extend `webapp/tests/contract.rs`, `save.rs`, `concurrency.rs` and add ledger, inventory, recovery and integration suites. Run targeted tests at each phase, preserve determinism/save-load properties, then use `just ci` for the release candidate. Benchmark with a declared target for concurrent players, economic commands/second, symbols and tick rate; report p95/p99 latency, queue depth, memory, commit latency and recovery time. Load must be bounded without dropping accepted commands.

Release requires reconciled backup restoration, zero unexplained supply drift, durable retry safety, authorization tests, and the full gameplay scenario. Outstanding product choices before tuning: reward cadence, genesis/cap policy, consumable goods and recipes, transfer restrictions, desired player scale, and whether downtime pauses production. External chain compatibility requires a separate decision before any bridge work.
