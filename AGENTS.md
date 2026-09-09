# Repository Guidelines

## Project Structure & Module Organization

The repository is a virtual cargo workspace with two members.

- `crates/fehu/src/` contains the Rust 2024 `fehu` utilities library: simulation, events, candles, configuration, the currency ledger, and the exchange/order book. `crates/fehu/src/lib.rs` defines public exports.
- `crates/fehu/tests/` holds library integration and property tests; `crates/fehu/benches/ticks.rs` contains Criterion benchmarks; `crates/fehu/examples/dump.rs` exports CSV data.
- `crates/fehu-economy/src/` contains the Axum/Tokio economy server; `crates/fehu-economy/tests/` covers API contracts, persistence, the economy, trading, and concurrency.
- `crates/fehu-economy/ui/src/` contains the framework-free TypeScript UI, with `panels/` and `chart/` modules. `crates/fehu-economy/static/` holds committed build output embedded in the server binary.
- `docs/architecture.html` illustrates actor interactions.

Because the workspace root is virtual, library commands name `-p fehu` explicitly: a bare `--no-default-features` build would also build the server, which re-enables `std`.

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

Follow existing TypeScript style: two-space indentation, single quotes, semicolons, and strict types. Keep state mutations in `actions.ts` and rendering in subscribed panels. Update `crates/fehu-economy/ui/src/types.ts` alongside server DTO changes.

## Testing Guidelines

Use descriptive `snake_case` test names in behavior-focused integration files. Rust tests use the standard harness, Tokio for async cases, and proptest for generated inputs. Preserve property-test regression seeds. Cover determinism, save/load equivalence, and account/order invariants when affected. Run targeted suites with `cargo test -p fehu --all-features --test determinism` or `cargo test -p fehu-economy --test contract`. No numerical coverage threshold is configured.

## Commit & Pull Request Guidelines

History favors concise, descriptive subjects, often prefixed by scope (`economy:`, `book:`, `docs:`). Follow that pattern. PRs should explain the behavior change, link relevant issues, and report validation performed; include screenshots for visible UI changes.
