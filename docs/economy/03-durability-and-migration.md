# 03: Durable command execution and migration

Status: planned.

Dependencies: [02](02-token-wallet-and-journal.md).

## Objective and implementation

Add storage module/migrations and wire `save.rs`, `engine.rs`, `main.rs`, API execution. Commit journal, receipt, outbox and recovery inputs together before success. Order clock/config commands with player commands. Import an existing save once (format 5; formats 2-4 pass through the existing migration to 5 first) into GAME at one unit per cent; reconcile balances and inventory, record explicit genesis issuance rather than inventing historic postings, preserve credentials/IDs and archive original. Verify holds/open orders/stops and future counters; fail the whole import on inconsistency. Historical evicted entries remain explicitly unavailable. Use backup and restore procedures that include committed WAL state.

## Required contract

Use one ordered writer and SQLite WAL with synchronous=FULL. Atomically persist postings, command digest/receipt, outbox and replay inputs before acknowledging. Stage in-memory book/RNG/wallet transitions so failed commits publish nothing and change nothing. Checkpoint engine version, sequence, clock, books and RNG. Preserve retry protection beyond bounded UI history; restore and replay without duplicate effects.

## Expected file scope

webapp/src/storage/**, webapp/migrations/**, webapp/src/save.rs, webapp/src/engine.rs, webapp/src/main.rs, webapp/src/api.rs, webapp/Cargo.toml, Cargo.lock, persistence tests.

## Acceptance

Failpoints before/after commit and before response, restart, retried commands, disk-full errors and deterministic replay demonstrate no partial or duplicated acknowledged action. Old demo executable must not write the migrated store.
