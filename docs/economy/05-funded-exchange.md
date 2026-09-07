# 05: Economically backed exchange

Status: planned. This file describes implementation work; it does not claim the feature exists.

Dependencies: [03](03-durability-and-migration.md), [04](04-wallet-and-game-api.md).

Read the [economy architecture and HTTP contract](../../ECONOMY_PLAN.md) and the [task index](README.md) before implementing. The parent plan's invariants and completion criteria apply to this task.

## Objective and implementation

Touch `market.rs`, `trading.rs`, economy settlement and core exchange only where needed. Register stable market/asset IDs and migrate every listed equity (the four seeded ones plus any listed at runtime). Explicitly choose per market: pure player/NPC order flow, or simulated reference price with funded NPC quotes. In authoritative mode replace unbacked synthetic liquidity with inventory/cash-limited NPC orders; synthetic tape-only prints must never settle into player wealth. Quote volume, not only execution, must respect shared NPC holds. Route fees to fee wallet, rebates from funded budget, dividends from issuer treasury to all eligible holders atomically. Permit exhausted NPC liquidity; never silently top it up.

## Required contract

Implement registered base-asset/GAME markets and the parent order/query/cancel contracts. Candidate matching, both settlement sides, inventory, holds, fees and the receipt form one atomic transition. Synthetic quotes must become funded NPC orders in authoritative markets; tape-only synthetic prints cannot change player wealth. Preserve core no_std and seeded determinism.

## Expected file scope

webapp/src/market.rs, webapp/src/trading.rs, webapp/src/economy/**, webapp/src/save.rs, src/exchange.rs and src/book.rs only as needed, trading and persistence tests.

These are expected locations; inspect the current tree and dependency results before selecting exact files.

## Acceptance and verification

player/player and player/NPC partial fills, fees, rebates, stops, cancellations, expiry, self/shared-account trades, insufficient issuer funds and depleted NPC stock preserve holdings and supply through restart. Existing order behavior remains covered.

Run focused integration/property tests for changed behavior and record the exact commands and results. Retain Rust feature compatibility and seeded determinism. Update server DTO contracts and TypeScript types together; rebuild committed static assets if UI source changes. The final integration task runs the full release gate.

## Handoff

Report the implemented interfaces, migrations/configuration, tests and any remaining limitations for dependent tasks. Do not mark complete on the basis of this document alone. Keep the parent plan and relevant API documentation consistent with the implemented behavior.
