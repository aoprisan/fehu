# Game economy engine: implementation plan

Fehu today is a synthetic stock market: a deterministic price process, a
limit order book, cash accounts and traders, behind an HTTP/SSE API. To be
the economy server for a game it needs three things it does not have: a
currency whose supply is conserved and auditable, goods that players can
produce, hold and consume, and a command journal so that every acknowledged
economic action survives a restart. Most of the rest already exists and
should be reused rather than rebuilt.

This plan is grounded in the current source (webapp save version 5). It is
written in two tiers so that a playable economy does not wait on
production-grade storage.

## Decisions taken and questions left open

Assumptions this plan is built on. Each is a default, not a requirement
inferred from a particular game.

- **One world per server, one currency.** The currency is called GAME and is
  counted in integer cents, exactly as cash is today. Multiple currencies
  and cross-world transfers are extensions, so ids carry a currency field
  from the start but only one currency is implemented.
- **Server custody, no chain.** The server is the only ledger. There is no
  wallet key material, no external chain and no redemption for real money.
- **A trusted game backend.** Rewards, quest results and inventory grants
  come from a service credential, never from a player request. Fehu owns
  currency, inventory, jobs and markets; the game owns everything else and
  submits results.
- **Two deployment tiers.** *Tier one* is one host, one process, the game
  backend on the same network, and a journal-plus-snapshot on local disk.
  That is what the repository's own defaults point at (an unset admin key is
  documented as what "a single-player game on localhost wants"). *Tier two*
  is a live service: crash-safe commits in SQLite, an event outbox, quotas
  and load tests. Tier two is listed at the end and is not on the path to a
  playable economy.
- **One market mode.** Funded markets replace synthetic ones on the server.
  The demo becomes an economy with an operator faucet and well-funded NPC
  merchants. The synthetic ladder and prints stay in the library crate for
  the determinism tests and for anyone using `fehu` as a bare simulator.

Open product choices that gate tuning, not implementation: reward cadence
and sizes, genesis supply and mint cap, the initial goods and recipes,
whether players may transfer currency to each other, target player count,
and whether production pauses while the server is down. Implement with
configuration for each and decide later with measurements.

## The token question

The request asks whether the currency can be modelled as a centralised
crypto token. The parts of the token model worth taking are exactly three:

1. an explicit **supply**, where mint and burn are the only operations that
   change it and both require operator authority and a recorded reason;
2. every movement as a set of signed **postings that sum to zero**, so the
   ledger is conserved by construction and can be audited by summing;
3. a plain **balance, supply and transfer** interface, which is what the
   ERC-20 surface amounts to once signatures and allowances are removed.

Everything else in the token model is a cost with no gameplay return here:
18 decimals, allowances, signed transactions, gas, and a chain. Listing a
symbol named COIN on the existing exchange would not do any of the three
either: it would create a traded asset priced in the current cash, not a
settlement currency. A future bridge to an external chain is a separate
project with its own custody and reorg decisions; nothing below prevents
it, and nothing below should be shaped by it.

## What exists and what changes

Evidence is from this checkout. Line references are to `webapp/src/` unless
the path says otherwise.

| Area | Today | Change |
|---|---|---|
| Money representation | `account.rs`: integer cents, `MAX_BALANCE_CENTS` of 10^15, signed ledger entries with running balance, per-account capped ledger | Keep all of it. Add a supply counter and make every movement a balanced transaction |
| Funding | `open_account` and `create_trader` take `cash_cents` from the request body; `deposit`, `withdraw` and `trader_deposit` are open to the owner | Opening balance is zero. Deposit and withdraw become operator mint and burn. Player routes cannot create or destroy currency |
| Fees | `Account::charge_fee` debits the taker; nothing is credited, so fee cents leave the system | Fees post to a venue wallet. Nothing leaves the system except through burn |
| Corporate payouts | `pay_dividend` and `pay_delisting` credit holders from nowhere and clip silently at the balance cap | Debit an issuer wallet per symbol; refuse the payout if it cannot be funded or fits nobody, never clip |
| Freeze and close | `set_status` is owner-gated, so a player can unfreeze themselves | Operator freeze and voluntary close are separate transitions with separate authority |
| Counterparties | `Market::book` settles only parties with a trader id; `reconcile.rs` says synthetic liquidity holds no inventory | Every fill has a funded trader on both sides. NPC merchants are traders with wallets and inventory |
| Assets | One kind: a stock with `shares_outstanding`, dividends and delisting | An asset kind field. Goods are assets that use the same holdings, reservations and book but have no dividends or buyout |
| Arithmetic | `notional_cents` and settlement saturate | Checked arithmetic on every authoritative path; refuse before mutating |
| Persistence | `save.rs`: whole-market snapshot, atomic rename, optional periodic autosave | Snapshot stays. Add an append-only command journal so nothing acknowledged is lost between snapshots |
| Retries | `client_order_id` is deduplicated only while the order record is retained | An idempotency key on every mutation, kept in the journal for the life of the world |
| Game events | `events.rs`: a catalogue of twelve semantic kinds mapped onto price effects, plus raw simulator events | Keep. Add effects on NPC demand and recipe yields alongside the price effects |
| Concurrency | `actor.rs`: one market actor, one actor per symbol, unbounded mailboxes | Keep the topology. Money and books already change only inside one market job; the journal write goes in that job |

