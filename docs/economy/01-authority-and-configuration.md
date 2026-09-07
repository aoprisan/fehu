# 01: Authority boundary and configuration

Status: planned.

Dependencies: None.

## Objective and implementation

Touch `auth.rs`, `api.rs`, `market.rs`, `main.rs`, API/contract tests and docs. Add explicit demo/authoritative mode; authoritative startup requires issuer credentials and durable storage configuration. Disable player deposits, arbitrary opening cash, self-unfreeze and unbounded signup grants. Define role/action matrix, key rotation/revocation and service provisioning. Demo mode remains explicit and unsuitable for authoritative wallets.

## Required contract

Document a role/action matrix for player, game service and issuer/admin. Audit every legacy route that can create cash or change account status. Until durable execution exists, authoritative economic writes must remain unavailable; accepting a database path alone is not a durability guarantee.

## Expected file scope

webapp/src/auth.rs, webapp/src/api.rs, webapp/src/market.rs, webapp/src/main.rs, webapp/tests/api.rs, webapp/tests/contract.rs, configuration documentation.

## Acceptance

Every money-creating alias and admin action is tested for unauthorized rejection; users cannot act for other players. No economic launch until later phases pass.
