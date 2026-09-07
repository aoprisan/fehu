# 04: Wallet and game-service API

Status: planned.

Dependencies: [03](03-durability-and-migration.md).

## Objective and implementation

Implement provisioning, wallet/history, transfer, issuer and reward endpoints in the parent HTTP contract with stable errors, decimal-string contracts, cursor pagination, rate/work limits and revocable scoped credentials. Persist reward event uniqueness independently of HTTP keys. Update `webapp/ui/src/types.ts` where DTOs change and rebuild committed static files if UI source changes. Expose issued, burned, treasury and player-held supply on the admin economy view in this task, as soon as tokens exist, rather than waiting for the release gate.

## Required contract

Implement /api/v1 players, assets, wallets, wallet transaction history, transfers, admin mints/burns and game rewards from the parent HTTP contract. Economic writes require Idempotency-Key; amounts and IDs are decimal strings. Rewards additionally deduplicate the original game event ID. Return durable receipts and stable errors; enforce ownership on pagination as well as individual reads.

## Expected file scope

webapp/src/api.rs, webapp/src/auth.rs, webapp/src/economy/**, webapp/tests/api.rs, webapp/tests/contract.rs, OpenAPI, webapp/ui/src/types.ts and webapp/static/** when needed.

## Acceptance

Concurrent competing transfers, lost-response retries, different-payload key reuse, cross-player reads, revoked keys and repeated grants all have regression tests.
