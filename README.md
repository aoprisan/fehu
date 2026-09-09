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
docs/                   architecture animation, economy engine plan
```

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
