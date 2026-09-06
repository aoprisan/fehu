# fehu — task runner. `just` lists recipes.

set shell := ["bash", "-euo", "pipefail", "-c"]

default:
    @just --list

# Build with every feature set (std, serde, and bare no_std + alloc).
build:
    cargo build --all-features
    cargo build --no-default-features
    cargo build --no-default-features --features serde

# Run the test suite under std + serde and again against the no_std library.
test:
    cargo test --all-features
    cargo test --no-default-features --tests

# clippy with -D warnings on all targets and feature sets, plus rustfmt check.
lint:
    cargo fmt --all -- --check
    cargo clippy --all-targets --all-features -- -D warnings
    cargo clippy --all-targets --no-default-features -- -D warnings

# criterion benchmark: ticks/sec for the default config and with market hours.
bench:
    cargo bench --bench ticks

# Build the library for wasm32-unknown-unknown (no_std + serde).
wasm:
    rustup target add wasm32-unknown-unknown
    cargo build --release --target wasm32-unknown-unknown --no-default-features --features serde
    cargo build --release --target wasm32-unknown-unknown --no-default-features

# Write one year of 1 s ticks and daily candles to CSV in OUT_DIR.
dump OUT_DIR="out" SEED="42":
    cargo run --release --example dump -- {{OUT_DIR}} {{SEED}}

# Everything CI would run.
ci: lint build test wasm
