# fehu market UI

The front end for the sample server in `webapp/`. TypeScript, built with Vite,
no UI framework.

## Working on it

```sh
just serve      # the Rust backend on :3000
just ui-dev     # Vite on :5173 with hot reload, proxying /api and /api/stream
```

Open <http://localhost:5173>. When you are done:

```sh
just ui         # typecheck, then build into ../static
```

`webapp/static/` is **committed build output**, embedded into the binary by
`include_str!` in `webapp/src/api.rs`. That keeps `cargo run -p fehu-webapp` a
single self-contained command with no Node toolchain, at the cost of having to
rebuild before you commit — `just ui-check` fails if you forget, and runs in
`just ci`. Asset names are fixed (`assets/app.js`, `assets/app.css`) rather
than content-hashed, because hashed names cannot be named by `include_str!`
and would churn the committed tree on every build.

## Layout

| Path | What it is |
| --- | --- |
| `src/types.ts` | The HTTP/SSE contract: one declaration per server DTO |
| `src/api.ts` | Typed fetch client; throws `ApiError` with the server's code |
| `src/store.ts` | The application state and a topic-keyed subscription |
| `src/actions.ts` | The only writer: fetches, stream application, commands |
| `src/stream.ts` | The SSE client, dispatching on the message tag |
| `src/chart/` | `layout.ts` is pure geometry; `chart.ts` draws to the canvas |
| `src/panels/` | One module per region of the screen |
| `src/main.ts` | Boot: build the store, wire the panels, open the stream |

Data flows one way. `actions` mutate the store and emit topics; panels
subscribe to the topics they draw from and re-render themselves. Nothing
renders straight from a fetch response, so the stream and the REST endpoints
cannot disagree about what is on screen.

There is no framework. Views are already functions of state, so adding one
later is a per-panel change rather than a rewrite; the 51 kB bundle
(17 kB gzipped) is the reason not to add one yet.

The panels are the market's — symbols, chart, book, tape, ticket, account,
events — plus one for the economy: `panels/economy.ts` draws the wallet the
player's money is in, the units they hold of the world's goods, and what
they have in the furnace. The three are one panel because a job spends all
three at once — it takes units and cents now and gives back units later —
so three separate panels would always be read as one.

`panels/ops.ts` is the operator's, and the odd one out. It is a full-page
overlay rather than a page of its own — one bundle, named by `include_str!`,
is the whole reason — opened from the header or by the `#economy` hash, and
it is the only panel that polls: `GET /api/overview` and `GET /api/health`
while it is open, nothing while it is closed. `/api/reconcile` snapshots the
whole market, so it is a button. Rates are the panel's own arithmetic over
the samples it keeps in the store, because the server counts and does not
remember. Its chart draws each series' *change* since the first reading, on
one axis, in a three-hue ramp of its own (`--series-1..3` in `styles.css`)
that is kept apart from `--up`/`--down`: those mean a price moved.

## Keeping the types honest

`src/types.ts` mirrors the `#[derive(Serialize)]` types in `webapp/src/` and
the parts of the `fehu` crate that cross the wire. It is hand-written, and
`webapp/tests/contract.rs` pins the exact JSON key set of every response, so
renaming a field in Rust fails `cargo test -p fehu-webapp` with a message
naming this file — rather than rendering `undefined` in a browser.

When a contract test fails, change the Rust type and `src/types.ts` together.
