# 02: Token, wallet and journal domain

Status: planned.

Dependencies: [01](01-authority-and-configuration.md).

## Objective and implementation

Add economy types, posting rules, typed holds and treasury/issuer/fee accounts; adapt `account.rs` and reconciliation. Implement atomic mint, burn, transfer and idempotent command semantics in pure domain tests. Define frozen/closed behavior: frozen wallets cannot initiate spending; legitimate settlement credits may arrive; closure requires zero balances and no holds/jobs; administrative freeze cannot be lifted by owner.

## Required contract

Provide checked integer asset amounts, typed wallet/asset/command/transaction IDs, balanced postings, explicit supply records and typed reservations. Mint and burn use an issuance control account; treasury, NPC and fee wallets count toward supply. Require nonnegative spendable balances and per-asset conservation. Reject the whole candidate transition on any invalid posting.

## Expected file scope

webapp/src/economy/**, webapp/src/account.rs, webapp/src/reconcile.rs, webapp/src/lib.rs, domain and property tests.

## Acceptance

Property tests conserve supply, reject overflow and overdrafts, and leave state identical on failure; shared-wallet reservations cannot double-spend.