Baseline: `cargo test --all-features` and `cargo test -p fehu-webapp` pass
on this checkout (see the end of this document).

## The ledger

The ledger is pure integer arithmetic with no clock, network or allocation
beyond `alloc`, so it belongs in the library crate as `src/ledger.rs`, next
to `book.rs`. That keeps the workspace at two crates and lets the property
tests run on the `no_std` path. `serde` derives sit behind the existing
`serde` feature as they do for every other library type.

```rust
// src/ledger.rs (sketch)

pub struct CurrencyId(pub u16);           // only GAME = 0 is implemented
pub struct WalletId(pub u64);

pub enum WalletKind {
    Player,     // one per account; the account's balance moves here
    Treasury,   // genesis supply lands here
    Budget,     // a finite pool a service credential may pay rewards from
    Npc,        // a merchant or producer
    Issuer,     // per symbol: funds dividends and delisting buyouts
    Venue,      // receives fees
    Issuance,   // the mint/burn control account; may go negative
}

pub struct Wallet {
    pub id: WalletId,
    pub kind: WalletKind,
    pub currency: CurrencyId,
    pub status: WalletStatus,             // Active | Frozen | Closed
    balance_cents: i64,                   // >= 0 except Issuance
    reserved_cents: i64,                  // 0 <= reserved <= balance
}

pub struct Posting {
    pub wallet: WalletId,
    pub amount_cents: i64,                // signed; the set sums to zero
}

pub enum Reason {
    Genesis, Mint, Burn, Transfer, Reward, Purchase, Fee, Rebate,
    Buy, Sell, Dividend, Delisting, JobCost, JobRefund, Migration,
}

pub struct Transaction {
    pub id: u64,                          // sequential, never reused
    pub command_id: u64,                  // the journal entry that caused it
    pub tick: u64,
    pub reason: Reason,
    pub postings: Vec<Posting>,           // 2..=N, balanced
    pub memo: Option<String>,
}

pub struct Supply {
    pub minted_cents: i64,
    pub burned_cents: i64,
}

pub struct Ledger {
    wallets: BTreeMap<WalletId, Wallet>,
    supply: Supply,
    next_tx: u64,
}

impl Ledger {
    /// Validate every posting against status, balance, reservation and cap,
    /// then apply all of them or none. Never saturates: overflow is an error.
    pub fn post(&mut self, tx: Transaction) -> Result<&Transaction, LedgerError>;
    pub fn hold(&mut self, w: WalletId, cents: i64) -> Result<HoldId, LedgerError>;
    pub fn release(&mut self, h: HoldId) -> Result<(), LedgerError>;
    /// Sum of all non-issuance balances == minted - burned.
    pub fn check(&self) -> Result<(), Vec<String>>;
}
```

Invariants `Ledger::check` enforces and the reconcile endpoint reports:

- Postings in every transaction sum to zero per currency. The sum of all
  wallet balances except issuance equals minted minus burned.
- Balances are non-negative outside the issuance wallet. Reserved is
  between zero and balance. Holds never change supply.
- A rejected transaction changes nothing. A repeated command returns its
  original result without posting again.
- A frozen wallet accepts credits, refuses debits, and has its open orders
  and stops cancelled in the same transaction that freezes it. Closing
  requires zero balance, zero holds, zero inventory and no running jobs.

