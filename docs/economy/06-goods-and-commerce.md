# 06: Goods and atomic commerce

Status: planned.

Dependencies: [04](04-wallet-and-game-api.md), [05](05-funded-exchange.md).

## Objective and implementation

Add persisted fungible asset definitions, wallet inventory, offer catalog, purchase/consume handlers and migrations. Transfer goods using the same holdings authority as market positions; do not create a second competing inventory balance. Dynamic listing uses persisted IDs; disabling trading must preserve holdings and history. Implement atomic purchase, fees and stock limits; defer NFTs and equipment metadata.

## Required contract

Implement assets, owned inventory, offers, purchases and consumptions under /api/v1. Prices and stock come from versioned server offers. Reuse wallet asset balances and holds across commerce and exchange. Payment, delivery and fees commit together. Disabling an asset market preserves ownership and historical records.

## Expected file scope

webapp/src/economy/**, webapp/src/api.rs, webapp/src/storage/**, webapp/migrations/**, API and commerce tests.

## Acceptance

Last-item purchase races have one winner; payment cannot succeed without item delivery; market-held goods cannot also be consumed or sold through an offer.
