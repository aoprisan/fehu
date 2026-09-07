# 06: Goods and atomic commerce

Status: planned. This file describes implementation work; it does not claim the feature exists.

Dependencies: [04](04-wallet-and-game-api.md), [05](05-funded-exchange.md).

Read the [economy architecture and HTTP contract](../../ECONOMY_PLAN.md) and the [task index](README.md) before implementing. The parent plan's invariants and completion criteria apply to this task.

## Objective and implementation

Add persisted fungible asset definitions, wallet inventory, offer catalog, purchase/consume handlers and migrations. Transfer goods using the same holdings authority as market positions; do not create a second competing inventory balance. Dynamic listing uses persisted IDs; disabling trading must preserve holdings and history. Implement atomic purchase, fees and stock limits; defer NFTs and equipment metadata.

## Required contract

Implement assets, owned inventory, offers, purchases and consumptions under /api/v1. Prices and stock come from versioned server offers. Reuse wallet asset balances and holds across commerce and exchange. Payment, delivery and fees commit together. Disabling an asset market preserves ownership and historical records.

## Expected file scope

webapp/src/economy/**, webapp/src/api.rs, webapp/src/storage/**, webapp/migrations/**, API and commerce tests.

These are expected locations; inspect the current tree and dependency results before selecting exact files.

## Acceptance and verification

last-item purchase races have one winner; payment cannot succeed without item delivery; market-held goods cannot also be consumed or sold through an offer.

Run focused integration/property tests for changed behavior and record the exact commands and results. Retain Rust feature compatibility and seeded determinism. Update server DTO contracts and TypeScript types together; rebuild committed static assets if UI source changes. The final integration task runs the full release gate.

## Handoff

Report the implemented interfaces, migrations/configuration, tests and any remaining limitations for dependent tasks. Do not mark complete on the basis of this document alone. Keep the parent plan and relevant API documentation consistent with the implemented behavior.
