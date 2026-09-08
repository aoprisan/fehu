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
backend. Those were written down as tier two, to be had by replacing the
journal file with SQLite without changing the command model. Milestone 5
built the outbox and left the file where it is; the reasoning is under
"Milestone 5 as built".

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
| 0 | ✅ Baseline: run `just ci`, record the results here | Green | hours |
| 1 | ✅ `src/ledger.rs` with property tests on the `no_std` path; `account.rs` rewired to it; funding routes made operator-only; fees to venue; payouts funded; freeze split from close; save version 6 | Contract tests show no player route changes supply; reconcile reports zero drift after the existing API tests | days |
| 2 | ✅ `webapp/src/journal.rs`; every mutation journaled inside its market job; `Idempotency-Key`; replay on start; engine tick journaled with its wall input; `GET /commands/{key}`; save version 7 | Kill the process at random points under the concurrency test; restart reproduces the last acknowledged state and no duplicate reward | week |
| 3 | ✅ `AssetKind`; goods in holdings; NPC wallets and policies; synthetic ladder and prints disabled on the server; catalogue, purchase and consume routes; save version 8 | Player-to-player and player-to-NPC fills, partial fills, IOC/FOK, amendments, stops, dividends and delistings conserve currency and units | weeks |
| 4 | ✅ Recipes and jobs; rewards from budgets; game events with production and demand effects; `/api/v1/economy` routes and `types.ts`; UI panels for wallet, inventory and jobs; `just ui` | The acceptance scenario below passes, including retries and restarts at every step | weeks |
| 5 | ✅ Tier two: outbox with cursor replay for the game backend, bounded admission, load test with a declared target, backup and restore drill. SQLite is *not* in it — see below for why the row got narrower | p95 command latency and recovery time under the declared load; restore reconciles | weeks, only if tier one is outgrown |
| 6 | ✅ The third principal: `service` credentials with scopes, issued and revoked by the operator; `POST /players` provisioning, idempotent on the game's own player id; save version 11 | A key carrying one scope opens that route and no other, on a locked server and an unlocked one; the operator still reaches everything; services and player mappings survive a restart | days |

Milestone 1 alone is a correct custodial token: conserved supply, operator
mint and burn, transfers and audited history. Milestone 2 makes it safe to
rely on. Milestones 3 and 4 make it an economy.

### Milestones 0 and 1 as built

Both are done. What was built follows the plan above; four things are worth
recording because they are decisions the plan left open or got slightly
wrong, and the code is now the reference for them.

**The baseline was not green.** `cargo clippy -p fehu-webapp --all-targets
-- -D warnings` failed on `main`: the engine step in
`reconciliation_checks_a_busy_market_and_detects_broken_reservations` was
never awaited, so that test asserted against a market that had not moved.
Fixed as part of milestone 0. The library and webapp test suites did pass.

**Opening an account is a faucet, not a mint.** The plan says the opening
balance becomes zero and `cash_cents` is removed. It is instead paid out of
treasury against a genesis supply — a transfer, so the supply is untouched,
which is the property the milestone is actually judged on and the stronger
of the two. It keeps the demo playable and the existing suites meaningful,
and it is the "operator faucet" the decisions section already assumes. A
treasury that cannot cover a request refuses it rather than printing the
difference.

**Unfunded liquidity is measured rather than removed.** Milestone 3 turns
off the synthetic ladder; until then, a fill against it has to settle
against *something* or the transaction cannot balance. It settles against a
`Synthetic` wallet, started with a float out of genesis and allowed to go
negative past it. Its debt is carried inside the conservation sum and
reported by `/api/reconcile` and `/api/supply`, so currency that unfunded
liquidity puts into players' hands is visible instead of quietly minted.
Retiring that debt is exactly what milestone 3 does; it should reach zero
there.

**Reservations are amounts, not handles.** The ledger sketch has
`hold`/`release(HoldId)`. What is implemented is amount-based
`reserve`/`release`, which is what the order path actually does and what
satisfies every invariant listed above. Handles that name *what* a
reservation is for belong with jobs and NPC inventory, and should arrive
with them in milestones 3 and 4.

Two bugs in the plan's own model, found by the property tests and fixed in
the ledger: `burned <= minted` is not an invariant while unfunded liquidity
is in debt (that debt is the slack), and a wallet allowed to hold a negative
balance must not be allowed to hold a reservation, or it can be drained
straight through one.

And one the plan does not mention at all, which conserved currency turns
from a leak into a hole: **a maker rebate has to come from somewhere.** With
`FEHU_MAKER_FEE_BPS` set and no taker fee to fund it — or with a taker that
has no wallet to charge, which every fill against synthetic liquidity is —
the venue is asked to pay currency it has never collected. Before the
ledger that money simply appeared. With it, the settlement was refused
*after* the book had traded: the shares moved and nothing was booked, an
empty fill log against an order the book had already worked down. A rebate
is now capped at what the venue holds plus what it collects on that same
fill, and what it cannot pay it does not pay. That is what `SettledFees`
is: what the venue actually charged, as opposed to what its schedule says
it would.