Changes to `webapp/src/account.rs` once the ledger exists. `Account` keeps
its id, owner, name and the per-account ledger view; its balance becomes a
`Player` wallet in the shared ledger.

| Today | After |
|---|---|
| `Account::open(.., deposit_cents, ..)` | Opens with zero. The `cash_cents` request field is removed from `open_account` and `create_trader` |
| `Account::deposit` | `Ledger::post(Reason::Mint)` from issuance, operator only, with a reason and a configured per-world cap |
| `Account::withdraw` | `Ledger::post(Reason::Burn)` to issuance, operator only |
| `Account::settle` | Buy and sell become one transaction with postings on both traders and the venue wallet for the fee |
| `Account::charge_fee` | Folded into the settlement transaction; the fee is a posting to `Venue` |
| `Account::pay_dividend`, `pay_delisting` | One transaction per symbol: issuer wallet debited by the total, each holder credited. If the issuer cannot fund it or any holder is at the cap, the whole payout is refused with the shortfall reported |
| `Account::set_status` | Split into `freeze`/`unfreeze` (operator) and `close` (owner, with the emptiness precondition) |
| `notional_cents` saturation | Returns `Result`; a product that does not fit is refused before any state changes |
| `LedgerEntry` | Becomes a per-wallet projection of `Transaction` postings; the running balance and capped history stay for the API |

## Assets: stocks and goods share one table

Holdings, share reservations, the book and the fill path already handle
integer units of a named symbol. Goods reuse all of it. The one addition is
a kind on `SymbolInfo`:

```rust
pub enum AssetKind {
    /// Fixed supply, pays dividends, can be delisted with a buyout.
    Stock { shares_outstanding: u64 },
    /// Produced and consumed. Supply is whatever has been issued minus
    /// whatever has been consumed; nothing else creates it.
    Good { issued: u64, consumed: u64, unit: &'static str },
}
```

Rules that follow from the kind: dividend and delist routes refuse a good;
purchase and consume routes refuse a stock; the reconcile pass checks that
holdings plus resting sell reservations equal `shares_outstanding` for a
stock and `issued - consumed` for a good. Goods live in the same
`Trader::positions` map, so a trader's inventory and portfolio are the same
structure and the same holdings endpoint. Unique items, durability and land
are extensions that would need a separate non-fungible table; they are out
of scope.

Recipes and jobs are small on top of that:

```rust
pub struct Recipe {
    pub id: RecipeId, pub version: u32,
    pub inputs: Vec<(Symbol, u64)>, pub outputs: Vec<(Symbol, u64)>,
    pub cost_cents: i64, pub duration_ticks: u64,
}

pub struct Job {
    pub id: JobId, pub recipe: (RecipeId, u32), pub owner: TraderId,
    pub started_tick: u64, pub due_tick: u64, pub status: JobStatus,
}
```

Starting a job consumes the inputs and posts the cost in one transaction.
Completion issues the outputs at the due tick, from the engine step, and
happens exactly once because it is a journaled command like everything
else. Cancellation before completion refunds nothing unless the recipe says
so. Production pauses while the server is down; catch-up is a later policy
with bounded work.

## Commands and the journal

The library is deterministic, so a checkpoint plus the ordered inputs since
that checkpoint reproduces the exact state. That is the persistence design:
journal the commands, not the state changes.

```rust
// webapp/src/journal.rs (sketch)

pub enum Command {
    CreatePlayer { external_id: String },
    Mint { to: WalletId, cents: i64, reason: String },
    Burn { from: WalletId, cents: i64, reason: String },
    Transfer { from: WalletId, to: WalletId, cents: i64 },
    Reward { budget: WalletId, to: WalletId, rule: RewardRule, source_event: String },
    Purchase { buyer: TraderId, item: Symbol, qty: u64 },
    Consume { owner: TraderId, item: Symbol, qty: u64 },
    PlaceOrder(OrderRequest), Amend(..), Cancel(..), PlaceStop(..), CancelStop(..),
    StartJob { owner: TraderId, recipe: RecipeId },
    GameEvent(GameEventRequest),
    Freeze { wallet: WalletId }, Unfreeze { .. }, Close { .. },
    ListSymbol(..), Delist(..), Dividend(..),
    Step { target: Timestamp, wall_ms: i64 },   // the engine tick
}

pub struct JournalEntry {
    pub seq: u64,                 // dense, starts after the checkpoint's seq
    pub principal: PrincipalId,
    pub idempotency_key: Option<String>,
    pub payload_hash: [u8; 32],
    pub command: Command,
    pub result: CommandResult,    // the response that was acknowledged
}
```

