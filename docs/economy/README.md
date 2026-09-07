# Economy implementation tasks

These eight Markdown briefs split the [economy implementation plan](../../ECONOMY_PLAN.md) into ordered tasks. They are repository documents, not new hird task numbers; the existing hird implementation brief is #12. No queue split is performed by adding these files.

All tasks are planned. The target is one server-authoritative game world using a centrally issued GAME token with two decimals, durable accounting, funded markets, goods, production and consumption. A token ledger alone does not complete the economy.

## Task order

| Task | Depends on |
|---|---|
| [01: Authority boundary and configuration](01-authority-and-configuration.md) | None |
| [02: Token, wallet and journal domain](02-token-wallet-and-journal.md) | 01 |
| [03: Durable command execution and migration](03-durability-and-migration.md) | 02 |
| [04: Wallet and game-service API](04-wallet-and-game-api.md) | 03 |
| [05: Economically backed exchange](05-funded-exchange.md) | 03, 04 |
| [06: Goods and atomic commerce](06-goods-and-commerce.md) | 04, 05 |
| [07: Production, consumption and economic policies](07-production-and-policies.md) | 06 |
| [08: Integration, balancing evidence and release gate](08-integration-and-release.md) | 01, 02, 03, 04, 05, 06, 07 |

The dependencies form a sequential implementation path; some rows list earlier dependencies explicitly to identify the required interfaces. Complete and verify each dependency before implementing its successor.

## Shared requirements

- Follow the parent plan's architecture, currency/inventory invariants, HTTP contracts and migration policy.
- Preserve the core library's no_std + alloc support and seeded native/Wasm determinism.
- Every authoritative mutation must be authorized, checked, atomic, durable and safe to retry.
- Keep credentials and player financial data out of public events.
- Use the final task to prove the complete reward -> purchase -> production -> consume/sell -> transfer -> trade loop, including restart and retry scenarios.

Read the parent plan for the source assessment, rationale, defaults and deferred options. These briefs add implementation boundaries and handoff requirements without replacing that design.
