# 05: Economically backed exchange

Status: planned.

Dependencies: [03](03-durability-and-migration.md), [04](04-wallet-and-game-api.md).

## Objective and implementation

Touch `market.rs`, `trading.rs`, economy settlement and core exchange only where needed. Register stable market/asset IDs and migrate every listed equity (the four seeded ones plus any listed at runtime). Explicitly choose per market: pure player/NPC order flow, or simulated reference price with funded NPC quotes. In authoritative mode replace unbacked synthetic liquidity with inventory/cash-limited NPC orders; synthetic tape-only prints must never settle into player wealth. Quote volume, not only execution, must respect shared NPC holds. Route fees to fee wallet, rebates from funded budget, dividends from issuer treasury to all eligible holders atomically. Permit exhausted NPC liquidity; never silently top it up. Treat the NPC market-making budget and treasury-paid dividends as budgeted faucets in the issuance policy, not only as liquidity: players who trade the mean reversion will drain the NPC fund, and that drain must be visible and bounded.

## Required contract

Implement registered base-asset/GAME markets and the parent order/query/cancel contracts. Candidate matching, both settlement sides, inventory, holds, fees and the receipt form one atomic transition. Synthetic quotes must become funded NPC orders in authoritative markets; tape-only synthetic prints cannot change player wealth. Preserve core no_std and seeded determinism.

## Expected file scope

webapp/src/market.rs, webapp/src/trading.rs, webapp/src/economy/**, webapp/src/save.rs, src/exchange.rs and src/book.rs only as needed, trading and persistence tests.

## Acceptance

Player/player and player/NPC partial fills, fees, rebates, stops, cancellations, expiry, self/shared-account trades, insufficient issuer funds and depleted NPC stock preserve holdings and supply through restart. Existing order behavior remains covered.
