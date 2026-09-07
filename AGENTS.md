# Repository Guidelines

## Project Structure & Module Organization

- `src/` contains the Rust `fehu` library: simulation, math, events, candles, time, configuration, order book, and exchange modules.
- `tests/` holds library integration and property tests; `benches/ticks.rs` contains Criterion benchmarks; `examples/dump.rs` exports CSV data.
- `webapp/src/` implements the Axum server; `webapp/tests/` covers API behavior, JSON contracts, and persistence.
- `webapp/ui/src/` contains the framework-free TypeScript UI, with `panels/` and `chart/` modules. `webapp/static/` holds committed assets embedded in the server binary.
- Read `README.md` for usage and `DESIGN.md` for model equations and architectural decisions.

## Build, Test, and Development Commands

Run recipes from the repository root:

- `just build`: build the library with standard, bare `no_std`, and serialization feature combinations.
- `just test`: run library tests with and without default features, plus backend tests.
- `just lint`: check rustfmt and run Clippy with warnings treated as errors.
- `just serve`: run the release backend on port 3000.
- `just ui-dev`: start Vite on port 5173; run the backend separately.
- `just ui`: install frontend dependencies, typecheck, and rebuild committed assets.
- `just ci`: run lint, builds, tests, WebAssembly builds, and UI freshness checks; requires Node and Rust tooling.
- `just bench`: run tick-throughput benchmarks.

## Coding Style & Naming Conventions

Use Rust 2024 conventions: four-space indentation, `snake_case` functions/modules, and `PascalCase` types. Format with `cargo fmt --all`; document public APIs. The core library forbids unsafe code and must retain `no_std + alloc` support. Preserve seeded determinism: use existing RNG and `libm` paths rather than clocks or global randomness.

Match TypeScript's two-space indentation, single quotes, semicolons, and strict compiler settings. Keep state mutations in `actions.ts` and rendering in subscribed panels.

## Testing Guidelines

Use descriptive `snake_case` Rust tests in the relevant integration file. Add regression coverage for behavioral changes; use Proptest for input invariants. No numeric coverage threshold is configured. Run focused tests with `cargo test --all-features --test determinism` or `cargo test -p fehu-webapp`.

Update `webapp/ui/src/types.ts` alongside server DTO changes and JSON contract tests. Rebuild and commit `webapp/static/` after UI changes.

## Commit & Pull Request Guidelines

Follow history's concise, scoped subjects, such as `webapp: keep the market across a restart` or `docs: clarify simulation limits`. PRs should explain behavior changes, report validation, link relevant issues, and include screenshots for visible UI changes.
