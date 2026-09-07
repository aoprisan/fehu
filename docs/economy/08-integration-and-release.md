# 08: Integration, balancing evidence and release gate

Status: planned. This file describes implementation work; it does not claim the feature exists.

Dependencies: [01](01-authority-and-configuration.md), [02](02-token-wallet-and-journal.md), [03](03-durability-and-migration.md), [04](04-wallet-and-game-api.md), [05](05-funded-exchange.md), [06](06-goods-and-commerce.md), [07](07-production-and-policies.md).

Read the [economy architecture and HTTP contract](../../ECONOMY_PLAN.md) and the [task index](README.md) before implementing. The parent plan's invariants and completion criteria apply to this task.

## Objective and implementation

Deliver OpenAPI, a runnable HTTP example and scenario fixture, operator configuration, migration/rollback instructions and economy metrics (issued/burned, treasury balances, reward/sink flow, traded goods price basket, NPC stock and job backlog). Run the full reward -> purchase -> production -> consume/sell -> transfer -> trade loop, restart midway, retry every write, and reconcile. Measure throughput and p95/p99 command latency using a documented workload and hardware; set the supported player/command budget from results rather than assuming the existing tick benchmark measures payment capacity. Keep one writer until evidence justifies partitioning. Run `just ci` with required tools; record failures or missing prerequisites honestly. UI changes also require rebuilt assets and visible-change verification.

## Required contract

Complete durable /api/v1 events and stream delivery, plus admin economy/reconcile views. Use authenticated event filtering and at-least-once delivery with stable IDs, cursors and resync behavior. Validate the complete game loop and publish an executable integration example, OpenAPI, operational runbook and measured capacity limits.

## Expected file scope

webapp/tests/**, examples/**, docs/**, README.md, DESIGN.md, webapp/src/metrics.rs, webapp/src/reconcile.rs, OpenAPI and scenario/load fixtures.

These are expected locations; inspect the current tree and dependency results before selecting exact files.

## Acceptance and verification

Complete all specified deliverables and release checks.

Run focused integration/property tests for changed behavior and record the exact commands and results. Retain Rust feature compatibility and seeded determinism. Update server DTO contracts and TypeScript types together; rebuild committed static assets if UI source changes. The final integration task runs the full release gate.

## Handoff

Report the implemented interfaces, migrations/configuration, tests and any remaining limitations for dependent tasks. Do not mark complete on the basis of this document alone. Keep the parent plan and relevant API documentation consistent with the implemented behavior.
