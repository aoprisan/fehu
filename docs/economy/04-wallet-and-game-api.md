# 04: Wallet and game-service API

Status: planned. This file describes implementation work; it does not claim the feature exists.

Dependencies: [03](03-durability-and-migration.md).

Read the [economy architecture and HTTP contract](../../ECONOMY_PLAN.md) and the [task index](README.md) before implementing. The parent plan's invariants and completion criteria apply to this task.

## Objective and implementation

Implement provisioning, wallet/history, transfer, issuer and reward endpoints in the parent HTTP contract with stable errors, decimal-string contracts, cursor pagination, rate/work limits and revocable scoped credentials. Persist reward event uniqueness independently of HTTP keys. Update `webapp/ui/src/types.ts` where DTOs change and rebuild committed static files if UI source changes.

## Required contract

Implement /api/v1 players, assets, wallets, wallet transaction history, transfers, admin mints/burns and game rewards from the parent HTTP contract. Economic writes require Idempotency-Key; amounts and IDs are decimal strings. Rewards additionally deduplicate the original game event ID. Return durable receipts and stable errors; enforce ownership on pagination as well as individual reads.

## Expected file scope

webapp/src/api.rs, webapp/src/auth.rs, webapp/src/economy/**, webapp/tests/api.rs, webapp/tests/contract.rs, OpenAPI, webapp/ui/src/types.ts and webapp/static/** when needed.

These are expected locations; inspect the current tree and dependency results before selecting exact files.

## Acceptance and verification

concurrent competing transfers, lost-response retries, different-payload key reuse, cross-player reads, revoked keys and repeated grants all have regression tests.

Run focused integration/property tests for changed behavior and record the exact commands and results. Retain Rust feature compatibility and seeded determinism. Update server DTO contracts and TypeScript types together; rebuild committed static assets if UI source changes. The final integration task runs the full release gate.

## Handoff

Report the implemented interfaces, migrations/configuration, tests and any remaining limitations for dependent tasks. Do not mark complete on the basis of this document alone. Keep the parent plan and relevant API documentation consistent with the implemented behavior.