How a mutation runs, all inside the one market job that already serialises
money and book changes:

1. Authenticate and resolve the principal. Look the idempotency key up in
   the journal index. Same key and same payload hash returns the recorded
   result. Same key and a different payload returns `409`.
2. Apply the command to the market. Every apply path validates fully before
   it mutates, so a refused command leaves nothing behind.
3. Append the entry to the journal file and `fsync`. Only then acknowledge
   and publish. If the append fails the process stops accepting mutations
   and logs the sequence it reached; the in-memory state is ahead of disk
   and must not be served as committed.
4. The periodic snapshot (`save.rs`, bumped to version 6) records the
   journal sequence it includes, and the journal is truncated at the next
   snapshot boundary.

Replay on start: load the newest snapshot, then apply journal entries after
its sequence in order. The engine tick is journaled with its wall-clock
input, so replay never reads the clock. Recipe and fee changes are commands
too, so a replayed job completes under the recipe that was current when it
started. The `client_order_id` mechanism is subsumed: it becomes the
idempotency key of `PlaceOrder`.

This is tier one. It loses at most the mutations after the last successful
`fsync`, which is none if the append is awaited before acknowledging. What
it does not provide is concurrent writers, a queryable history beyond the
retained ledger views, or an outbox for at-least-once delivery to the game
backend. Those are tier two and replace the journal file with SQLite
without changing the command model.

Determinism under funded markets: skipping the synthetic ladder and prints
does not perturb the bare price series because that flow already draws from
its own `long_jump`ed stream. Any change to the draw order of the shared
path bumps `STATE_VERSION` or `EXCHANGE_VERSION` and updates the golden
hashes in `tests/determinism.rs` in the same change, never the constants
alone.

## API

Authoritative routes live under `/api/v1/economy`. Existing routes stay
until the funded path passes the acceptance scenario, then the ones that
create currency are removed and the rest are aliased. Money stays a JSON
number: the balance cap is below 2^53, so nothing is lost in JavaScript,
and every existing DTO in `webapp/ui/src/types.ts` and the contract test
already uses numbers.

Every mutation takes an `Idempotency-Key` header. Every success returns the
transaction id and the journal sequence. Errors use stable codes:
`insufficient_funds`, `insufficient_inventory`, `budget_exhausted`,
`frozen`, `idempotency_conflict`, `overloaded`, `unauthorized`.

