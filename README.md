# fehu

A deterministic synthetic market, and a game economy server built on it.

The repository is a cargo workspace of two crates. The first is the library
— the utilities a game needs to have a market at all, portable and
bit-reproducible. The second is the server that runs one.

| Crate | What it is |
|---|---|
| [`crates/fehu`](crates/fehu) | The utilities library: a deterministic synthetic stock price simulator, a limit order book, a conserved currency ledger, and an exchange that composes them. `no_std + alloc`, bit-identical on native and WebAssembly. This is the published crate. |
| [`crates/fehu-economy`](crates/fehu-economy) | The economy server binary: an axum backend that runs several `fehu` symbols in wall-clock time behind an HTTP/SSE API, with accounts, goods, jobs, rewards, a command journal and a browser UI. Not published. |

```sh
just serve                          # the economy server on :3000
cargo run --release -p fehu-economy # the same thing without just
```

The library's README is its rustdoc, so its examples are compiled as
doctests; the server's README is the reference for the whole HTTP surface
and every `FEHU_*` environment variable.

## Layout

```text
crates/fehu/            the utilities library
  src/                  sim, book, ledger, exchange, candles, time, math
  tests/ benches/ examples/
crates/fehu-economy/    the economy server binary
  src/                  actors, journal, market, HTTP API
  tests/                contract, economy, journal, load, …
  ui/                   TypeScript front-end (Vite)
  static/               committed UI build output, embedded in the binary
docs/                   the world to play, the live demo, the architecture animation
```

## Watching it work

Three pages in [`docs/`](docs), all self-contained — open any of them in a
browser, no server needed.

| Page | What it does |
|---|---|
| [`docs/world.html`](docs/world.html) | **A world you can play.** The economy as a loop: the treasury funds the merchants, the merchants pay for finished goods, the seam sells ore and the takings come home — animated, coin by coin and unit by unit. A game runs on top of it. You hold one hand, the world's own trader holds the other, and it plays the same four moves against the same clock and the same events. |
| [`docs/demo.html`](docs/demo.html) | **A server you can watch.** A miniature of the economy server runs in the page: actors, the command journal, the ledger, the books and the world. Push it around, then crash it and watch a snapshot plus the journal put it back — with the state hash taken before and after. |
| [`docs/architecture.html`](docs/architecture.html) | **Where an order goes.** One request at a time, hop by hop, through the router, the market actor and a symbol's book. |

## Tooling

`just` (see the [`justfile`](justfile)) is the entry point; `just ci` is
everything CI runs.

```sh
just build   # the library across std, no_std and no_std+serde, then the server
just test    # the same feature sets, then the server's suite
just lint    # rustfmt --check and clippy -D warnings over every feature set
just wasm    # wasm32-unknown-unknown build of the library
just bench   # criterion, ticks/sec
just serve   # run the economy server
just ui      # rebuild the committed UI bundle (commit the result)
```

## License

MIT OR Apache-2.0.
