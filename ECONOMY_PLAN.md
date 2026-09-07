# Fehu as a game economy engine

Assessment and implementation brief, 2026-09-07. This is a plan, not an implemented economy. Source inspection is the evidence below; no runtime or load-testing claims are made.

## Recommendation and scope

Keep `fehu` as the deterministic price and matching library and turn the Axum application into the authoritative economy service. Model currency as a centrally issued, fungible game token: wallets, integer balances, total supply, mint, burn and transfer. A blockchain is unnecessary for this server-owned authority model. An actual on-chain token would additionally require a chain, wallet signatures, custody decisions and deposit/withdrawal reconciliation; reserve that for a separately requested integration.

The token is a ledger of centrally issued credits with balance, supply and transfer semantics. It is not a crypto token and the docs should not describe it by analogy to one. Do not implement delegated allowances or arbitrary smart contracts for the first game release. Bearer credentials authorize server commands; they are not crypto wallets.

Planning defaults: one game world per server/database; one settlement token, `GAME`, with two decimal places to preserve current cents exactly; no redemption, public-chain bridge, borrowing or shorting. Token value is its purchasing power against goods and assets; do not put a random price on the sole unit of account. A second currency and exchange pair can be added later. Use configurable issuance and sink policies, not a hardcoded claim that fixed supply guarantees a balanced economy.

Deliver a complete initial loop: provision a player, pay a bounded reward, buy goods, consume inputs to produce an item, sell or transfer it, pay a fee, trade an asset, and recover all acknowledged actions after a crash. Game rules are server-configured; clients cannot declare their own rewards or production completion. These defaults allow implementation without deciding a game's entire design first. Before public launch, the game owner must supply reward budgets, recipes, prices and expected traffic; ship a reproducible example configuration in the meantime.

## Token design decisions

- **Closed by design.** No redemption, bridge or withdrawal to anything of real value. That line does more than it looks like: the moment tokens can leave the game, the simulated equities become instruments a regulator may treat as securities. Keep it.
- **Minor units are inherited, not chosen.** Two decimals exist because the simulator prices stocks in cents. Basis-point fees on small trades round to zero at that granularity, so define fee rounding direction and a minimum fee per market, or carry more minor units internally. Asset definitions record their own decimals, so decide this before the first migration; it is cheap now and expensive later.
- **Conservation counts system wallets; balancing does not.** Treasury, NPC, fee and issuer wallets are ordinary holders so that supply equals minted minus burned. The number a designer needs is player-held supply. The admin economy view reports both, split.
- **The exchange is a faucet with a budget.** Today synthetic prints conjure cash. Under this plan NPC quotes around a simulated reference price are funded, and players who trade the mean reversion will drain that fund. Treasury-paid dividends are a faucet too. Both belong in the issuance policy next to rewards, with a budget and a visible drain, not only under "liquidity". Exhausted NPC liquidity is an allowed state.
- **Ship a default policy with numbers.** The example world must carry rates, not only mechanics: reward budget per player per day, sink rates, NPC market-making budget, shop restock. Issued-versus-burned and the goods price basket exist from the first task that mints, so the first game owner sees inflation in metrics rather than in complaints.

## Current foundations and gaps

| Area | Current evidence | Required change |
|---|---|---|
| Price/matching | `src/sim.rs`, `src/exchange.rs`, `src/book.rs`; the model is documented in those modules' docs and in `docs/architecture.html` (`DESIGN.md` no longer exists) | Reuse deterministic simulation and order semantics; preserve `no_std + alloc`, seeded RNG and native/Wasm behavior. |
| Identity | `webapp/src/auth.rs` hashes player keys; `api.rs` checks account ownership and gates game-master routes (listing, delisting, dividends, halts, events, reconcile) behind a single `FEHU_ADMIN_KEY`, which leaves them open when unset | Add scoped game-service/admin principals, key lifecycle, and explicit world boundary. Ownership alone does not authorize issuance. |
| Money creation | `api.rs` lets owners deposit and supply opening cash; signup can create a funded trader | Close every faucet in authoritative mode, including alternate trader routes and repeated signup. Provision empty wallets and fund from an explicit policy/treasury. |
| Accounting | `account.rs` uses cents, reservations and capped per-account ledgers | Add a permanent balanced journal and supply records. Replace settlement saturation and dividend clipping with checked, atomic success or refusal. |
| Market counterparties | `trading.rs::apply_trade` settles trader sides; synthetic owners have no account. Fees/rebates alter player cash. `market.rs::pay_dividend` credits holders | Back counterparties, rebates and dividends with funded accounts and owned inventory. A price reduction on dividend is not a currency debit. |
| Asset model | `symbol.rs::TICKERS` seeds four symbols; `symbols.rs` lists and delists more at runtime (up to `FEHU_MAX_SYMBOLS`) by interning each ticker as a leaked `&'static str` (`save.rs::Symbol`), so a re-listed ticker inherits the old listing's history; positions belong to traders | Introduce stable asset IDs, persisted definitions, fungible inventory and account-level ownership/reservations. Keep traders as authorized market identities. |
| Durability | `save.rs` format 5 saves whole state every `FEHU_SAVE_SECS`; formats 2-4 migrate on load; retained ledgers/logs are bounded | Commit commands and effects before responding; periodic saves cannot guarantee acknowledged payments survive a crash. |
| Runtime | `engine.rs` advances market time as a job on the market actor. There are no locks (`actor.rs`, `market.rs`): the market actor is the one ordered writer for money and books, each symbol actor owns its simulator, book and RNG, and reads go straight to the symbol actor | Keep the market actor as the single ordered writer initially. Add durable scheduled production, deterministic command ordering and explicit offline-time policy. |
| Integration | `api.rs` supplies orders, semantic price shocks, SSE and reconciliation; tests cover API, contracts, saves, runtime listing and actor concurrency | Add wallet, transfer, purchase, inventory, recipe/job and durable event APIs; existing semantic events are price shocks, not goods production. |