Principals are of three kinds and are resolved from the bearer key, never
from a body field: a **player** (today's user), a **service** with scopes
(`provision`, `reward`, `inventory`, `events`), and an **operator**
(today's admin key, now required to be set outside a demo build).

| Route | Who | Does |
|---|---|---|
| `POST /players` | service:provision | Map an external player id to a user, account, trader and zero-balance wallet. Idempotent on the external id |
| `GET /wallets/{id}`, `GET /wallets/{id}/transactions` | owner or service | Balance, reserved, available, and the retained posting history |
| `POST /transfers` | owner or service | Move available currency between two wallets in one transaction |
| `POST /rewards` | service:reward | Pay a configured reward rule from a budget wallet, keyed on the game's source event id |
| `POST /purchases` | owner or service:inventory | Charge the catalogue price and issue the good, or refuse, in one transaction |
| `POST /consume` | owner or service:inventory | Destroy units of a good the caller holds free of reservations |
| `GET /players/{id}/inventory` | owner or service | Positions in goods, with reservations |
| `POST /jobs`, `GET /jobs/{id}` | owner | Start a recipe; poll its status |
| `POST /markets/{symbol}/orders` and the existing amend, cancel and stop routes | owner | Unchanged semantics, funded on both sides |
| `POST /game-events` | service:events | The existing catalogue, journaled, with a unique source id |
| `POST /admin/mint`, `/admin/burn`, `/admin/freeze`, `/admin/unfreeze`, `/admin/recipes`, `/admin/catalog` | operator | Supply and policy changes, each with a reason string that lands in the journal |
| `GET /supply` | any authenticated | Minted, burned, treasury, budgets, player holdings, venue |
| `GET /commands/{key}` | the principal that sent it | Recover the result of a command whose response was lost |
| `GET /reconcile` | operator | The existing pass, extended to supply, inventory and jobs |

The SSE stream keeps ticks and quotes public and fills private as it does
now, but the key moves out of the query string into a header or a
short-lived stream ticket. That changes `webapp/ui/src/stream.ts` and needs
`just ui` to regenerate the committed bundle in the same commit.

## NPC merchants and producers

An NPC is a trader with a wallet funded from treasury and an inventory
issued at world start. It quotes from a policy, not from the simulator: a
merchant targets an inventory level and a margin over its cost basis, and
places ordinary limit orders through the same `PlaceOrder` command as a
player. A producer runs recipes and sells the output. The price simulator
survives as a noisy reference each NPC may read when setting its quotes, so
game events still move prices, but through NPC decisions rather than
through prints that nobody paid for.

When an NPC runs out of currency or stock, its side of the book goes empty.
That is the point: scarcity is visible, and nothing is issued implicitly.
NPC decisions run inside the engine step, so they are journaled with it and
replay identically.

## Milestones

Each milestone is shippable on its own and gates the next. Sizes are rough
and to be replaced by measurement after the first two.

| # | Scope | Done when | Size |
|---|---|---|---|
| 0 | Baseline: run `just ci`, record the results here | Green | hours |
| 1 | `src/ledger.rs` with property tests on the `no_std` path; `account.rs` rewired to it; funding routes made operator-only; fees to venue; payouts funded; freeze split from close; save version 6 | Contract tests show no player route changes supply; reconcile reports zero drift after the existing API tests | days |
| 2 | `webapp/src/journal.rs`; every mutation journaled inside its market job; `Idempotency-Key`; replay on start; engine tick journaled with its wall input; `GET /commands/{key}` | Kill the process at random points under the concurrency test; restart reproduces the last acknowledged state and no duplicate reward | week |
| 3 | `AssetKind`; goods in holdings; NPC wallets and policies; synthetic ladder and prints disabled on the server; catalogue, purchase and consume routes | Player-to-player and player-to-NPC fills, partial fills, IOC/FOK, amendments, stops, dividends and delistings conserve currency and units | weeks |
| 4 | Recipes and jobs; rewards from budgets; game events with production and demand effects; `/api/v1/economy` routes and `types.ts`; UI panels for wallet, inventory and jobs; `just ui` | The acceptance scenario below passes, including retries and restarts at every step | weeks |
| 5 | Tier two: SQLite journal and history, outbox with cursor replay for the game backend, bounded admission and quotas, load test with a declared target, backup and restore drill | p95 command latency and recovery time under the declared load; restore reconciles | weeks, only if tier one is outgrown |

Milestone 1 alone is a correct custodial token: conserved supply, operator
mint and burn, transfers and audited history. Milestone 2 makes it safe to
rely on. Milestones 3 and 4 make it an economy.

## Acceptance scenario

Initialise a world with a genesis supply in treasury. Onboard two players.
Pay one a quest reward from a budget wallet. That player buys ore from an
NPC merchant, starts a smelting job, waits for the tick that completes it,
lists the ingot, and the second player buys it. The venue takes its fee.
The second player consumes the ingot.

After each step, and again after replaying every request with the same
idempotency key and after restarting the server at each journal boundary:
both wallets, the NPC, the budget, treasury and venue sum to the genesis
supply; ore and ingot counts equal issued minus consumed; no hold is
outstanding without an order or job behind it. Also run the negative cases:
an empty budget, an NPC out of stock, a recipient at the balance cap, a
freeze while an order is resting, and a job completion delivered twice.

## Save format

There is no production data to migrate; the current snapshot is a demo.
Save version 6 adds the ledger, wallets, asset kinds, recipes, jobs and the
journal sequence. A version 5 file is refused, as today, and the demo world
is regenerated from seeds. If a version 5 world is ever worth keeping, one
importer posts its balances as a labelled `Migration` transaction from
issuance, marks every symbol a stock, and records the sequence it started
from. That is a day of work when needed and does not need designing now.

## Test baseline

Run on this checkout before writing the plan, with the environment's
default toolchain:

```
cargo test --all-features      # library: all suites pass, 0 failed
cargo test -p fehu-webapp      # webapp: all suites pass, 0 failed
```

`just lint`, the `no_std` path and the wasm build were not run for this
documentation-only change; milestone 0 runs `just ci` in full.
