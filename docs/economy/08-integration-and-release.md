# 08: Integration, balancing evidence and release gate

Status: planned.

Dependencies: [01](01-authority-and-configuration.md), [02](02-token-wallet-and-journal.md), [03](03-durability-and-migration.md), [04](04-wallet-and-game-api.md), [05](05-funded-exchange.md), [06](06-goods-and-commerce.md), [07](07-production-and-policies.md).

## Objective and implementation

Deliver OpenAPI, a runnable HTTP example and scenario fixture, operator configuration, migration/rollback instructions and economy metrics (issued/burned, treasury balances, reward/sink flow, traded goods price basket, NPC stock and job backlog). Run the full reward -> purchase -> production -> consume/sell -> transfer -> trade loop, restart midway, retry every write, and reconcile. Measure throughput and p95/p99 command latency using a documented workload and hardware; set the supported player/command budget from results rather than assuming the existing tick benchmark measures payment capacity. Keep one writer until evidence justifies partitioning. Run `just ci` with required tools; record failures or missing prerequisites honestly. UI changes also require rebuilt assets and visible-change verification. The admin economy view must split system-held supply (treasury, NPC, fee and issuer wallets) from player-held supply; the second number is the one balancing needs.

## Required contract

Complete durable /api/v1 events and stream delivery, plus admin economy/reconcile views. Use authenticated event filtering and at-least-once delivery with stable IDs, cursors and resync behavior. Validate the complete game loop and publish an executable integration example, OpenAPI, operational runbook and measured capacity limits.

## Expected file scope

webapp/tests/**, examples/**, docs/** (including docs/architecture.html), README.md, webapp/src/metrics.rs, webapp/src/reconcile.rs, OpenAPI and scenario/load fixtures.

## Acceptance

The full reward -> purchase -> production -> consume/sell -> transfer -> trade loop runs with a restart midway and every write retried, reconciliation is clean afterwards, throughput and p95/p99 latency are measured on documented hardware, and `just ci` passes or its failures and missing prerequisites are recorded.
