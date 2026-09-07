# Economy implementation tasks

These eight briefs split the [economy implementation plan](../../ECONOMY_PLAN.md) into ordered tasks. The plan holds the assessment, rationale, defaults, invariants, HTTP contract and deferred options; each brief holds the implementation text for one task and is the canonical version of it.

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

## How to use a brief

Every brief is planned work: it describes what to build and does not claim the feature exists. Read the parent plan and this index before starting one; the plan's invariants and completion criteria apply to every task.

- **Expected file scope** lists likely locations. Inspect the current tree and the results of the dependency tasks before choosing exact files.
- **Acceptance** is the behavior the task must demonstrate. Run focused integration and property tests for changed behavior and record the exact commands and results. Retain Rust feature compatibility and seeded determinism. Update server DTO contracts and TypeScript types together, and rebuild committed static assets if UI source changes. The final task runs the full release gate.
- **Handoff.** On finishing, report the implemented interfaces, migrations and configuration, the tests run and any remaining limitations for dependent tasks. Do not mark a task complete on the basis of its brief alone. Keep the parent plan and the API documentation consistent with the implemented behavior.
