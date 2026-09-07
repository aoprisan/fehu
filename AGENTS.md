# Repository Guidelines

## Project Structure & Module Organization

- `src/` contains the Rust 2024 `fehu` library: simulation, events, candles, configuration, and the exchange/order book. `src/lib.rs` defines public exports.
- `tests/` holds library integration and property tests; `benches/ticks.rs` contains Criterion benchmarks; `examples/dump.rs` exports CSV data.
- `webapp/src/` contains the Axum/Tokio server; `webapp/tests/` covers API contracts, persistence, trading, and concurrency.
- `webapp/ui/src/` contains the framework-free TypeScript UI, with `panels/` and `chart/` modules. `webapp/static/` holds committed build output embedded in the server binary.
- `docs/architecture.html` illustrates actor interactions.

## Build, Test, and Development Commands

Run commands from the repository root; `just` lists available recipes.

- `just build`: build the library across default, serde, and `no_std` feature configurations.
- `just test`: run library tests across feature configurations and backend tests.
- `just lint`: check rustfmt and run Clippy with warnings treated as errors.
- `just serve`: run the backend on port 3000; `just ui-dev` starts Vite on port 5173 with API proxying.
- `just ui`: install UI dependencies, typecheck, and regenerate committed assets. Include those assets with UI changes.
- `just bench`: run tick-performance benchmarks.
- `just ci`: run lint, builds, tests, WebAssembly builds, and UI bundle consistency checks; requires Rust, Node/npm, and just.

## Coding Style & Naming Conventions

Use rustfmt’s four-space Rust formatting, `snake_case` functions/modules, and `PascalCase` types. Document public APIs; unsafe code is forbidden. Preserve `no_std + alloc` support, seeded determinism, and `libm` transcendental math. Represent monetary amounts as integer cents.

Follow existing TypeScript style: two-space indentation, single quotes, semicolons, and strict types. Keep state mutations in `actions.ts` and rendering in subscribed panels. Update `webapp/ui/src/types.ts` alongside server DTO changes.

## Testing Guidelines

Use descriptive `snake_case` test names in behavior-focused integration files. Rust tests use the standard harness, Tokio for async cases, and proptest for generated inputs. Preserve property-test regression seeds. Cover determinism, save/load equivalence, and account/order invariants when affected. Run targeted suites with `cargo test --all-features --test determinism` or `cargo test -p fehu-webapp --test contract`. No numerical coverage threshold is configured.

## Commit & Pull Request Guidelines

History favors concise, descriptive subjects, often prefixed by scope (`webapp:`, `book:`, `docs:`). Follow that pattern. PRs should explain the behavior change, link relevant issues, and report validation performed; include screenshots for visible UI changes.
