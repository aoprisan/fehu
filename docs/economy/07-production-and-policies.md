# 07: Production, consumption and economic policies

Status: planned.

Dependencies: [06](06-goods-and-commerce.md).

## Objective and implementation

Add recipes, jobs, capacity, deterministic scheduler and budgets. Store recipe version and refund policy in each job. Default offline policy pauses simulated time at last committed time; bounded administrative catch-up is explicit, journaled and resumable. Finish outputs once even after restart/cancel races; handle output limits without losing inputs or silently dropping output. Example world includes ore -> tools, treasury-funded rewards/wages, shop stock and repair/consumption sink, with bounded NPC demand. Game semantic shocks can influence configured demand or production through explicit rules, while preserving existing price events. Give the example world concrete rates, not only mechanics: reward budget per player per day, sink rates, NPC market-making budget and shop restock, so the first game owner starts from a worked policy rather than guesses.

## Required contract

Implement recipes and production job create/read/cancel endpoints, with server-controlled completion. Persist input handling, recipe version, output quantities, capacity, finish time and cancellation/refund policy. Use explicit resource issuance/destruction postings and journaled time advancement. External game effects are delivered through idempotently consumed receipts, not an assumed cross-database transaction.

## Expected file scope

webapp/src/economy/**, webapp/src/engine.rs, webapp/src/events.rs, webapp/src/api.rs, persistence migrations, scenario configuration and scheduler tests.

## Acceptance

Seeded scenario replay produces identical jobs, holdings and prices; budget exhaustion, storage failure, full output wallet and simultaneous job cancellation/completion are covered.