## Architecture and invariants

Add server-side economy domain modules (`webapp/src/economy/`) and a storage adapter, leaving clocks, HTTP, credentials and database dependencies out of the core library. Use typed `AssetId`, `WalletId`, `TransactionId`, `CommandId` and versioned configuration. Persist symbol-to-asset mappings rather than interning runtime symbols into leaked static strings.

Use signed checked 64-bit token units and wider checked intermediates for price-times-quantity and supply aggregates. Retain explicit amount and supply caps. New API amounts and IDs are decimal strings, avoiding JavaScript precision loss; legacy cents adapters accept only exactly representable values. Define price as token units per asset lot, including fee rounding. Never silently saturate or truncate economic value.

Every command validates authority, funds, destination limits and all effects before commit. A transaction includes balanced postings per asset, holds, inventory changes, supply changes, command receipt and outgoing events. Mint/burn balance against a dedicated issuance control account, excluded from circulating wallet balances. Treasury, NPC, issuer and fee wallets are ordinary counted holders. Transfers and fees conserve supply; mint/burn alone change it. Reservations are a subset of ownership, not additional supply. Require `0 <= reserved <= balance` and sum of counted balances equals minted minus burned, separately for each asset. Resource production/consumption uses explicit authorized creation/destruction records with recipe provenance.

All orders sharing a wallet share the same available funds and inventory. Matching and settlement must commit together: stage a candidate book/domain transition, validate both counterparties and fees, persist it, then expose it. A failed storage commit must leave books, wallets, holds and RNG state unchanged. Do not mutate the current book and attempt a database debit afterwards.

For the initial single-server deployment, use SQLite with a single writer, WAL and `synchronous=FULL` on local durable storage. SQLite documents atomic commit and the additional WAL sync required for this durability setting: https://www.sqlite.org/atomiccommit.html and https://www.sqlite.org/pragma.html#pragma_synchronous. Verify the chosen driver and settings during implementation. Do not split money and inventory into independently committed stores.

Persist an ordered, versioned command journal (including tick targets, accepted game inputs and configuration changes), financial postings, receipts and an outbox in one transaction. Checkpoints include book state, RNG state, simulation time, scheduled jobs and last applied sequence. Recovery restores a verified checkpoint and replays committed commands with the same engine version. Read views must be rebuildable; replay suppresses duplicate publication. Full snapshots per command are acceptable only as a measured first implementation, not an assumed performance solution. Failed persistence makes authoritative writes unavailable until recovery; it must not merely log and continue.

Require `Idempotency-Key` on economic writes, scoped to world and principal, with a canonical operation/payload digest. Same key and same request returns the original receipt; same key with different content returns `409`. Store receipts beyond bounded UI/order history, and document retention without permitting an old payment key to execute again. Reward grants also have a unique game event ID so changing the request key cannot double-claim one reward.

Outbox delivery is at least once, with durable ordered event IDs and authenticated per-player filtering. Consumers deduplicate; provide cursor-based history and an explicit resync response if history is archived. Do not advertise exactly-once network delivery. Never include private balances in public market events.

## Proposed HTTP contract

All paths below are new `/api/v1` contracts; publish OpenAPI with request, response and error examples. Use owned-wallet authorization on player reads/writes, and scoped game-service/admin authorization on policy or issuance. Return transaction ID, sequence, status and affected balances on committed economic writes. Use `400` for malformed amounts, `401/403` for credentials/authority, `404` for inaccessible resources, `409` for insufficient funds, state/version or idempotency conflicts, and `503` for unavailable durable storage.

