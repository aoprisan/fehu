# 01: Authority boundary and configuration

Status: planned. This file describes implementation work; it does not claim the feature exists.

Dependencies: None.

Read the [economy architecture and HTTP contract](../../ECONOMY_PLAN.md) and the [task index](README.md) before implementing. The parent plan's invariants and completion criteria apply to this task.

## Objective and implementation

Touch `auth.rs`, `api.rs`, `market.rs`, `main.rs`, API/contract tests and docs. Add explicit demo/authoritative mode; authoritative startup requires issuer credentials and durable storage configuration. Disable player deposits, arbitrary opening cash, self-unfreeze and unbounded signup grants. Define role/action matrix, key rotation/revocation and service provisioning. Demo mode remains explicit and unsuitable for authoritative wallets.

## Required contract

Document a role/action matrix for player, game service and issuer/admin. Audit every legacy route that can create cash or change account status. Until durable execution exists, authoritative economic writes must remain unavailable; accepting a database path alone is not a durability guarantee.

## Expected file scope

webapp/src/auth.rs, webapp/src/api.rs, webapp/src/market.rs, webapp/src/main.rs, webapp/tests/api.rs, webapp/tests/contract.rs, configuration documentation.

These are expected locations; inspect the current tree and dependency results before selecting exact files.

## Acceptance and verification

every money-creating alias and admin action is tested for unauthorized rejection; users cannot act for other players. No economic launch until later phases pass.

Run focused integration/property tests for changed behavior and record the exact commands and results. Retain Rust feature compatibility and seeded determinism. Update server DTO contracts and TypeScript types together; rebuild committed static assets if UI source changes. The final integration task runs the full release gate.

## Handoff

Report the implemented interfaces, migrations/configuration, tests and any remaining limitations for dependent tasks. Do not mark complete on the basis of this document alone. Keep the parent plan and relevant API documentation consistent with the implemented behavior.
