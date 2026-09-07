# 02: Token, wallet and journal domain

Status: planned. This file describes implementation work; it does not claim the feature exists.

Dependencies: [01](01-authority-and-configuration.md).

Read the [economy architecture and HTTP contract](../../ECONOMY_PLAN.md) and the [task index](README.md) before implementing. The parent plan's invariants and completion criteria apply to this task.

## Objective and implementation

Add economy types, posting rules, typed holds and treasury/issuer/fee accounts; adapt `account.rs` and reconciliation. Implement atomic mint, burn, transfer and idempotent command semantics in pure domain tests. Define frozen/closed behavior: frozen wallets cannot initiate spending; legitimate settlement credits may arrive; closure requires zero balances and no holds/jobs; administrative freeze cannot be lifted by owner.

## Required contract

Provide checked integer asset amounts, typed wallet/asset/command/transaction IDs, balanced postings, explicit supply records and typed reservations. Mint and burn use an issuance control account; treasury, NPC and fee wallets count toward supply. Require nonnegative spendable balances and per-asset conservation. Reject the whole candidate transition on any invalid posting.

## Expected file scope

webapp/src/economy/**, webapp/src/account.rs, webapp/src/reconcile.rs, webapp/src/lib.rs, domain and property tests.

These are expected locations; inspect the current tree and dependency results before selecting exact files.

## Acceptance and verification

property tests conserve supply, reject overflow and overdrafts, and leave state identical on failure; shared-wallet reservations cannot double-spend.

Run focused integration/property tests for changed behavior and record the exact commands and results. Retain Rust feature compatibility and seeded determinism. Update server DTO contracts and TypeScript types together; rebuild committed static assets if UI source changes. The final integration task runs the full release gate.

## Handoff

Report the implemented interfaces, migrations/configuration, tests and any remaining limitations for dependent tasks. Do not mark complete on the basis of this document alone. Keep the parent plan and relevant API documentation consistent with the implemented behavior.