| Method/path | Authority and behavior |
|---|---|
| `POST /players` | Game service provisions unique external player ID and an empty wallet; duplicate external ID cannot earn another starter grant. |
| `GET /assets`, `GET /assets/{id}` | Definitions, decimals, trade mode and supply visibility policy. |
| `GET /wallets/{id}`, `GET /wallets/{id}/transactions?cursor=...` | Owner sees balances, holds and stable journal pagination. |
| `POST /transfers` | Owner transfers `{from_wallet_id,to_wallet_id,asset_id,amount}` atomically; destination policy checked. |
| `POST /admin/mints`, `POST /admin/burns` | Scoped issuer; asset, amount, wallet, reason and policy reference; cap checked and audited. |
| `POST /game/rewards` | Game service submits player and unique event ID plus configured reward rule; treasury debit or explicitly budgeted mint. |
| `GET /wallets/{id}/inventory` | Owner's fungible quantities and reserved/available inventory. |
| `GET /offers`, `POST /purchases` | Purchase references server offer/version and quantity; atomic token-for-goods exchange with NPC/player wallet and stock. |
| `GET /recipes`, `POST /production/jobs` | Validate known recipe, ownership, inputs, capacity and fee; consume/hold inputs exactly once, persist finish time. |
| `GET /production/jobs/{id}`, `POST /production/jobs/{id}/cancel` | Owner reads/cancels according to recorded recipe refund policy; completion cannot be supplied by client. |
| `POST /consumptions` | Server-defined item action and quantity; consume inventory and record effect receipt atomically. |
| `POST /markets/{id}/orders` and order query/cancel routes | Existing order semantics adapted to registered base asset and GAME quote wallet with durable settlement. |
| `GET /events?cursor=...`, `GET /stream` | Durable catch-up and live authorized events. |
| `GET /admin/economy`, `GET /admin/reconcile` | Supply, treasury, faucets/sinks, inventory and journal invariants. |

Example purchase: `{ "offer_id": "ore-shop", "version": "3", "wallet_id": "42", "quantity": "5" }` plus an idempotency header. The server looks up the price and stock; the client never supplies the payment amount or asserts ownership. Quests/world effects outside this server consume the outbox receipt idempotently; do not claim atomicity across an external game database.

## Ordered implementation work

Each phase is a separately implementable task after its dependencies. The briefs in [`docs/economy/`](docs/economy/README.md) hold the canonical implementation text, file scope and acceptance for each; this list is the map. Estimates are relative scope, not delivery commitments: phases 2, 3 and 5 contain the largest correctness risks.

1. **[Authority boundary and configuration](docs/economy/01-authority-and-configuration.md)** (first; medium). Explicit demo/authoritative mode, every faucet closed in authoritative mode, a role/action matrix and key lifecycle.
2. **[Token, wallet and journal domain](docs/economy/02-token-wallet-and-journal.md)** (after 1; large). Economy types, balanced postings, typed holds, issuance control and system wallets, frozen/closed semantics, property-tested conservation.
3. **[Durable command execution and migration](docs/economy/03-durability-and-migration.md)** (after 2; large). Journal, receipts, outbox and checkpoints committed together before acknowledgement; one-time import of the existing save; failpoint and replay tests.
4. **[Wallet and game-service API](docs/economy/04-wallet-and-game-api.md)** (after 3; medium). Provisioning, wallets, history, transfers, issuance and rewards under `/api/v1`, with idempotency, decimal strings and cursor pagination; supply metrics from day one.
5. **[Economically backed exchange](docs/economy/05-funded-exchange.md)** (after 3 and 4; large). Stable market/asset IDs, funded NPC liquidity instead of synthetic prints, fees, rebates and dividends from real wallets, all settled atomically.
6. **[Goods and atomic commerce](docs/economy/06-goods-and-commerce.md)** (after 4 and 5; large). Persisted asset definitions, wallet inventory shared with the exchange, offers, atomic purchase and consumption.
7. **[Production, consumption and economic policies](docs/economy/07-production-and-policies.md)** (after 6; large). Recipes, jobs, deterministic scheduler, offline-time policy, budgets, and an example world with concrete rates.
8. **[Integration, balancing evidence and release gate](docs/economy/08-integration-and-release.md)** (after 1-7; medium/large). OpenAPI, runnable example, operator docs, metrics, the full loop under restart and retry, measured capacity.

## Completion criteria and later options

The implementation task is complete only when all eight phases and the executable end-to-end loop pass, every authoritative mutation has a durable receipt, all currency and inventory reconcile, retries survive history eviction and restarts, and the docs describe deployed behavior. Existing API tests passing alone are insufficient. Add focused Rust integration/property tests and fault-injection tests; retain core determinism and feature-matrix checks.

Later options: multiple worlds/tenancy, second currencies/FX, auctions, nonfungible equipment, credit, public-chain bridging, horizontal writers and advanced macroeconomic feedback. None is necessary to demonstrate a server-run game economy. A functioning token ledger alone is also insufficient: goods, production, consumption and game-service integration are part of this plan's first complete economy.
