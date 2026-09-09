# fehu — task runner. `just` lists recipes.
#
# The workspace root is virtual, so every library recipe names `-p fehu`:
# a bare `cargo build --no-default-features` here would also build
# `fehu-economy`, whose dependency on `fehu` re-enables `std` and quietly
# defeats the no_std check.

set shell := ["bash", "-euo", "pipefail", "-c"]

default:
    @just --list

# Build the library with every feature set (std, serde, and bare no_std +
# alloc), then the economy server.
build:
    cargo build -p fehu --all-features
    cargo build -p fehu --no-default-features
    cargo build -p fehu --no-default-features --features serde
    cargo build -p fehu-economy

# Run the library suite under std + serde and again against the no_std
# library, then the economy server's suite.
test:
    cargo test -p fehu --all-features
    cargo test -p fehu --no-default-features --tests
    cargo test -p fehu-economy

# clippy with -D warnings on all targets and feature sets, plus rustfmt check.
lint:
    cargo fmt --all -- --check
    cargo clippy -p fehu --all-targets --all-features -- -D warnings
    cargo clippy -p fehu --all-targets --no-default-features -- -D warnings
    cargo clippy -p fehu-economy --all-targets -- -D warnings

# criterion benchmark: ticks/sec for the default config and with market hours.
bench:
    cargo bench -p fehu --bench ticks

# Build the library for wasm32-unknown-unknown (no_std + serde).
wasm:
    rustup target add wasm32-unknown-unknown
    cargo build -p fehu --release --target wasm32-unknown-unknown --no-default-features --features serde
    cargo build -p fehu --release --target wasm32-unknown-unknown --no-default-features

# Write one year of 1 s ticks and daily candles to CSV in OUT_DIR.
dump OUT_DIR="out" SEED="42":
    cargo run -p fehu --release --example dump -- {{OUT_DIR}} {{SEED}}

# Run the economy server (seeded symbols, OHLC UI, game-event endpoints).
serve BIND="0.0.0.0:3000" TIME_SCALE="1":
    FEHU_BIND={{BIND}} FEHU_TIME_SCALE={{TIME_SCALE}} cargo run --release -p fehu-economy

# Build the TypeScript UI into crates/fehu-economy/static. That output is
# committed and embedded in the binary, so `cargo run` needs no Node
# toolchain — only this.
ui:
    cd crates/fehu-economy/ui && npm ci && npm run build

# Vite dev server with hot reload on :5173, proxying /api to `just serve`.
ui-dev:
    cd crates/fehu-economy/ui && npm install && npm run dev

# Typecheck the UI and fail if the committed bundle is out of date.
ui-check:
    cd crates/fehu-economy/ui && npm ci && npm run build
    git diff --exit-code -- crates/fehu-economy/static

# Everything CI would run. `ui-check` needs Node; the Rust recipes do not.
ci: lint build test wasm ui-check
