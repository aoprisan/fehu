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
    cargo test -p fehu-webapp

# clippy with -D warnings on all targets and feature sets, plus rustfmt check.
lint:
    cargo fmt --all -- --check
    cargo clippy --all-targets --all-features -- -D warnings
    cargo clippy --all-targets --no-default-features -- -D warnings
    cargo clippy -p fehu-webapp --all-targets -- -D warnings

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

# Run the sample web app (four seeded symbols, OHLC UI, game-event endpoints).
serve BIND="0.0.0.0:3000" TIME_SCALE="1":
    FEHU_BIND={{BIND}} FEHU_TIME_SCALE={{TIME_SCALE}} cargo run --release -p fehu-webapp

# Build the TypeScript UI into webapp/static. That output is committed and
# embedded in the binary, so `cargo run` needs no Node toolchain — only this.
ui:
    cd webapp/ui && npm ci && npm run build

# Vite dev server with hot reload on :5173, proxying /api to `just serve`.
ui-dev:
    cd webapp/ui && npm install && npm run dev

# Typecheck the UI and fail if the committed bundle is out of date.
ui-check:
    cd webapp/ui && npm ci && npm run build
    git diff --exit-code -- webapp/static

# Everything CI would run. `ui-check` needs Node; the Rust recipes do not.
ci: lint build test wasm ui-check