That fix exposed a second of the same shape, and a far more ordinary one:
**a resting buy's reservation was still held when its own fill was
posted.** The reserved cash *is* the cash that pays for the fill, so a
trader who had committed most of their balance to an order looked
insolvent at the moment it filled, and the settlement was refused — again
after the book had traded. A bid for 1212 shares worked down to 791
remaining with an empty fill log, no position and an untouched balance.
The reservation is now released before the settlement is posted rather
than after it.

Both were invisible before the ledger, because settlement could not fail:
the account was simply credited or debited whatever it held. Being unable
to fail quietly is most of what a balanced transaction is for. The general
lesson for milestones 2 and 3: settlement runs *after* the book has
traded and cannot be unwound, so everything it needs must be true before
it is called. `Market::settle_trade` counts and logs a refusal it cannot
prevent, and `/api/reconcile` reports the drift, but that is a smoke alarm
rather than a design.

### Milestone 2 as built

Done. `webapp/src/journal.rs` holds the command model, the file, the
idempotency index and the single apply path; `webapp/tests/journal.rs` holds
the acceptance. Five things are worth recording.

**The command enum is the mutation surface, not a description of it.** The
plan reads as though the journal sits beside the handlers, writing down what
they did. Two implementations of one mutation — the handler's and the
replay's — would drift, and the drift would only show up on the restart after
a crash, which is the worst possible place to find it. So there is one:
`journal::apply`, which the handlers reach through `Market::run_command` and
which replay reaches directly. What was in the handlers moved into it, and
what is left in `api.rs` is HTTP — authorise, clean, build a `Command`, map
the outcome. This is the change the milestone actually cost.

**A command reads nothing a replay cannot.** The plan says this of the engine
tick and leaves the rest implicit, but `Market::place`, `amend`, `cancel`,
`halt`, `resume` and the dividend all read `self.clock.now()`, and every
route stamped its records with `wall_now_ms()`. All of them now take the
instant from the entry: the simulated one is pinned on the market for the
duration of the apply (`Market::now`), and the wall-clock one is a field of
the entry. The API key was the other reading — `Keyring::issue` calls the
operating system's randomness — so the key is now generated by the route and
only its digest is journaled.

**Which is also how the credential stopped being kept anywhere.** The
keyring used to hold a newly issued key until the sign-up response took it.
Journaling that would have made the file a list of live credentials, so
instead nothing holds one: the route generates the key, hands it to the
market as a digest, and puts it into the response itself. The cost is
recorded rather than hidden — a *replayed* sign-up comes back without a key,
because there is no second copy to give. A client that loses that response
creates another user.

**The idempotency hash covers what the client asked for, not what the server
chose.** Found by the sign-up test: a retried sign-up generates a fresh key,
so hashing the command whole made every retry a `409` against itself. The
digest is taken over the command with the server's own values blanked.

**Only what a client waits on is flushed.** `fsync` per engine tick, four
times a second, for an entry nobody is waiting on, would be most of the cost
of the journal for none of the benefit. Steps are appended without a flush;
everything else flushes before it answers, and because the appends are
sequential that flush carries every step before it. So a crash can only lose
steps that no acknowledged command depended on — and replay reconstructs
those by stepping to wall time again.

