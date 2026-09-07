# 03: Durable command execution and migration

Status: planned. This file describes implementation work; it does not claim the feature exists.

Dependencies: [02](02-token-wallet-and-journal.md).

Read the [economy architecture and HTTP contract](../../ECONOMY_PLAN.md) and the [task index](README.md) before implementing. The parent plan's invariants and completion criteria apply to this task.

## Objective and implementation

Add storage module/migrations and wire `save.rs`, `engine.rs`, `main.rs`, API execution. Commit journal, receipt, outbox and recovery inputs together before success. Order clock/config commands with player commands. Import an existing v4 save once into GAME at one unit per cent; reconcile balances and inventory, record explicit genesis issuance rather than inventing historic postings, preserve credentials/IDs and archive original. Verify holds/open orders/stops and future counters; fail the whole import on inconsistency. Historical evicted entries remain explicitly unavailable. Use backup and restore procedures that include committed WAL state.

## Required contract

Use one ordered writer and SQLite WAL with synchronous=FULL. Atomically persist postings, command digest/receipt, outbox and replay inputs before acknowledging. Stage in-memory book/RNG/wallet transitions so failed commits publish nothing and change nothing. Checkpoint engine version, sequence, clock, books and RNG. Preserve retry protection beyond bounded UI history; restore and replay without duplicate effects.

## Expected file scope

webapp/src/storage/**, webapp/migrations/**, webapp/src/save.rs, webapp/src/engine.rs, webapp/src/main.rs, webapp/src/api.rs, webapp/Cargo.toml, Cargo.lock, persistence tests.

These are expected locations; inspect the current tree and dependency results before selecting exact files.

## Acceptance and verification

failpoints before/after commit and before response, restart, retried commands, disk-full errors and deterministic replay demonstrate no partial or duplicated acknowledged action. Old demo executable must not write the migrated store.

Run focused integration/property tests for changed behavior and record the exact commands and results. Retain Rust feature compatibility and seeded determinism. Update server DTO contracts and TypeScript types together; rebuild committed static assets if UI source changes. The final integration task runs the full release gate.

## Handoff

Report the implemented interfaces, migrations/configuration, tests and any remaining limitations for dependent tasks. Do not mark complete on the basis of this document alone. Keep the parent plan and relevant API documentation consistent with the implemented behavior.
