# 07: Production, consumption and economic policies

Status: planned. This file describes implementation work; it does not claim the feature exists.

Dependencies: [06](06-goods-and-commerce.md).

Read the [economy architecture and HTTP contract](../../ECONOMY_PLAN.md) and the [task index](README.md) before implementing. The parent plan's invariants and completion criteria apply to this task.

## Objective and implementation

Add recipes, jobs, capacity, deterministic scheduler and budgets. Store recipe version and refund policy in each job. Default offline policy pauses simulated time at last committed time; bounded administrative catch-up is explicit, journaled and resumable. Finish outputs once even after restart/cancel races; handle output limits without losing inputs or silently dropping output. Example world includes ore -> tools, treasury-funded rewards/wages, shop stock and repair/consumption sink, with bounded NPC demand. Game semantic shocks can influence configured demand or production through explicit rules, while preserving existing price events.

## Required contract

Implement recipes and production job create/read/cancel endpoints, with server-controlled completion. Persist input handling, recipe version, output quantities, capacity, finish time and cancellation/refund policy. Use explicit resource issuance/destruction postings and journaled time advancement. External game effects are delivered through idempotently consumed receipts, not an assumed cross-database transaction.

## Expected file scope

webapp/src/economy/**, webapp/src/engine.rs, webapp/src/events.rs, webapp/src/api.rs, persistence migrations, scenario configuration and scheduler tests.

These are expected locations; inspect the current tree and dependency results before selecting exact files.

## Acceptance and verification

seeded scenario replay produces identical jobs, holdings and prices; budget exhaustion, storage failure, full output wallet and simultaneous job cancellation/completion are covered.

Run focused integration/property tests for changed behavior and record the exact commands and results. Retain Rust feature compatibility and seeded determinism. Update server DTO contracts and TypeScript types together; rebuild committed static assets if UI source changes. The final integration task runs the full release gate.

## Handoff

Report the implemented interfaces, migrations/configuration, tests and any remaining limitations for dependent tasks. Do not mark complete on the basis of this document alone. Keep the parent plan and relevant API documentation consistent with the implemented behavior.