The two things the plan asked for that are *not* free: listing a symbol now
warms its history up inside the market job rather than beside it, because a
listing is a command like any other and a command is applied by the market
(listing is rare and an operator's; a fill is neither). And the idempotency
index is bounded — `FEHU_COMMAND_LOG`, 10 000 by default — rather than kept
"for the life of the world", so a retry under an evicted key is applied again
rather than replayed. Both are noted where they are: the cap is in
`Options::command_log`, the warm-up in `journal::apply_listing`.

Determinism is untouched again: the journal draws no randomness and the price
process never sees it, so neither `STATE_VERSION` nor `EXCHANGE_VERSION`
moves, and `tests/determinism.rs` and `tests/trading.rs` pass unchanged.

### Milestone 3 as built

Done. `src/exchange.rs` gained the switch, `webapp/src/symbol.rs` the asset
kind, `webapp/src/catalog.rs` and `webapp/src/npc.rs` are new, and
`webapp/tests/goods.rs`, `webapp/tests/npc.rs` and
`webapp/tests/economy_goods.rs` hold the acceptance. Six things are worth
recording.

**The ladder is a library capability, not a server policy.**
`TradingParams::synthetic` turns the synthetic maker ladder and the printed
flow off; `requote` and `synthetic_flow` then return without drawing
anything, so the book holds only real orders, the tape only real trades, and
the tick's volume is whatever traders did. The simulator carries on untouched
as the reference price — which is what its own `long_jump`ed stream was
always for — so the bare price series is identical either way and neither
`STATE_VERSION` nor the golden hashes move. `EXCHANGE_VERSION` moves to 2 for
the field.

**The supply ceiling belongs to the ladder, not to the asset.** The plan says
a buy is bounded by the units no trader holds or is already bidding for.
That rule exists because the ladder will sell what nobody holds. Without a
ladder it is not merely unnecessary but wrong: every fill then comes from a
holder who has reserved the units, so a bid can no more conjure one than a
wish can, and enforcing the ceiling would stop two players bidding for the
same good at once. The check is now conditional on the symbol quoting
synthetically, and the audit branches the same way — for a stock with a
ladder, holdings plus resting bids against the float; without one, holdings
alone; for a good, the stricter equality: every issued unit that has not been
consumed is held by somebody.

**A good is listed holding nothing.** The plan's `Good { issued, consumed }`
could have taken an opening `issued` at the listing, and that would have been
a hole: units nobody holds and nobody can buy, because there is no ladder to
sell them. So a listing issues none, and the only things that make a unit are
a catalogue purchase and an NPC endowment — both of which hand the units to a
holder in the same command that creates them.

**The catalogue separates the two conservation laws.** A purchase moves
currency (buyer to the good's issuer wallet, one balanced transaction, supply
untouched) *and* creates units. Keeping those apart is what makes it
auditable: `/api/supply` reads the same before and after, and the unit count
is checked by its own sentence. Consuming is the mirror and simpler still —
units leave, no currency moves, because a thing that has been used up is not
a thing that has been sold. A good's issuer wallet therefore takes no float
out of treasury: it is where money arrives, not a payout waiting to happen.

**An NPC is an ordinary trader, and two decisions make it a cheap one.** It
has a wallet of its own kind, a position, reservations, and orders that go
through the same command path a player's do; a fill against it settles,
charges fees and reconciles like any other, which is why nothing needed
special-casing. What is new is when it quotes. Redrawing every tick would put
an order on the book four times a second per level per side and take it off
again, so quotes are redrawn when the reference leaves a band *or* when fewer
orders are resting than the NPC left behind — the band stops it redrawing a
book that has not moved, the count stops it *not* redrawing one that has been
eaten, and levels it still cannot fund are refused and not counted, so an NPC
that can afford nothing settles at zero rather than trying forever. And its
orders are not written to the order log: nobody asks after them by id, and
logging them would evict every player's record within a minute. The book, the
reservations and the audit do not read that log.

**A user with no key is now legal, in exactly one case.** An NPC needs an
identity — a trader belongs to a user and every audit walks that link — but
no player is behind it, so it is created with no credential at all: there is
none to leak and no request can arrive claiming to be it. The save
invariant that every user has exactly one API key digest is relaxed to
"…or is an NPC's", and no further.

What is *not* done, deliberately: the ladder is still on by default.
`FEHU_SYNTHETIC=0` turns it off world-wide and is tested — with it off, every
fill has a funded counterparty and `synthetic_debt_cents` stays at zero,
which is the number this milestone exists to retire — but a world with no
merchants and no players would otherwise have an empty book, and the demo is
a world with no merchants and no players. Seeding merchants for the four
seeded symbols is a change to what the demo *is*, and belongs with milestone
4's world configuration rather than smuggled in here. A good is always
quoted without a ladder regardless, because its units are counted.

Determinism is untouched a third time. Quoting reads the market and the
symbol and nothing else — no clock, no randomness — and happens inside
`Command::Step`, so a replayed step draws the same book.

### Milestone 4 as built

Done. `webapp/src/jobs.rs`, `webapp/src/rewards.rs` and `webapp/src/world.rs`
are new; `webapp/tests/jobs.rs`, `webapp/tests/rewards.rs` and
`webapp/tests/economy_jobs.rs` hold the acceptance; the UI gained one panel
and the save format went to version 9. Nine things are worth recording.

**A job is due at an instant, not after a number of ticks.** The plan
measures a recipe in `duration_ticks`. A tick belongs to a *symbol* — each
simulator has its own interval, and a world with four listings has four
answers to "how long is a tick" — and a job belongs to no symbol. So a
recipe takes `duration_secs` of simulated time and a job carries the instant
it is due at, which the engine step compares against the instant it is
advancing to. That instant is a field of the journal entry, so a replayed
step delivers exactly the jobs the original one delivered.

**A job holds nothing.** The plan pairs jobs with the hold *handles* that
milestone 1 deliberately did not build, on the assumption that a job would
reserve its inputs for its duration. It does not: starting one consumes the
inputs and posts the cost in the same command. That keeps the acceptance's
third question — is anything held for nothing — trivially answerable, needs
no reservation kind that every audit would have to learn, and gives a job
the same refusal shape as everything else: check, move the money, move the
units, and a refusal leaves the world as it found it.

**What a job will deliver is decided when it starts.** The outputs are
resolved, scaled and written into the job at the start, so a recipe an
operator rewrites afterwards changes the next job and not this one, and
neither does an event that lands halfway through. The recipe's version is
recorded beside them, which is the audit trail the plan's `(RecipeId, u32)`
was for, without a job having to go looking for a text that may have been
rewritten twice since.

**What comes out of the furnace costs what went into it.** The first
implementation destroyed the inputs — `Trader::destroy`, which consuming
uses — and gave the output the job's cash cost as its basis. That is
conserved but it reads wrong: smelting showed as a realised loss on the ore
followed by a phantom gain on the ingot. Inputs are now *withdrawn*
(`Trader::withdraw`): their basis leaves the position with them and lands in
what the output is reckoned to have cost. Consuming still writes off, because
a thing that has been used up really is a loss.

**A reward is idempotent on the game's event id, not on the request.** An
`Idempotency-Key` protects a request; it does not protect an *event*. A game
backend that crashes after paying, restarts and re-derives the quest result
will send a second request with a second key for the same kill. So a reward
carries the game's own `source` id, that id is remembered with the receipt it
produced, and the second request gets the first receipt back with
`duplicate: true` and moves nothing. The index is bounded (`FEHU_REWARD_LOG`)
and saved with the market, for the same reasons the idempotency index is.

**The operator key is the service credential.** The plan's API section has
three principal kinds — player, service with scopes, operator — and this
milestone implements two. Rewards, budgets, recipes and the catalogue are the
operator's, which in tier one *is* the trusted game backend: one host, one
process, the game backend on the same network. Splitting `provision`,
`reward`, `inventory` and `events` into separate scoped credentials is a
change to `auth.rs` and the journaled `Principal`, worth doing when there is
more than one service and not before. `POST /api/transfers` is the one new
route a player can reach, and it moves currency without making any.

**The world's mood is integer and linear.** A game event now pushes
modifiers on production and demand alongside its price effects. The price
effects decay exponentially in `f64` inside the deterministic core; these do
not — a modifier is at full strength when it lands and ramps down to nothing
in integer basis points. A yield is a count of units and a quote size is a
count of units, so measuring them in floats would only add a rounding
question, and replay reaches the same batch without touching `libm` at all.
Production is read once, when a job starts; demand is read every time a
merchant re-quotes, which is how the world's appetite reaches the book.

**`/api/v1/economy` is a second spelling, not a second implementation.**
Every route under it is the same handler as the one it mirrors, so the game
backend can speak the documented economy API and the demo UI can go on
speaking the surface it was written against. Two of the plan's rows are not
aliased: mint, burn and freeze stay at their account-addressed paths, because
`/admin/mint` would need an account in its body and there is nothing to gain
from a third way of naming one.

**The demo can now be an economy without being one by default.** Milestone 3
left the ladder switched on because a world with no merchants and no players
would otherwise have an empty book, and said seeding the four demo symbols
belonged here. It does, as `FEHU_SEED_MERCHANTS_CENTS`: a world that is being
*warmed up* gets a funded merchant behind every listing, each given that much
currency out of treasury and about as much stock as it would buy, through the
same journaled command a request would send. With `FEHU_SYNTHETIC=0` beside
it the demo is a market in which everything on the book was paid for. It is
off by default, because turning it on by default would change what every
existing test's world is; and a *restored* world is left alone, since it
already has the merchants it had.

**And the acceptance found one thing.** `POST /api/symbols` answered a retry
of the listing that succeeded with `409 already listed`: its cheap
"is this ticker taken" check ran *before* `run_command` reached the
idempotency index. The one command that genuinely cannot be sent twice was
also the one whose lost response could not be recovered. It now asks the
index first, and answers a retry with what it answered the first time.

Determinism is untouched a fourth time. Jobs and rewards draw no randomness,
the price process never sees them, and the world's modifiers are integers, so
neither `STATE_VERSION` nor `EXCHANGE_VERSION` moves and the golden hashes in
`tests/determinism.rs` are unchanged.

### Milestone 5 as built

Done, and narrower than the row it was written into. `webapp/src/outbox.rs`
is new; `limit.rs` gained an `Admission`; `webapp/tests/outbox.rs` and
`webapp/tests/load.rs` hold the acceptance; the save format went to version
10. Ten things are worth recording, and the first is why SQLite is not here.

**Tier two turned out to mean delivery, admission and drills — not a
database.** The plan's tier two replaces the journal file with SQLite, and
that was written down as one decision because the three things it was meant
to buy arrived together in the sentence. Taken apart, they do not:

- *Crash-safe commits.* Already had. An entry a client is waiting on is
  `fsync`ed before the response goes out, so an acknowledged command is on
  disk; the milestone-2 acceptance kills the process at arbitrary points and
  gets it back. SQLite would change how that durability is spelled, not
  whether it exists.
- *Concurrent writers.* Not wanted. This server has exactly one writer by
  design — every change to money or to a book is one job on the market actor,
  which is what keeps reservations and fills consistent without a mutex, and
  is the property the whole architecture above it rests on. A second writer
  is not an upgrade to that; it is a different server.
- *A queryable history.* The only one left, and it is a reporting need rather
  than a serving one. A snapshot is a complete, self-describing JSON world,
  and `GET /api/backup` now hands one over to whatever wants to query it.

So the file stays and the milestone is what remained: an outbox, a bound on
concurrent work, a declared load, and a drill. Adding a C dependency and a
second persistence model to a webapp whose whole persistence story fits in
two files, for a serving need that turned out not to exist, would have been
the expensive half of a plan written before the cheap half was built.

**The outbox carries what nobody asked for.** The plan says "outbox with
cursor replay" and leaves the contents open. The line drawn is: a fact with a
*requester* is already recoverable — the sender has its response, and a lost
response comes back from `GET /api/commands/{key}` under its
`Idempotency-Key`. A fact with no requester is recoverable from nothing. So a
purchase, a transfer, a reward and a job *starting* are not in the outbox,
and a fill, a job coming due, an order the venue withdrew, a stop that fired,
a halt, a listing, a delisting and an accepted game event are. That line was
already in the code before this milestone — `StreamMessage::JobDone`'s own
comment says exactly it about a job starting — so the outbox is the stream's
message kinds minus `hello` and `tick`, and no new vocabulary had to be
invented for it.

**Ticks are left out on purpose.** They are the highest-volume thing the
server produces, they are market data rather than economy, and
`/api/symbols/{sym}/bars` has them whenever they are wanted. A durable log of
them would be a time-series database, which is a different project.

**The outbox is state, not a side-channel.** Entries are appended inside the
command that caused them and committed only once that command is in the
journal, so the log holds what was *committed* rather than what was
attempted; they are saved with the snapshot and rebuilt by replay, so a
restart returns the same facts under the same numbers. Everything else
follows from that: the acknowledgement is a `Command` like any other, because
a cursor that moved only in memory would fall back to the snapshot's value on
a restart and hand the backend facts it had already acted on; and a fact
published *outside* a command is refused entry, because an entry replay would
not reproduce is worse in a log that promises replay than no entry at all.
The guard is `Market::pinned_now` — the same field that keeps a command off
the clock.

**A fact is stored as it will be sent.** An entry keeps its payload as
rendered JSON rather than as the typed message. Nothing in the server reads
one back — it is written once, handed on unchanged and dropped — so
rendering at the point it is noted keeps the outbox from becoming a second
reason for every DTO on the stream to grow a `Deserialize` impl and a
`&'static str` interning module. `CommandRecord::result` keeps a response the
same way, for the same reason.

**Bounded admission is a semaphore, not an actor.** Everything else in this
server that is shared is an actor; this one must not be, because its whole
purpose is to answer *without waiting on anything*. A request that finds the
server full is refused on the spot with `503 overloaded` and a
`Retry-After` — a "later" the client can act on, which is precisely what it
could not do while sitting in a queue. It sits inside the rate limiter, so a
client already over its own allowance never takes a place from one that is
not.

**Reads are not gated.** `/api/health` is the single most useful thing to be
able to read when a server is full, and shedding it would take the
instrumentation away exactly when it is wanted. Reads are also cheap: they go
to the symbol actors and queue behind nothing. Only mutations and new stream
connections take a place.

**The steadier of the two declared numbers is throughput.** The plan's done-
when is p95 latency, and `webapp/tests/load.rs` asserts one — but a p95
measured in a debug build on shared CI, beside every other test binary, is
noisy, and a test that fails when the machine is busy is a test nobody
trusts. So the file declares both: the whole 512-mutation burst through in
under five seconds, and the p95 of one request under 750 ms. The first is the
real bound and the second is the shape. Measured while writing this: p50
around 60 ms, p95 around 125 ms, max around 140 ms, no request shed, about
four thousand commands a second; recovery from a snapshot plus a journal of
the whole burst, about 230 ms.

**A backup is a snapshot, taken out of band and handed back.** A snapshot is
complete on its own — the journal beside a live state file only covers the
gap since the server's own last one — so backing up needs no new format and
no journal, just a snapshot at a moment of the operator's choosing, taken
from one consistent market job like `/api/reconcile` is. It truncates
nothing, because the live state file still needs the journal it has. And it
is *handed back* rather than written to a path the request names: an operator
route that wrote wherever its body said would be an arbitrary file write with
a key on it, and `curl > backup.json` is the same drill without one.

**And what is still not here.** Delivery is pull, not push: the backend polls
`GET /api/outbox` and there are no webhooks, which is the right default when
the backend is on the same network and the wrong one across a WAN. Quotas
beyond the rate limiter and the two admission bounds — a cap on one trader's
resting orders, say — are not built; nothing measured has asked for them yet.
The plan's `service` principal with scopes is still unbuilt for the reason
milestone 4 gave: the operator key is the trusted game backend in tier one.
And the outbox's retention is a ring, not a history: what a consumer misses
past `FEHU_OUTBOX` is gone, and the read says so rather than pretending
otherwise.

Determinism is untouched a fifth time. The outbox and the admission bounds
draw no randomness, the price process never sees either, and the golden
hashes in `tests/determinism.rs` are unchanged.

### Milestone 6 as built

Done. `webapp/src/service.rs` is new; `account.rs` gained the player
mapping, `market.rs` the registry behind the published directory, `api.rs`
the `Trusted<Scope>` extractor and five routes; `webapp/tests/service.rs`
holds the acceptance; the save format went to version 11. Seven things are
worth recording.

**This is the row the plan kept deferring.** Milestone 4 left the `service`
principal unbuilt "on the assumption that the operator key is the trusted
game backend in tier one", and milestone 5 repeated it word for word. That
assumption is fine for a single-player game on localhost and wrong for
anything shared, because it is not a statement about *authentication* — it is
one about blast radius. A backend that has to pay a quest reward held the
credential that can also mint currency, burn it, freeze accounts and rewrite
the catalogue, so every bug and every leak in the backend was a bug in the
mint. Nothing about that needed a new deployment tier to become worth
fixing.

**A scope narrows a credential; it does not narrow the operator.** This is
the decision the whole milestone rests on, and it is what made it cheap.
Every route a scope opens is still open to the operator on exactly the terms
`Admin` always applied — an unset `FEHU_ADMIN_KEY` included, which stays
open because that is what the repository documents a single-player game on
localhost gets. So a service credential is an *additional, weaker* way in
rather than a new requirement, a world that never issues one behaves exactly
as it did before there were any, and every one of the 265 tests that predate
the milestone passes untouched, bar the one line noted below. A design that had made scopes mandatory would have
rewritten every one of them, for no security a world with one operator does
not already have.

**A key the registry knows is judged as that service, and only as that
service.** The resolution order is the safety property, not a detail. The
obvious spelling — try the operator first, fall back to the service — is
wrong on an unlocked server, where `Admin` accepts anything: a key carrying
`reward` alone would be silently promoted to operator authority by the *lack*
of configuration, which is precisely the deployment least likely to notice.
So the registry is asked first, and a scope the key does not carry is refused
with `missing_scope` even where nothing is locked. `a_service_key_is_never_promoted_to_the_operators` is that
test.

**Issuing is the operator's, reachable through no scope.** There is no
`services` scope and there will not be one: a credential that could issue a
credential could issue itself a wider one, which would make every other scope
advisory. Granting authority stays with the credential that already has all
of it. Revoking is a tombstone rather than a delete, so a key that was taken
away is refused as *revoked* rather than as unknown, and the service id in a
journal entry still resolves to the thing that sent it.

**Provisioning is idempotent on the game's id, not on the
`Idempotency-Key`.** Same shape as `PayReward`'s `source`, and for the same
reason: the backend already has an id for every player and should not have to
remember a second one, or remember whether it has called before. So a repeat
answers `200` with the same ids and `created: false`, and the account opens
*empty* — arriving in the world creates no currency, which is the invariant
the ledger rests on and is checked at the one route that makes people. The
`api_key` on a repeat is `null`, and that cost one small change: `with_key`
now attaches a credential only to a `201`, because a command that answered
`200` created nothing and the key generated for it was never installed.
Handing it back would have been handing out a credential that opens nothing.

**The registry lives in the published directory, not beside it.** It was
written as a field on `Market` first, and every service key was refused as
unknown — the directory a request is authorised against is a *snapshot* the
market publishes, so a second copy is a second thing to keep in step and the
one a request never reads. The API keys were already right about this. Held
in the directory, authorising a game-backend request sends no job anywhere,
which is the property that made the actor topology worth having.

**One route changed shape, deliberately.** `POST /api/v1/economy/players` was
an alias for `create_trader` — a sign-up under an economy path, with no
external id anywhere in it. It is now the provisioning route the plan
specified. `POST /api/traders` is untouched, so nothing lost a way in, but a
client calling the v1 path with `cash_cents` and no `external_id` now gets a
`400`. Making the alias real is the point of the milestone; leaving it to
mean something else would have left the plan's own API section describing a
route that did not exist.

**And what is still not here.** The SSE key still travels in the query
string. The plan lists moving it into a header or a short-lived stream ticket
in the same section as the service principal, and it is a real weakness —
query strings reach proxy logs — but it is about how a credential *travels*
rather than what it may *do*, and it drags the committed UI bundle into a
change that otherwise touches no TypeScript. It is its own milestone. Nor is
there key rotation: a service that loses its key is issued a new service,
because the registry kept only a digest. And a service has no rate-limit
bucket of its own: the limiter keys on a *user* id, so a service key falls
into the shared anonymous bucket — which is exactly where the operator key
already was, so nothing regressed, but a busy backend and an unauthenticated
client now spend the same allowance for a reason that is no longer true.
Giving a service its own bucket is a small change and the right one as soon
as anything measures it. Quotas beyond that are still the rate limiter and
the two admission bounds.

Determinism is untouched a sixth time. Services, scopes and player mappings
draw no randomness, the price process never sees any of them, and the golden
hashes in `tests/determinism.rs` are unchanged.

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

Version 10 is current: it adds the outbox — the facts the game backend has
not collected yet, and the cursor saying how far it has read. A version 9
file has neither, and starting from one would tell a consumer the log begins
at 1 when it does not, so it is refused like every other version.

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

### After milestone 6

Run the same way, command by command. All of it passes:

```
cargo fmt --all -- --check                                  # clean
cargo clippy --all-targets --all-features -- -D warnings    # clean
cargo clippy --all-targets --no-default-features -- -D warnings
cargo clippy -p fehu-webapp --all-targets -- -D warnings    # clean
cargo build --all-features / --no-default-features / +serde # clean
cargo build --target wasm32-unknown-unknown (both feature sets)
just ui-check                                               # bundle unchanged
cargo test --all-features                                   # 103 passed, 0 failed
cargo test --no-default-features --tests                    #  96 passed, 0 failed
cargo test -p fehu-webapp                                   # 288 passed, 0 failed
```

The webapp gained `tests/service.rs` (14) and `service.rs`'s own unit tests
(7), plus one contract test and one reconcile test. `just ui-check` reports
the committed bundle unchanged: the only UI change is `types.ts`, and types
are erased before anything is emitted.

The 265 webapp tests that predate the milestone pass unchanged, with one
exception: `reconciliation_contract` gained `players_checked`, because the
reconciliation now counts provisioned players and that test exists to fail
when a response's key set moves. Nothing about authorisation was edited to
make a predating test pass, which is the claim "a scope narrows a credential,
not the operator" makes — checked rather than asserted.

### After milestone 5

Run the same way, command by command. All of it passes:

```
cargo fmt --all -- --check                                  # clean
cargo clippy --all-targets --all-features -- -D warnings    # clean
cargo clippy --all-targets --no-default-features -- -D warnings
cargo clippy -p fehu-webapp --all-targets -- -D warnings    # clean
cargo build --all-features / --no-default-features / +serde # clean
cargo build --target wasm32-unknown-unknown (both feature sets)
cargo test --all-features                                   # 103 passed, 0 failed
cargo test --no-default-features --tests                    #  96 passed, 0 failed
cargo test -p fehu-webapp                                   # 265 passed, 0 failed
```

The webapp gained `tests/outbox.rs` (14) and `tests/load.rs` (8), and the
declared load reported, on the machine this was written on:

```
burst:      n=512 in 1.09s p50=57ms p95=122ms p99=133ms max=137ms shed=0
saturated:  n=512 in 0.83s p50=12ms p95=128ms max=160ms shed=491  (max_inflight=1)
recovery:   227ms, and the restored world reconciles
```

### After milestone 4

Run the same way, command by command. All of it passes:

```
cargo fmt --all -- --check                                  # clean
cargo clippy --all-targets --all-features -- -D warnings    # clean
cargo clippy --all-targets --no-default-features -- -D warnings
cargo clippy -p fehu-webapp --all-targets -- -D warnings    # clean
cargo build --all-features / --no-default-features / +serde # clean
cargo test --all-features                                   # 103 passed, 0 failed
cargo test --no-default-features --tests                    #  96 passed, 0 failed
cargo test -p fehu-webapp                                   # 228 passed, 0 failed
cargo build --release --target wasm32-unknown-unknown …     # clean, both feature sets
cd webapp/ui && npm ci && npm run build                     # bundle rebuilt and committed
```

Three new suites. `webapp/tests/jobs.rs`: a recipe that names only listed
goods, a job refused for want of ore, for want of cents and for units already
promised to a resting sell, one delivered on the step that reaches it and
only once, one cancelled for what the recipe says it gives back, one retried
under a single key and started once, an event moving the yield of the *next*
job and the size a merchant quotes, a modifier ramping down and being swept,
and a world stopped with a job in the furnace that comes back running it.
`webapp/tests/rewards.rs`: a budget filled out of treasury and never minted,
a reward that moves currency rather than making it, one refused when the
budget runs dry, the same game event paid once however many keys ask for it,
transfers between players, a frozen account that is paid but pays nobody, and
a restart that still remembers what it has paid for. One test joins
`webapp/tests/npc.rs`: a world with no synthetic liquidity, seeded, quoting
both sides of every book and owing nobody anything.

`webapp/tests/economy_jobs.rs` is the milestone's own acceptance: the
scenario above, asked after every step whether the currency adds up, whether
every unit is somewhere and whether anything is held for nothing; then every
request sent again under its original key, with nothing moving for any of
it; then the world rebuilt from the journal at each of the sixteen
boundaries it passed through. The plan's negative cases are the tests beside
it — an empty budget, a merchant out of stock, a recipient at the balance
cap, a freeze while an order is resting, and a job that must be delivered
once however many steps pass over it and once more after a replay.

The 195 webapp tests that predate the milestone are unchanged and pass, apart
from three key sets in `contract.rs` that gained a field.

### After milestone 3

Run the same way, command by command. All of it passes:

```
cargo fmt --all -- --check                                  # clean
cargo clippy --all-targets --all-features -- -D warnings    # clean
cargo clippy --all-targets --no-default-features -- -D warnings
cargo clippy -p fehu-webapp --all-targets -- -D warnings    # clean
cargo build --all-features / --no-default-features / +serde # clean
cargo test --all-features                                   # 103 passed, 0 failed
cargo test --no-default-features --tests                    #  96 passed, 0 failed
cargo test -p fehu-webapp                                   # 179 passed, 0 failed
```

Three new suites. `webapp/tests/goods.rs`: a good listed holding nothing and
quoting nothing, a good with neither shareholders nor a buyout, the
catalogue running out and being taken away, units promised to a resting sell
that cannot be eaten, a purchase not paid for twice by a retry, and a world
with goods in it coming back whole. `webapp/tests/npc.rs`: a merchant funded
out of treasury without making currency, quoting both sides, running out and
falling silent on the side it has run out of, switched off and back on,
given shares of a stock nobody held, coming back quoting what it was
quoting, and a world with no synthetic liquidity that owes nobody anything.
`webapp/tests/economy_goods.rs` is the milestone's own acceptance: every
kind of fill the venue accepts — partial, IOC, FOK, amended, player to
merchant, player to player, a stop that fired — and then dividends and a
delisting, each followed by the same two questions, does the currency add up
and is every unit somewhere.

One library test joins them: an exchange with the ladder off holds only what
traders put in it, and its price series is the bare simulator's.

The 145 webapp tests that predate the milestone are unchanged and pass.

### After milestone 2

Run the same way, command by command. All of it passes:

```
cargo fmt --all -- --check                                  # clean
cargo clippy --all-targets --all-features -- -D warnings    # clean
cargo clippy --all-targets --no-default-features -- -D warnings
cargo clippy -p fehu-webapp --all-targets -- -D warnings    # clean
cargo test --all-features                                   # 102 passed, 0 failed
cargo test --no-default-features --tests                    #  95 passed, 0 failed
cargo test -p fehu-webapp                                   # 156 passed, 0 failed
```

New suite: `webapp/tests/journal.rs` — a restart with no snapshot of the work
at all, retried deposits and sign-ups, a key reused for a different request,
a lost response recovered by its key, the journal truncated by a snapshot,
the engine tick replayed off the clock, a journal with no snapshot to replay
onto, a market whose journal has stopped taking entries, and the milestone's
own acceptance: stopping at every acknowledged point in a mixed workload and
coming back at exactly that point, with no reward paid twice.

The other 145 webapp tests are unchanged and pass, which is the useful
result: every route now runs as a journaled command and none of them behaves
differently for it.

### After milestone 1

`just` is not installed in the environment this was built in, so the `ci`
recipe was run command by command. All of it passes:

```
cargo fmt --all -- --check                                  # clean
cargo clippy --all-targets --all-features -- -D warnings    # clean
cargo clippy --all-targets --no-default-features -- -D warnings
cargo clippy -p fehu-webapp --all-targets -- -D warnings    # clean (was red)
cargo build --all-features / --no-default-features / +serde # clean
cargo test --all-features                                   # 102 passed, 0 failed
cargo test --no-default-features --tests                    #  95 passed, 0 failed
cargo test -p fehu-webapp                                   # 136 passed, 0 failed
cargo build --release --target wasm32-unknown-unknown …     # clean, both feature sets
cd webapp/ui && npm ci && npm run build                     # bundle unchanged
```

The determinism goldens in `tests/determinism.rs` and the exchange
invariants in `tests/trading.rs` are untouched and still pass: the ledger
draws no randomness and the price process never sees it, so neither
`STATE_VERSION` nor `EXCHANGE_VERSION` moves.

New suites: `tests/ledger.rs` (property tests, on both the `std` and
`no_std` paths) and `webapp/tests/economy.rs` (conservation across
sign-ups, fills, fees, dividends, delistings, freezes and restarts, and
that only an operator can mint or burn).
