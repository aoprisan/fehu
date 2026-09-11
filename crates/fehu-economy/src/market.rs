//! The market: the actor that clears every trade, the symbol actors it
//! drives, and the shared application state that addresses them.
//!
//! # No locks
//!
//! Nothing in this server is behind a mutex. Every piece of state belongs
//! to one tokio task and is reached only by sending that task a job
//! ([`crate::actor`]):
//!
//! | actor | owns | who sends it jobs |
//! |---|---|---|
//! | one [`SymbolState`] actor per symbol | the simulator, the book, the bars, the tape, the halt, the stops | anyone for reads; **only the market** for anything that changes the book |
//! | the [`Market`] actor | users, accounts, traders, the order log, the event log, the symbol table, the order-id counter | request handlers and the engine |
//! | the [`Stream`] actor | the sequence counter and the replay buffer | anyone, to publish; connections, to subscribe |
//! | the rate limiter actor | the token buckets | the rate-limit middleware |
//!
//! Two things are read on every request and change only at sign-up or
//! listing time — who a key speaks for, and which symbols exist. They are
//! published by the market on `tokio::sync::watch` channels as immutable
//! snapshots, so a handler reads them without sending anything.
//!
//! # The one rule
//!
//! **Calls go one way: the market calls the symbols; the symbols call
//! nobody.** A handler calls the market, or a symbol for a read, never one
//! from inside the other. With no cycle there is no deadlock to design
//! around, and the two invariants that used to need one big lock fall out:
//!
//! * The market is the only thing that ever changes a book — an order, a
//!   cancel, a resume, a delisting, and the engine step itself all run as
//!   jobs *on the market actor*, which calls the symbol for the book
//!   operation and then books the money side before it runs anything else.
//!   So the reservation behind every resting order is exactly what the book
//!   says it should be, `held + bids ≤ outstanding` is checked against
//!   numbers that cannot move under it, and every fill is in the accounts
//!   before the next job sees the book.
//! * A consistent snapshot of the whole market — for reconciliation, for a
//!   save — is one job on the market that asks each symbol for a copy of
//!   itself. Nothing else can be halfway through a book while it runs.
//!
//! What runs in parallel is what is expensive: the engine step advances
//! every symbol's simulator at once (the market job fans the step out to
//! every symbol actor and joins them), and every read of a quote, a book,
//! a bar or the tape goes straight to that symbol's actor and waits on
//! nothing else. What is serialised is what has to be: the money.
//!
//! Order ids span every book, because the order log and the client-order-id
//! index do. The market keeps the counter: an order takes the larger of the
//! counter and the target book's own next id, and the book's counter after
//! the submit is folded back in, so no two trader orders in any two books
//! share an id, and a delisted book's counter is folded in as it leaves.
//!
//! A symbol that is delisted is dropped from the published table and marked
//! delisted under its own actor. A request that took a handle to it just
//! before finds the mark and is answered "no such symbol".

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fehu::{
    Interval, MarketHours, Order, OrderStatus, Owner, Resting, Side, Timestamp, Trade, TraderId,
};
use serde::Serialize;
use tokio::sync::{broadcast, watch};

use fehu::ledger::{Draft, Ledger, LedgerError, Reason, WalletId, WalletKind};

use crate::account::{
    Account, AccountId, LedgerEntry, LedgerKind, MAX_BALANCE_CENTS, MoneyError, Player, User,
    UserId, check_transfer, settlement_draft,
};
use crate::actor::{Actor, Gone};
use crate::auth::Keyring;
use crate::catalog::{
    Catalog, CatalogError, CatalogItem, ConsumeReceipt, GoodsError, PurchaseReceipt,
};
use crate::events::EventRecord;
use crate::jobs::{
    Job, JobBook, JobDelivery, JobError, JobStatus, Line, Recipe, RecipeBook, scaled_qty,
};
use crate::journal::JournalError;
use crate::limit::{Decision, Limiter, Rate};
use crate::metrics::Metrics;
use crate::npc::{BPS, MAX_NPCS, Npc, NpcDto, Policy};
use crate::rewards::{Budget, BudgetDto, RewardBook, RewardError, RewardReceipt, RewardRule};
use crate::save::{MarketSave, STATE_VERSION, Save};
use crate::service::{ScopeSet, Service, ServiceAuth, ServiceError, ServiceId, Services};
use crate::trading::{
    BookDto, Fees, FillRecord, HoldingDto, Liquidity, MAX_STOPS_PER_TRADER, OpenOrderDto,
    OrderRecord, OrderResponse, Refused, SettledFees, StopOrder, StopRequest, TradeDto, Trader,
};
use crate::world::{Effect, SymbolEffects, WorldEffects};

pub use crate::symbol::*;

/// When a move halts a symbol, and for how long.
#[derive(Clone, Copy, Debug)]
pub struct HaltPolicy {
    /// A move this far from the band halts a symbol; `0` turns that off.
    pub price_limit_pct: f64,
    /// How long an automatic halt lasts, in simulated seconds.
    pub halt_secs: u64,
}

/// A listed symbol: its ticker, and the actor that owns its state.
pub struct Symbol {
    pub ticker: &'static str,
    actor: Actor<SymbolState>,
}

impl Symbol {
    fn spawn(state: SymbolState) -> Arc<Self> {
        Arc::new(Self {
            ticker: state.info.symbol,
            actor: Actor::spawn(state),
        })
    }

    /// Ask the symbol something. For reads, anyone may; for anything that
    /// changes the book, only the market does — see the module docs.
    pub async fn ask<R: Send + 'static>(
        &self,
        f: impl FnOnce(&SymbolState) -> R + Send + 'static,
    ) -> Result<R, Gone> {
        self.actor.call(move |s| f(s)).await
    }

    /// Ask the symbol something, unless it has been delisted.
    pub async fn ask_listed<R: Send + 'static>(
        &self,
        f: impl FnOnce(&SymbolState) -> R + Send + 'static,
    ) -> Result<Option<R>, Gone> {
        self.actor.call(move |s| (!s.delisted).then(|| f(s))).await
    }

    /// Change the symbol. The market's to call, nobody else's.
    pub(crate) async fn change<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut SymbolState) -> R + Send + 'static,
    ) -> Result<R, Gone> {
        self.actor.call(f).await
    }
}

/// The symbol table, in listing order. Published by the market as an
/// immutable snapshot; a new one replaces it on every listing and
/// delisting.
#[derive(Default)]
pub struct Listings {
    symbols: Vec<Arc<Symbol>>,
}

impl Listings {
    /// Every listing, in order.
    pub fn all(&self) -> &[Arc<Symbol>] {
        &self.symbols
    }

    /// The listing for `ticker`, matched case-insensitively.
    pub fn get(&self, ticker: &str) -> Option<&Arc<Symbol>> {
        self.symbols
            .iter()
            .find(|s| s.ticker.eq_ignore_ascii_case(ticker))
    }

    pub fn len(&self) -> usize {
        self.symbols.len()
    }

    pub fn is_empty(&self) -> bool {
        self.symbols.is_empty()
    }
}

/// Who is who: the API keys, the service credentials, and who owns which
/// trader. Published by the market as an immutable snapshot; read on every
/// authenticated request, by the rate limiter and by every open stream
/// without sending anything.
#[derive(Clone, Debug, Default)]
pub struct Directory {
    keys: Keyring,
    services: Services,
    owners: BTreeMap<TraderId, UserId>,
    account_owners: BTreeMap<AccountId, UserId>,
}

impl Directory {
    /// The user `key` speaks for, if it is one of ours.
    pub fn user_of(&self, key: &str) -> Option<UserId> {
        self.keys.user_of(key)
    }

    /// The service `key` speaks for, if it is one of ours — revoked or not,
    /// so a credential that was taken away is refused as revoked rather than
    /// as unknown. See [`crate::service`].
    pub fn service_of(&self, key: &str) -> Option<ServiceAuth> {
        self.services.resolve(key)
    }

    /// The user `trader` belongs to, if the trader exists.
    pub fn owner_of(&self, trader: TraderId) -> Option<UserId> {
        self.owners.get(&trader).copied()
    }

    /// The user `account` belongs to, if the account exists.
    pub fn account_owner(&self, account: AccountId) -> Option<UserId> {
        self.account_owners.get(&account).copied()
    }
}

/// Why a symbol could not be listed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListingError {
    /// A symbol with this ticker is already listed. Delist it first.
    AlreadyListed(&'static str),
    /// The market already lists as many symbols as it will.
    Full { max: usize },
}

impl std::fmt::Display for ListingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyListed(t) => write!(f, "{t} is already listed"),
            Self::Full { max } => write!(f, "this market lists at most {max} symbols"),
        }
    }
}

impl std::error::Error for ListingError {}

/// Why takings could not be swept to treasury.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SweepError {
    /// No wallet by that id.
    UnknownWallet(WalletId),
    /// The wallet is not one that collects takings. Only an issuer's and the
    /// venue's are: a player's or an NPC's is somebody's money, a budget's
    /// is set aside on purpose, and treasury is where a sweep goes.
    NotTakings { wallet: WalletId, kind: WalletKind },
    /// Nothing to sweep, or more asked for than is there.
    Money(MoneyError),
}

impl std::fmt::Display for SweepError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownWallet(w) => write!(f, "no wallet {}", w.0),
            Self::NotTakings { wallet, kind } => write!(
                f,
                "wallet {} is a {} wallet, and only an issuer's or the venue's takings are swept",
                wallet.0,
                kind.label()
            ),
            Self::Money(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SweepError {}

/// Why a symbol could not be delisted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DelistError {
    /// No such symbol.
    Unknown,
    /// The buy-out price is not a price.
    Price,
    /// The issuer wallet cannot fund the buyout. Nothing was delisted.
    Unfunded(LedgerError),
}

impl std::fmt::Display for DelistError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown => write!(f, "no such symbol"),
            Self::Price => write!(f, "a buy-out price cannot be negative"),
            Self::Unfunded(e) => write!(f, "the buyout is not funded: {e}"),
        }
    }
}

impl std::error::Error for DelistError {}

/// Why a corporate payout — a dividend, a delisting buyout — was refused.
///
/// Both are funded: the currency comes out of the symbol's issuer wallet,
/// and if it is not there the payout does not happen. Nothing is paid to
/// some holders and not others, and nothing is clipped at a balance cap: an
/// obligation the issuer cannot meet is an obligation it still owes.
#[derive(Debug)]
pub enum PayoutError {
    /// The issuer wallet cannot fund it, or a holder cannot be paid.
    Unfunded(LedgerError),
    /// The market or a symbol actor is gone.
    Gone,
}

impl From<Gone> for PayoutError {
    fn from(_: Gone) -> Self {
        Self::Gone
    }
}

impl From<LedgerError> for PayoutError {
    fn from(e: LedgerError) -> Self {
        Self::Unfunded(e)
    }
}

impl std::fmt::Display for PayoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unfunded(e) => write!(f, "the payout is not funded: {e}"),
            Self::Gone => write!(f, "the market is shutting down"),
        }
    }
}

impl std::error::Error for PayoutError {}

/// What a delisting undid, and what it paid for the shares.
#[derive(Clone, Debug, Serialize)]
pub struct Delisting {
    pub symbol: &'static str,
    /// Paid on every share held. Zero is a real answer: a company can be
    /// worth nothing.
    pub cents_per_share: i64,
    /// What the symbol last traded at, for comparison.
    pub last_price_cents: i64,
    /// Resting orders withdrawn, releasing what they reserved.
    pub orders_cancelled: usize,
    /// Untriggered stops dropped. They reserved nothing.
    pub stops_cancelled: usize,
    pub shares_bought_out: u64,
    /// Accounts credited. A holder paid nothing — a buy-out at zero — is
    /// bought out but not credited.
    pub accounts_paid: usize,
    pub total_cents: i64,
}

/// What a dividend paid, and the price it went ex at.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Dividend {
    pub symbol: &'static str,
    pub cents_per_share: i64,
    /// The price the dividend was declared against, before it went ex.
    pub price_cents: i64,
    pub shares_paid: u64,
    /// Accounts credited. A user with two traders holding the same symbol is
    /// paid once per trader, into whichever account each trades on.
    pub accounts_paid: usize,
    pub total_cents: i64,
}

/// Why a submission could not be placed. The web layer turns these into
/// status codes; the engine turns them into a stop that fired and was
/// refused.
#[derive(Clone, Debug)]
pub enum PlaceError {
    /// The symbol is not listed (or was delisted a moment ago).
    UnknownSymbol,
    /// The symbol turned the order away before it reached the book.
    Check(OrderCheck),
    /// The account or the trader's shares would not fund it.
    Refused(Refused),
    /// The trader is not one this market knows.
    UnknownTrader(u64),
    /// The book itself refused the order.
    Invalid(String),
    /// The same `client_order_id` was already used for a different order.
    DuplicateClientId {
        client_order_id: String,
        order_id: u64,
    },
    /// The symbol's actor has stopped.
    Gone,
}

impl std::fmt::Display for PlaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownSymbol => write!(f, "no such symbol"),
            Self::Check(OrderCheck::Invalid(message)) | Self::Invalid(message) => {
                write!(f, "{message}")
            }
            Self::Check(OrderCheck::Closed(closed)) => write!(f, "{closed}"),
            Self::Check(OrderCheck::SelfTrade(ids)) => {
                write!(
                    f,
                    "the order would trade with the trader's own orders {ids:?}"
                )
            }
            Self::Check(OrderCheck::WouldCross {
                price_cents,
                best_cents,
            }) => write!(
                f,
                "a post-only order at {price_cents} would cross the market at {best_cents}"
            ),
            Self::Check(OrderCheck::UnknownOrder(id)) => write!(f, "no such order: {id}"),
            Self::Refused(e) => write!(f, "{e}"),
            Self::UnknownTrader(id) => write!(f, "no such trader: {id}"),
            Self::DuplicateClientId {
                client_order_id,
                order_id,
            } => write!(
                f,
                "client_order_id {client_order_id:?} was already used for order {order_id}"
            ),
            Self::Gone => write!(f, "the symbol has stopped"),
        }
    }
}

impl From<Gone> for PlaceError {
    fn from(_: Gone) -> Self {
        Self::Gone
    }
}

impl From<OrderCheck> for PlaceError {
    fn from(check: OrderCheck) -> Self {
        Self::Check(check)
    }
}

/// An order request as the market sees it: what the trader asked for,
/// already cleaned by the web layer.
#[derive(Clone, Debug)]
pub struct PlaceRequest {
    pub order: Order,
    pub client_order_id: Option<String>,
    pub post_only: bool,
    pub day: bool,
    pub expires_at_ms: Option<i64>,
    pub display_qty: Option<u64>,
    /// Write the order into the order log, so a client can look it up later.
    ///
    /// True for everything a client sent. False for an NPC's quotes, which
    /// nobody will ever ask after by id and which are redrawn often enough
    /// that logging them would evict every player's record within a minute.
    /// The book, the reservations and the audit do not read the log, so
    /// leaving them out of it changes nothing they check.
    pub logged: bool,
}

/// What placing an order came to.
#[derive(Clone, Debug)]
pub enum Placed {
    /// The order was sent, and this is what happened to it.
    New(OrderResponse),
    /// The same `client_order_id` had already been placed: this is the
    /// first response again, and nothing new happened.
    Replayed(OrderResponse),
}

impl Placed {
    pub fn response(&self) -> &OrderResponse {
        match self {
            Self::New(r) | Self::Replayed(r) => r,
        }
    }
}

/// An amendment as the market sees it: which resting order, and what it
/// should become.
#[derive(Clone, Debug)]
pub struct Amendment {
    pub order_id: u64,
    /// A new price, or the old one.
    pub price_cents: Option<i64>,
    /// A new quantity, or what was left of the old one.
    pub qty: Option<u64>,
    pub post_only: bool,
    pub client_order_id: Option<String>,
}

/// What an amendment came to.
#[derive(Clone, Debug)]
pub struct Amended {
    pub replaced_order_id: u64,
    /// How much of the withdrawn order had filled before it went.
    pub replaced_filled: u64,
    pub order: OrderResponse,
}

/// A symbol as a trader's records see it: what it is worth, and what the
/// trader has resting and armed on it.
#[derive(Clone, Debug)]
pub struct SymbolView {
    pub symbol: &'static str,
    /// The reference price, which positions are marked to.
    pub mark_cents: i64,
    /// This listing is a good rather than a company, so a position in it is
    /// inventory rather than a shareholding.
    pub is_good: bool,
    /// Every trader's resting orders.
    pub open_orders: Vec<OpenOrderDto>,
    /// Every trader's stops.
    pub stops: Vec<StopOrder>,
}

impl SymbolView {
    fn of(s: &SymbolState) -> Self {
        let sym = s.info.symbol;
        Self {
            symbol: sym,
            mark_cents: s.exchange.reference_cents(),
            is_good: s.info.is_good(),
            open_orders: s
                .exchange
                .book()
                .orders()
                .filter(|o| o.owner.trader().is_some())
                .map(|o| OpenOrderDto::from_resting(sym, o))
                .collect(),
            stops: s.stops.clone(),
        }
    }
}

/// The counts `GET /api/health` reports from the market.
#[derive(Clone, Debug, Default)]
pub struct MarketHealth {
    pub symbols: usize,
    pub ticks_total: u64,
    pub trades_total: u64,
    pub resting_orders: usize,
    pub stops_held: usize,
    pub users: usize,
    pub accounts: usize,
    pub traders: usize,
    pub cash_cents: i64,
    pub orders_placed: u64,
    pub orders_refused: u64,
    pub fills_booked: u64,
    /// Fills the book made that the ledger then refused to settle, since
    /// start-up. Always zero in a healthy market: everything settlement
    /// needs is made true before it is called, because by then the book has
    /// already traded and cannot be unwound. A number above zero means
    /// shares moved and money did not, and the market should be reconciled.
    pub settlement_failures: u64,
    pub events_logged: usize,
}

/// The wallets a world has exactly one of, whatever it lists or who signs
/// up.
///
/// Everything else — a player's wallet, a symbol's issuer — is opened as it
/// is needed and found through the thing that owns it. These four are found
/// here because nothing owns them: they are the world.
#[derive(Clone, Copy, Debug, Serialize, serde::Deserialize)]
pub struct Wallets {
    /// The mint and burn control account.
    pub issuance: WalletId,
    /// Where the genesis supply sits, and what the faucet funds new accounts
    /// out of.
    pub treasury: WalletId,
    /// Where fees collect. Nothing leaves the world through a fee.
    pub venue: WalletId,
    /// The counterparty for fills against the simulator's unfunded
    /// liquidity. See [`WalletKind::Synthetic`].
    pub synthetic: WalletId,
}

impl Default for Wallets {
    /// The ids [`Wallets::open`] hands out in a fresh ledger.
    ///
    /// Only for building an empty [`MarketSave`], which is then filled in;
    /// a world's real wallet ids come from its own ledger, and the two agree
    /// because opening them is the first thing a world does.
    fn default() -> Self {
        Self {
            issuance: WalletId(1),
            treasury: WalletId(2),
            venue: WalletId(3),
            synthetic: WalletId(4),
        }
    }
}

impl Wallets {
    /// Open the four in a fresh ledger, in a fixed order so that a world
    /// built from the same options twice numbers them the same way.
    fn open(ledger: &mut Ledger) -> Self {
        Self {
            issuance: ledger.issuance_wallet(),
            treasury: ledger.open(WalletKind::Treasury),
            venue: ledger.open(WalletKind::Venue),
            synthetic: ledger.open(WalletKind::Synthetic),
        }
    }
}

/// `GET /api/supply`: how much currency exists, and where it sits.
#[derive(Clone, Debug, Default, Serialize)]
pub struct SupplyDto {
    /// Created since genesis, genesis included.
    pub minted_cents: i64,
    /// Destroyed since.
    pub burned_cents: i64,
    /// `minted − burned`.
    pub outstanding_cents: i64,
    /// What the wallets actually hold. Equal to `outstanding_cents` unless
    /// something is wrong, which `balanced` says.
    pub circulating_cents: i64,
    /// The two agree: currency is conserved.
    pub balanced: bool,
    /// Waiting in treasury to be handed out.
    pub treasury_cents: i64,
    /// Collected in fees.
    pub venue_cents: i64,
    /// Set aside to fund dividends and buyouts, across every symbol.
    pub issuer_cents: i64,
    /// Held by players, across every account.
    pub player_cents: i64,
    /// Sitting in the tills of the traders the world runs itself.
    pub npc_cents: i64,
    /// Set aside in budgets, waiting to be paid out as rewards.
    pub budget_cents: i64,
    /// What unfunded liquidity has put into players' hands beyond its float.
    pub synthetic_debt_cents: i64,
    /// Wallets open, the four the world always has included.
    pub wallets: usize,
}

impl SupplyDto {
    /// Read it off a market.
    pub fn of(m: &Market) -> Self {
        let supply = m.ledger.supply();
        let circulating = m.ledger.circulating_cents();
        let by_kind = |kind: WalletKind| {
            m.ledger
                .wallets()
                .filter(|w| w.kind == kind)
                .map(|w| w.balance_cents())
                .fold(0i64, i64::saturating_add)
        };
        Self {
            minted_cents: supply.minted_cents,
            burned_cents: supply.burned_cents,
            outstanding_cents: supply.outstanding_cents(),
            circulating_cents: i64::try_from(circulating).unwrap_or(i64::MAX),
            balanced: circulating == i128::from(supply.outstanding_cents()),
            treasury_cents: by_kind(WalletKind::Treasury),
            venue_cents: by_kind(WalletKind::Venue),
            issuer_cents: by_kind(WalletKind::Issuer),
            player_cents: by_kind(WalletKind::Player),
            npc_cents: by_kind(WalletKind::Npc),
            budget_cents: by_kind(WalletKind::Budget),
            synthetic_debt_cents: i64::try_from(m.ledger.synthetic_debt_cents())
                .unwrap_or(i64::MAX),
            wallets: m.ledger.len(),
        }
    }
}

/// One wallet in the directory the operator reads: what it holds, and whose
/// it is.
///
/// Distinct from `WalletDto`, which is one wallet answering for itself. This
/// is a row in a list of all of them, so it carries the one thing a list
/// needs and a single read does not: a name to show instead of a number.
#[derive(Clone, Debug, Serialize)]
pub struct WalletRow {
    pub wallet: WalletId,
    /// `player`, `treasury`, `budget`, `npc`, `issuer`, `venue`, `issuance`
    /// or `synthetic`.
    pub kind: &'static str,
    pub status: &'static str,
    pub balance_cents: i64,
    /// Committed to resting buy orders.
    pub reserved_cents: i64,
    /// `balance − reserved`: what can still be spent.
    pub available_cents: i64,
    /// The account whose money this is, if it is anybody's.
    pub account_id: Option<u64>,
    /// What to call it on a screen: the account's name, the merchant's, the
    /// budget's, or the symbol whose payouts it funds. `null` for the four
    /// wallets the world always has, whose kind is their name.
    pub owner: Option<String>,
}

/// What one [`Reason`] has moved since genesis.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct FlowDto {
    /// The reason's stable label: `mint`, `reward`, `fee`, `buy`, …
    pub reason: &'static str,
    /// Transactions posted with it.
    pub count: u64,
    /// What they moved, in cents.
    pub cents: i64,
}

/// What is in the furnace, and what has come out of it.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct JobsSummary {
    /// Jobs the book holds, running and finished.
    pub held: usize,
    pub running: usize,
    pub done: usize,
    pub cancelled: usize,
    /// When the next running job is due, or `null` if none is.
    pub next_due_ms: Option<i64>,
}

/// Who is in the world, and how many of them cannot move money.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct PeopleSummary {
    pub users: usize,
    pub accounts: usize,
    pub traders: usize,
    /// Players the game backend has provisioned.
    pub players: usize,
    /// Accounts whose wallet is frozen, and whose is closed. A frozen wallet
    /// still takes credits; a closed one is finished.
    pub frozen: usize,
    pub closed: usize,
}

/// `GET /api/economy/overview`: the whole economy in one consistent read.
///
/// One market job, so every number in it was true at the same instant —
/// which is the point of having it rather than the eight requests it
/// replaces. A dashboard that assembled supply, wallets, budgets and the
/// world separately would show a reward that had left its budget and not yet
/// arrived in a wallet, and an operator would be right not to trust it.
///
/// Everything here is cheap: counters, balances and clones of lists the
/// market already holds. Nothing that walks the books belongs in it —
/// [`crate::reconcile`] takes a snapshot of the whole world and is asked for
/// deliberately, not polled.
#[derive(Clone, Debug, Serialize)]
pub struct OverviewDto {
    /// Simulated time the reading was taken at.
    pub at_ms: i64,
    pub supply: SupplyDto,
    /// Every reason, in a fixed order, whether it has moved anything or not.
    pub flows: Vec<FlowDto>,
    /// Every wallet, in id order.
    pub wallets: Vec<WalletRow>,
    pub budgets: Vec<BudgetDto>,
    /// What a named reward is worth.
    pub rules: Vec<RewardRule>,
    pub npcs: Vec<NpcDto>,
    /// What events are doing to production and demand, per symbol, now.
    pub effects: Vec<SymbolEffects>,
    /// The modifiers behind those numbers, still in force.
    pub modifiers: Vec<crate::world::Modifier>,
    pub jobs: JobsSummary,
    pub people: PeopleSummary,
    /// Where the game backend has read to.
    pub outbox: crate::outbox::Cursor,
    /// Commands applied since the world began.
    pub journal_seq: u64,
}

/// The market: everybody's money, every order ever sent, and the symbols
/// those orders went to. One actor; see the module docs for why.
pub struct Market {
    symbols: Vec<Arc<Symbol>>,
    listings_tx: watch::Sender<Arc<Listings>>,
    directory: Directory,
    directory_tx: watch::Sender<Arc<Directory>>,
    stream: Stream,
    clock: SimClock,
    halts: HaltPolicy,
    max_symbols: usize,
    /// Most recent events, oldest first.
    events: VecDeque<EventRecord>,
    event_cap: usize,
    next_event_id: u64,
    /// Every wallet in the world and the supply behind them. The one
    /// authority on who holds what: an account is an identity and a history,
    /// and its balance is a wallet in here.
    pub ledger: Ledger,
    /// The four wallets the world always has.
    pub wallets: Wallets,
    /// Per symbol, the wallet its dividends and its delisting buyout are
    /// paid out of. A payout it cannot fund is refused, never clipped. For a
    /// good it is where the money paid for its units lands.
    pub issuers: BTreeMap<&'static str, WalletId>,
    /// What the world will make, and for how much. See [`crate::catalog`].
    pub catalog: Catalog,
    /// The traders the world runs itself, by trader id. See [`crate::npc`].
    pub npcs: BTreeMap<TraderId, Npc>,
    /// What the world knows how to make. See [`crate::jobs`].
    pub recipes: RecipeBook,
    /// What is in the furnace, and what came out of it.
    pub jobs: JobBook,
    /// The pools rewards are paid from, the rules that price them, and every
    /// game event id already paid. See [`crate::rewards`].
    pub rewards: RewardBook,
    /// What game events are doing to production and demand right now. See
    /// [`crate::world`].
    pub world: WorldEffects,
    /// Settlements the ledger refused after the book had already traded.
    /// Always zero in a healthy market; see [`Market::book`].
    pub settlement_failures: u64,
    /// The players the game backend has provisioned, by the external id it
    /// knows each of them by. See [`Market::provision_player`].
    pub players: BTreeMap<String, Player>,
    next_service_id: u64,
    pub users: BTreeMap<UserId, User>,
    next_user_id: u64,
    pub accounts: BTreeMap<AccountId, Account>,
    next_account_id: u64,
    pub traders: BTreeMap<TraderId, Trader>,
    next_trader_id: u64,
    /// Ids for the stops held on the symbols. Separate from order ids: a
    /// stop only becomes an order when it fires.
    next_stop_id: u64,
    /// The lowest id the next trader order may take, in any book. See the
    /// module docs.
    next_order_id: fehu::OrderId,
    /// Every order the log still holds, by order id.
    orders: BTreeMap<u64, OrderRecord>,
    /// Order ids in the order they were accepted, for eviction.
    order_ids: VecDeque<u64>,
    /// `(trader, client_order_id)` → order id, for idempotent submission.
    client_order_ids: BTreeMap<(TraderId, String), u64>,
    fill_log: usize,
    ledger_log: usize,
    order_log: usize,
    /// Orders sent to a book since start-up, and submissions turned away
    /// before they got there. Counters, not state: they are not saved, and a
    /// restart begins them again.
    pub orders_placed: u64,
    pub orders_refused: u64,
    /// Fills booked to traders' accounts since start-up.
    pub fills_booked: u64,
    /// What the venue charges for a fill.
    pub fees: Fees,
    /// What a newly listed symbol's issuer wallet is funded with.
    issuer_float_cents: i64,
    /// The options this world was built with. Held here as well as on
    /// [`App`] because a command is applied by the market and has to be
    /// applied under the same settings whether it arrives now or is
    /// replayed at start-up.
    options: Options,
    /// The simulated instant the command being applied was accepted at, or
    /// `None` when nothing is being applied. See [`Market::now`].
    pub(crate) pinned_now: Option<Timestamp>,
    /// Every accepted command since the last snapshot, on disk.
    pub journal: crate::journal::Journal,
    /// What each `Idempotency-Key` answered, so a retry is not a second
    /// command.
    pub commands: crate::journal::CommandLog,
    /// The facts a game backend reads at its own pace. See
    /// [`crate::outbox`].
    pub outbox: crate::outbox::Outbox,
    /// Facts published by the command being applied, waiting for it to be
    /// journaled. Emptied into the outbox once it is, and thrown away if it
    /// is refused: scratch, never saved.
    pending_facts: Vec<(String, serde_json::Value)>,
}

// ---------------------------------------------------------------------------
// The clearing half: people, money, the order log. Synchronous; nothing here
// touches a symbol.

impl Market {
    fn publish_directory(&self) {
        self.directory_tx
            .send_replace(Arc::new(self.directory.clone()));
    }

    fn publish_listings(&self) {
        self.listings_tx.send_replace(Arc::new(Listings {
            symbols: self.symbols.clone(),
        }));
    }

    /// The listing for `ticker`, matched case-insensitively.
    pub fn symbol(&self, ticker: &str) -> Option<&Arc<Symbol>> {
        self.symbols
            .iter()
            .find(|s| s.ticker.eq_ignore_ascii_case(ticker))
    }

    /// Every listing, in order.
    pub fn symbols(&self) -> &[Arc<Symbol>] {
        &self.symbols
    }

    /// Register a user under the API key digest generated for them.
    ///
    /// `name` and `email` are trimmed and truncated; an empty name becomes
    /// `user-{id}`. The credential itself never reaches the market: it is
    /// generated by the route, returned once in the response that created
    /// the user, and only its digest is stored, journaled and saved.
    pub fn create_user(
        &mut self,
        name: Option<String>,
        email: Option<String>,
        now_ms: i64,
        key_digest: String,
    ) -> UserId {
        let id = self.insert_user(name, email, now_ms);
        self.directory.keys.install(id, key_digest);
        self.publish_directory();
        id
    }

    /// Register a user nobody can sign in as.
    ///
    /// An NPC needs an identity — a trader belongs to a user, and every
    /// audit walks that link — but it has no player behind it, so it is
    /// given no key at all. There is then no credential for it to leak, and
    /// no request can ever arrive claiming to be it.
    pub fn create_house_user(&mut self, name: Option<String>, now_ms: i64) -> UserId {
        let id = self.insert_user(name, None, now_ms);
        self.publish_directory();
        id
    }

    fn insert_user(&mut self, name: Option<String>, email: Option<String>, now_ms: i64) -> UserId {
        let id = UserId(self.next_user_id);
        self.next_user_id += 1;
        let user = User {
            id,
            name: clean(name, 64).unwrap_or_else(|| format!("user-{}", id.0)),
            email: clean(email, 254),
            created_at_ms: now_ms,
            accounts: Vec::new(),
        };
        self.users.insert(id, user);
        id
    }

    /// Open an account for `user_id` and pay it `cash_cents` out of treasury.
    ///
    /// The account itself opens empty and is then funded by the faucet, which
    /// is a *transfer*: opening an account moves currency that already
    /// exists rather than creating any, so no route a player can reach
    /// changes the supply. A treasury that cannot cover the request refuses
    /// it — a dry faucet is an operator's problem, and saying so is better
    /// than printing money or quietly handing out less than was asked for.
    pub fn open_account(
        &mut self,
        user_id: UserId,
        name: Option<String>,
        cash_cents: i64,
        now_ms: i64,
    ) -> Result<AccountId, MoneyError> {
        self.open_account_as(user_id, name, cash_cents, now_ms, WalletKind::Player)
    }

    /// [`Market::open_account`], saying what kind of wallet holds the money.
    ///
    /// The kind is not a permission — every wallet but issuance obeys the
    /// same rules — it is what lets an audit say *where* the world's
    /// currency is sitting, and a shop till is not a player's pocket.
    fn open_account_as(
        &mut self,
        user_id: UserId,
        name: Option<String>,
        cash_cents: i64,
        now_ms: i64,
        kind: WalletKind,
    ) -> Result<AccountId, MoneyError> {
        debug_assert!(self.users.contains_key(&user_id), "unknown user");
        if cash_cents < 0 {
            return Err(MoneyError::NotPositive {
                amount_cents: cash_cents,
            });
        }
        if cash_cents > 0 {
            check_transfer(cash_cents)?;
            // Before the account exists, so a refusal leaves nothing behind.
            let available = self.ledger.available(self.wallets.treasury);
            if cash_cents > available {
                return Err(MoneyError::Ledger(LedgerError::Insufficient {
                    wallet: self.wallets.treasury,
                    needed_cents: cash_cents,
                    available_cents: available,
                }));
            }
        }
        let id = AccountId(self.next_account_id);
        let wallet = self.ledger.open(kind);
        let account = Account::open(
            id,
            user_id,
            clean(name, 64).unwrap_or_else(|| format!("account-{}", id.0)),
            wallet,
            self.ledger_log,
            now_ms,
        );
        self.next_account_id += 1;
        self.accounts.insert(id, account);
        self.directory.account_owners.insert(id, user_id);
        self.publish_directory();
        if let Some(user) = self.users.get_mut(&user_id) {
            user.accounts.push(id);
        }
        if cash_cents > 0 {
            self.fund_account(id, cash_cents, Some("opening balance".into()), now_ms)?;
        }
        Ok(id)
    }

    /// Pay `cents` into `account` out of treasury.
    ///
    /// The faucet. It moves currency rather than making it, so it can run
    /// dry, and when it does it says so.
    pub fn fund_account(
        &mut self,
        account: AccountId,
        cents: i64,
        memo: Option<String>,
        now_ms: i64,
    ) -> Result<LedgerEntry, MoneyError> {
        let treasury = self.wallets.treasury;
        self.move_in(account, Reason::Faucet, treasury, cents, memo, now_ms)
    }

    /// Create `cents` and put them in `account`. Operator authority: this is
    /// the only way currency enters the world after genesis.
    pub fn mint_into(
        &mut self,
        account: AccountId,
        cents: i64,
        memo: Option<String>,
        now_ms: i64,
    ) -> Result<LedgerEntry, MoneyError> {
        check_transfer(cents)?;
        let wallet = self.wallet_of(account)?;
        let tx = self.ledger.mint(wallet, cents, Reason::Mint)?;
        Ok(self.write_entry(account, LedgerKind::Deposit, tx.id, cents, memo, now_ms))
    }

    /// Destroy `cents` out of `account`. Operator authority: the only way
    /// currency leaves the world.
    pub fn burn_from(
        &mut self,
        account: AccountId,
        cents: i64,
        memo: Option<String>,
        now_ms: i64,
    ) -> Result<LedgerEntry, MoneyError> {
        check_transfer(cents)?;
        let wallet = self.wallet_of(account)?;
        let tx = self.ledger.burn(wallet, cents)?;
        Ok(self.write_entry(account, LedgerKind::Withdrawal, tx.id, -cents, memo, now_ms))
    }

    /// Move `cents` from one wallet into `account`, and write the row.
    fn move_in(
        &mut self,
        account: AccountId,
        reason: Reason,
        from: WalletId,
        cents: i64,
        memo: Option<String>,
        now_ms: i64,
    ) -> Result<LedgerEntry, MoneyError> {
        // What the account's history calls a movement follows from why it
        // happened, so the caller does not get to say both.
        let kind = match reason {
            Reason::Burn => LedgerKind::Withdrawal,
            Reason::Dividend => LedgerKind::Dividend,
            Reason::Delisting => LedgerKind::Delisting,
            _ => LedgerKind::Deposit,
        };
        check_transfer(cents)?;
        let wallet = self.wallet_of(account)?;
        let tx = self
            .ledger
            .post(Draft::new(reason).debit(from, cents).credit(wallet, cents))?;
        Ok(self.write_entry(account, kind, tx.id, cents, memo, now_ms))
    }

    /// The wallet `account`'s money lives in.
    fn wallet_of(&self, account: AccountId) -> Result<WalletId, MoneyError> {
        self.accounts
            .get(&account)
            .map(|a| a.wallet)
            .ok_or(MoneyError::NoWallet { account })
    }

    /// Write an account's view of a transaction that has already posted.
    fn write_entry(
        &mut self,
        account: AccountId,
        kind: LedgerKind,
        tx_id: u64,
        amount_cents: i64,
        memo: Option<String>,
        now_ms: i64,
    ) -> LedgerEntry {
        let Self {
            ledger, accounts, ..
        } = self;
        accounts.get_mut(&account).expect("resolved above").record(
            ledger,
            kind,
            tx_id,
            amount_cents,
            now_ms,
            None,
            None,
            memo,
        )
    }

    /// The wallet a symbol's dividends and buyout are paid out of, opening
    /// one the first time it is asked for.
    pub fn issuer_wallet(&mut self, symbol: &'static str) -> WalletId {
        if let Some(id) = self.issuers.get(symbol) {
            return *id;
        }
        let id = self.ledger.open(WalletKind::Issuer);
        self.issuers.insert(symbol, id);
        id
    }

    /// Create a trader for `user_id` that trades on `account_id`.
    pub fn create_trader(
        &mut self,
        user_id: UserId,
        account_id: AccountId,
        name: Option<String>,
        now_ms: i64,
    ) -> TraderId {
        let id = TraderId(self.next_trader_id);
        self.next_trader_id += 1;
        let name = clean(name, 64).unwrap_or_else(|| format!("trader-{}", id.0));
        self.traders.insert(
            id,
            Trader::new(id, user_id, account_id, name, self.fill_log, now_ms),
        );
        self.directory.owners.insert(id, user_id);
        self.publish_directory();
        id
    }

    /// The one-call sign-up behind `POST /api/traders`: a user, an account
    /// funded out of treasury with `cash_cents`, and a trader that trades on
    /// it.
    pub fn sign_up(
        &mut self,
        name: Option<String>,
        email: Option<String>,
        cash_cents: i64,
        now_ms: i64,
        key_digest: String,
    ) -> Result<TraderId, MoneyError> {
        let user = self.create_user(name.clone(), email, now_ms, key_digest);
        let account = self.open_account(user, name.clone(), cash_cents, now_ms)?;
        Ok(self.create_trader(user, account, name, now_ms))
    }

    /// The credentials the game backend speaks with.
    ///
    /// They live in the published [`Directory`] rather than beside it, for
    /// the same reason the API keys do: every request that presents one is
    /// authorised from the published snapshot without sending a job
    /// anywhere, so a second copy here would be a second thing to keep in
    /// step and the one a stale request would read.
    pub fn services(&self) -> &Services {
        &self.directory.services
    }

    /// Issue a service credential: a key the game backend speaks with that
    /// carries `scopes` and nothing else. See [`crate::service`].
    ///
    /// The key itself never reaches here. The route that issues one
    /// generates it, journals the digest and returns the only copy, exactly
    /// as a user's sign-up does.
    pub fn create_service(
        &mut self,
        name: &str,
        scopes: ScopeSet,
        digest: String,
        now_ms: i64,
    ) -> Result<ServiceId, ServiceError> {
        let name = crate::service::clean_name(name)?;
        if scopes.is_empty() {
            return Err(ServiceError::NoScopes);
        }
        let id = ServiceId(self.next_service_id);
        self.next_service_id += 1;
        self.directory.services.install(Service {
            id,
            name,
            scopes,
            digest,
            revoked: false,
            created_ms: now_ms,
            revoked_ms: None,
        });
        self.publish_directory();
        Ok(id)
    }

    /// Take a service's key away. Returns whether there was such a service;
    /// revoking a revoked one changes nothing and is not an error.
    pub fn revoke_service(&mut self, id: ServiceId, now_ms: i64) -> bool {
        let revoked = self.directory.services.revoke(id, now_ms);
        if revoked {
            self.publish_directory();
        }
        revoked
    }

    /// Map a player the game already has onto a user, an account and a
    /// trader, and hand back what was created — or what already was.
    ///
    /// Idempotent on `external_id`: the second call for a player finds the
    /// mapping and creates nothing, so the backend may provision on every
    /// login without keeping a record of whether it has. The account opens
    /// empty, because currency is minted by the operator and paid as
    /// rewards; nothing about arriving in the world creates any.
    ///
    /// `Err` is only a name or an id the world would not take; a repeat is
    /// `Ok` with `created` false.
    pub fn provision_player(
        &mut self,
        external_id: &str,
        name: Option<String>,
        email: Option<String>,
        now_ms: i64,
        key_digest: String,
    ) -> Result<(Player, bool), MoneyError> {
        let external_id = external_id.to_owned();
        if let Some(player) = self.players.get(&external_id) {
            return Ok((player.clone(), false));
        }
        let user = self.create_user(name.clone(), email, now_ms, key_digest);
        let account = self.open_account(user, name.clone(), 0, now_ms)?;
        let trader = self.create_trader(user, account, name, now_ms);
        let wallet = self
            .accounts
            .get(&account)
            .map(|a| a.wallet)
            .ok_or(MoneyError::NoWallet { account })?;
        let player = Player {
            external_id: external_id.clone(),
            user_id: user,
            account_id: account,
            trader_id: trader,
            wallet,
            created_at_ms: now_ms,
        };
        self.players.insert(external_id, player.clone());
        Ok((player, true))
    }

    /// The player the game knows by `external_id`, if this world has them.
    pub fn player(&self, external_id: &str) -> Option<&Player> {
        self.players.get(external_id)
    }

    /// The account a trader trades on.
    pub fn account_of(&self, trader: TraderId) -> Option<&Account> {
        let t = self.traders.get(&trader)?;
        self.accounts.get(&t.account_id)
    }

    /// The trader that trades on `account`, if there is one.
    pub fn trader_on(&self, account: AccountId) -> Option<&Trader> {
        self.traders.values().find(|t| t.account_id == account)
    }

    /// A trader and its account, both mutable.
    pub fn trader_and_account(&mut self, trader: TraderId) -> Option<(&mut Trader, &mut Account)> {
        let Self {
            traders, accounts, ..
        } = self;
        let t = traders.get_mut(&trader)?;
        let a = accounts.get_mut(&t.account_id)?;
        Some((t, a))
    }

    /// A trader, its account and the ledger, all three at once.
    ///
    /// Every money movement needs all of them — the ledger to post to, the
    /// account to write the row in, the trader for the position — and they
    /// live in three fields of one struct, so this is the borrow that lets a
    /// caller have them together.
    fn settling(&mut self, trader: TraderId) -> Option<(&mut Trader, &mut Account, &mut Ledger)> {
        let Self {
            traders,
            accounts,
            ledger,
            ..
        } = self;
        let t = traders.get_mut(&trader)?;
        let a = accounts.get_mut(&t.account_id)?;
        Some((t, a, ledger))
    }

    /// Shares of `sym` the traders hold between them.
    pub fn held_shares(&self, sym: &str) -> u64 {
        self.traders
            .values()
            .map(|t| t.held_shares(sym))
            .fold(0, u64::saturating_add)
    }

    /// Shares of `sym` that `user` could sell right now: what their traders
    /// hold, less what their resting sells already promised.
    pub fn user_free_shares(&self, user: UserId, sym: &str) -> u64 {
        self.traders
            .values()
            .filter(|t| t.user_id == user)
            .map(|t| t.free_shares(sym))
            .fold(0, u64::saturating_add)
    }

    /// What `user` owns, one entry per symbol, added up across every trader
    /// of theirs and marked to `marks`. Ordered by ticker.
    pub fn user_holdings(&self, user: UserId, marks: &[SymbolView]) -> Vec<HoldingDto> {
        let mut by_symbol: BTreeMap<&'static str, HoldingDto> = BTreeMap::new();
        for trader in self.traders.values().filter(|t| t.user_id == user) {
            for sym in trader.positions.keys() {
                by_symbol
                    .entry(sym)
                    .or_insert_with(|| HoldingDto::empty(sym))
                    .add(trader);
            }
        }
        by_symbol
            .into_values()
            .map(|mut h| {
                h.mark(
                    marks
                        .iter()
                        .find(|v| v.symbol == h.symbol)
                        .map_or(0, |v| v.mark_cents),
                );
                h
            })
            .collect()
    }

    /// One trader's positions, marked, with what each one has promised to a
    /// resting sell.
    ///
    /// `goods_only` is what an inventory is: the units a player is holding,
    /// as opposed to the companies they own a piece of. Both are the same
    /// map and the same reservations — see [`crate::symbol::AssetKind`] —
    /// so this is a filter rather than a second table.
    pub fn trader_holdings(
        &self,
        trader: TraderId,
        marks: &[SymbolView],
        goods_only: bool,
    ) -> Vec<HoldingDto> {
        let Some(t) = self.traders.get(&trader) else {
            return Vec::new();
        };
        t.positions
            .keys()
            .filter(|sym| {
                !goods_only
                    || marks
                        .iter()
                        .find(|v| v.symbol == **sym)
                        .is_some_and(|v| v.is_good)
            })
            .map(|sym| {
                let mut holding = HoldingDto::empty(sym);
                holding.add(t);
                holding.mark(
                    marks
                        .iter()
                        .find(|v| v.symbol == *sym)
                        .map_or(0, |v| v.mark_cents),
                );
                holding
            })
            .collect()
    }

    /// One order by id, whatever became of it — as long as the log still
    /// holds it.
    pub fn order(&self, order_id: u64) -> Option<&OrderRecord> {
        self.orders.get(&order_id)
    }

    /// A trader's orders, newest first.
    pub fn orders_of(&self, trader: TraderId) -> impl Iterator<Item = &OrderRecord> {
        self.order_ids
            .iter()
            .rev()
            .filter_map(|id| self.orders.get(id))
            .filter(move |o| o.trader_id == trader.0)
    }

    /// The order a `client_order_id` was already used for, if any. A repeat
    /// submission is answered from this rather than sent to the book again.
    pub fn order_by_client_id(&self, trader: TraderId, client_id: &str) -> Option<&OrderRecord> {
        self.client_order_ids
            .get(&(trader, client_id.to_string()))
            .and_then(|id| self.orders.get(id))
    }

    /// Add an accepted order to the log, evicting the oldest finished ones
    /// once it is over `order_log`. Live orders are never evicted.
    pub fn record_order(&mut self, record: OrderRecord) {
        let id = record.order_id;
        if let Some(client_id) = record.client_order_id.clone() {
            self.client_order_ids
                .insert((TraderId(record.trader_id), client_id), id);
        }
        self.orders.insert(id, record);
        self.order_ids.push_back(id);
        while self.orders.len() > self.order_log {
            let Some(pos) = self
                .order_ids
                .iter()
                .position(|id| self.orders.get(id).is_some_and(|o| !o.is_live()))
            else {
                break; // Everything still in the log is live: keep it all.
            };
            let Some(id) = self.order_ids.remove(pos) else {
                break;
            };
            if let Some(old) = self.orders.remove(&id)
                && let Some(client_id) = old.client_order_id
            {
                self.client_order_ids
                    .remove(&(TraderId(old.trader_id), client_id));
            }
        }
    }

    /// Give back what a withdrawn order reserved and mark its record
    /// cancelled.
    fn release(&mut self, trader: TraderId, sym: &'static str, cancelled: &Resting, now_ms: i64) {
        if let Some((t, account, ledger)) = self.settling(trader) {
            t.release(
                ledger,
                account,
                sym,
                cancelled.side,
                cancelled.outstanding(),
                cancelled.price_cents,
            );
        }
        if let Some(record) = self.orders.get_mut(&cancelled.id.0)
            && record.symbol == sym
        {
            record.cancel(cancelled.outstanding(), now_ms);
        }
    }

    /// Book every trade in `trades` (for symbol `sym`): settle the money,
    /// update the traders involved, publish the fills. Returns how many there
    /// were.
    ///
    /// **One transaction per trade, not one per party.** A fill moves
    /// currency between the two sides and the venue at once, and posting it
    /// per party would move it twice. Whichever side is not a trader — the
    /// simulator's ladder, its printed flow — posts against the synthetic
    /// wallet, so the transaction balances and the currency that unfunded
    /// liquidity puts into a player's hands is visible as that wallet's debt
    /// instead of appearing from nowhere.
    fn book(&mut self, sym: &'static str, trades: &[Trade]) -> Vec<FillRecord> {
        let fees = self.fees;
        let mut fills = Vec::new();
        for t in trades {
            // A resting order that trades has moved on since it was accepted.
            for party in [&t.taker, &t.maker] {
                if party.owner.trader().is_some()
                    && let Some(record) = self.orders.get_mut(&party.order.0)
                    && record.symbol == sym
                {
                    record.fill(t.qty, t.price_cents, t.ts.0);
                }
            }
            // Give back what the resting side reserved *before* settling it.
            // The cash a resting buy holds back is exactly the cash that pays
            // for its own fill, so a trader who committed their whole balance
            // to an order would look insolvent at the moment it filled — and
            // the settlement would be refused after the book had traded.
            if let Some(id) = t.maker.owner.trader()
                && let Some((trader, account, ledger)) = self.settling(id)
            {
                let side = t.taker_side.opposite();
                trader.release(ledger, account, sym, side, t.qty, t.price_cents);
            }
            let Some((tx_id, settled)) = self.settle_trade(sym, t, fees) else {
                continue;
            };
            let mut parties = [t.taker.owner.trader(), t.maker.owner.trader()];
            if parties[0] == parties[1] {
                parties[1] = None;
            }
            for id in parties.into_iter().flatten() {
                if let Some((trader, account, ledger)) = self.settling(id) {
                    fills.extend(trader.apply_trade(ledger, account, sym, t, settled, tx_id));
                }
            }
        }
        self.fills_booked = self.fills_booked.saturating_add(fills.len() as u64);
        for fill in fills.clone() {
            self.announce(StreamMessage::Fill {
                trader_id: fill.trader_id,
                fill,
            });
        }
        fills
    }

    /// Move the currency one trade owes, as a single balanced transaction:
    /// the buyer's side, the seller's side, and the venue's cut of each.
    /// Returns the transaction id, or `None` if the ledger refused it.
    ///
    /// A refusal here is a bug, not a case to handle: the book has already
    /// traded and cannot be unwound, so the shares have moved and the money
    /// has not. Everything that could cause one is prevented upstream — a
    /// buy is authorised with its fee before it is submitted, a resting buy
    /// holds a reservation, and freezing an account cancels its resting
    /// orders in the same job. What is left is the balance cap on a very
    /// large sell. So this counts the refusal, logs it loudly and leaves the
    /// drift for [`crate::reconcile`] to report, rather than papering over it
    /// with currency nobody minted.
    fn settle_trade(
        &mut self,
        sym: &'static str,
        trade: &Trade,
        fees: Fees,
    ) -> Option<(u64, SettledFees)> {
        let synthetic = self.wallets.synthetic;
        let wallet_of = |owner: Owner| match owner.trader() {
            Some(id) => self
                .traders
                .get(&id)
                .and_then(|t| self.accounts.get(&t.account_id))
                .map_or(synthetic, |a| a.wallet),
            None => synthetic,
        };
        let taker = wallet_of(trade.taker.owner);
        let maker = wallet_of(trade.maker.owner);
        let value = match fehu::ledger::checked_notional_cents(trade.price_cents, trade.qty) {
            Ok(value) => value,
            Err(e) => {
                self.settlement_failures = self.settlement_failures.saturating_add(1);
                tracing::error!(symbol = sym, error = %e, "fill notional does not fit");
                return None;
            }
        };
        // Only a real account is charged: synthetic liquidity has no wallet
        // of its own to pay out of, and a fee it "paid" would be currency
        // conjured into the venue's.
        let fee_for = |owner: Owner, liquidity| {
            if owner.trader().is_some() {
                fees.on(liquidity, value)
            } else {
                0
            }
        };
        let taker_fee = fee_for(trade.taker.owner, Liquidity::Taker);
        let venue = self.wallets.venue;
        // A rebate comes out of what the venue has taken, and no further.
        // Nothing else caps it: a taker fee smaller than the maker rebate,
        // or a taker with no wallet to charge, would otherwise ask the venue
        // to pay currency it never collected — which is the same money from
        // nowhere that fees leaving the world used to be, pointing the other
        // way. What it cannot pay, it does not pay.
        let purse = self
            .ledger
            .available(venue)
            .saturating_add(-taker_fee)
            .max(0);
        let maker_fee = fee_for(trade.maker.owner, Liquidity::Maker).min(purse);
        let settled = SettledFees {
            taker_cents: taker_fee,
            maker_cents: maker_fee,
        };
        if maker_fee < fee_for(trade.maker.owner, Liquidity::Maker) {
            tracing::warn!(
                symbol = sym,
                purse,
                "maker rebate capped at what the venue has collected"
            );
        }
        let draft = settlement_draft(
            taker,
            maker,
            venue,
            trade.taker_side,
            value,
            taker_fee,
            trade.ts.0,
        )
        // The maker's fee is the other half of the same transaction: its
        // rebate comes out of the venue's takings, not out of thin air.
        .posting(maker, maker_fee)
        .posting(venue, -maker_fee);
        match self.ledger.post(draft) {
            Ok(tx) => Some((tx.id, settled)),
            Err(e) => {
                self.settlement_failures = self.settlement_failures.saturating_add(1);
                tracing::error!(
                    symbol = sym,
                    price_cents = trade.price_cents,
                    qty = trade.qty,
                    error = %e,
                    "settlement refused after the book had already traded"
                );
                None
            }
        }
    }

    /// Assign the next id to `rec`, append it to the log, publish it and
    /// return it.
    pub fn record(&mut self, mut rec: EventRecord) -> EventRecord {
        rec.id = self.next_event_id;
        self.next_event_id += 1;
        if self.events.len() >= self.event_cap {
            self.events.pop_front();
        }
        self.events.push_back(rec.clone());
        self.announce(StreamMessage::Event(rec.clone()));
        rec
    }

    /// Publish a fact: out over the SSE stream now, to whoever
    /// [`StreamMessage::audience`] admits, and into the outbox for
    /// the game backend to collect at its own pace.
    ///
    /// The outbox half is deliberately narrower than the stream. It takes a
    /// fact only while a command is being applied — `pinned_now` is what says
    /// so — because a fact published outside one is not journaled, and an
    /// entry a restart would not reproduce is worse in a log that promises
    /// replay than no entry at all. The facts collected here are not in the
    /// outbox yet either: [`Market::run_command`] puts them there once the
    /// command that caused them is on disk, so the log holds what was
    /// committed rather than what was attempted.
    ///
    /// Ticks do not come through here. They are market data, they are the
    /// highest-volume thing the server produces, and they can be had again
    /// from the bars; see [`crate::outbox`].
    pub(crate) fn announce(&mut self, message: StreamMessage) {
        if self.pinned_now.is_some() && self.outbox.is_enabled() {
            match serde_json::to_value(&message) {
                Ok(event) => self.pending_facts.push((message.kind().to_string(), event)),
                // A fact that will not serialise cannot be delivered, and
                // dropping it silently would make the outbox a liar. It is
                // still published: the stream is best-effort either way.
                Err(e) => tracing::error!(
                    kind = message.kind(),
                    error = %e,
                    "a fact could not be written to the outbox"
                ),
            }
        }
        self.stream.publish(message);
    }

    /// Move the facts of the command just journaled into the outbox, under
    /// its sequence.
    pub(crate) fn commit_facts(&mut self, command_seq: u64, at_ms: i64, wall_ms: i64) {
        let facts = std::mem::take(&mut self.pending_facts);
        self.outbox.commit(command_seq, at_ms, wall_ms, facts);
    }

    /// Throw away the facts of a command that changed nothing.
    pub(crate) fn discard_facts(&mut self) {
        self.pending_facts.clear();
    }

    /// The most recent `limit` events matching `keep`, newest first.
    pub fn events(&self, limit: usize, keep: impl Fn(&EventRecord) -> bool) -> Vec<EventRecord> {
        self.events
            .iter()
            .rev()
            .filter(|e| keep(e))
            .take(limit)
            .cloned()
            .collect()
    }

    /// The stream, to publish on.
    pub fn stream(&self) -> &Stream {
        &self.stream
    }

    /// The simulated instant the market is working at.
    ///
    /// The wall-derived clock, except while a journaled command is being
    /// applied: then it is the instant that command was accepted at, which
    /// is what lets a command replayed on the next start behave as it did
    /// when it was acknowledged. See [`crate::journal`].
    pub fn now(&self) -> Timestamp {
        self.pinned_now.unwrap_or_else(|| self.clock.now())
    }

    /// Warm a listing up under this market's own calendar, tick and lot.
    ///
    /// The same work [`App::prepare_listing`] does, on the market rather
    /// than beside it, because a listing is a journaled command and a
    /// command is applied by the market.
    ///
    /// # Errors
    /// The first [`fehu::ConfigError`] in the requested config.
    pub fn prepare_listing(
        &self,
        spec: SymbolSpec,
        history_days: usize,
        now: Timestamp,
    ) -> Result<SymbolState, fehu::ConfigError> {
        prepare_listing(&self.options, spec, history_days, now)
    }
}

// ---------------------------------------------------------------------------
// The trading half: everything that touches a book. Each of these is one
// job on the market actor; the symbol calls inside it are the only awaits.

impl Market {
    /// The id the next trader order in a book whose counter is at
    /// `book_next` takes.
    fn allocate_order_id(&mut self, book_next: fehu::OrderId) -> fehu::OrderId {
        let id = self.next_order_id.max(book_next);
        self.next_order_id = fehu::OrderId(id.0 + 1);
        id
    }

    /// Send an order: check it against the symbol, fund it from the
    /// account, send it to the book, book what follows — the fills, the
    /// reservation of what rests, the order log — and publish the fills.
    pub async fn place(
        &mut self,
        symbol: &Symbol,
        trader: TraderId,
        req: PlaceRequest,
    ) -> Result<Placed, PlaceError> {
        let sym = symbol.ticker;
        let now = self.now();
        let order = req.order;
        let prepared = symbol
            .ask_listed({
                let (post_only, day, expires) = (req.post_only, req.day, req.expires_at_ms);
                move |s| s.prepare(&order, post_only, day, expires, now)
            })
            .await?
            .ok_or(PlaceError::UnknownSymbol)??;
        // The same order sent twice — a retry after a timeout, say — is
        // placed once: the first response is replayed, and a re-used id that
        // asks for something else is refused rather than quietly obeyed.
        if let Some(id) = req.client_order_id.as_deref()
            && let Some(record) = self.order_by_client_id(trader, id)
        {
            return match record.accepted.clone() {
                Some(accepted) if record.matches(&order, sym) => Ok(Placed::Replayed(accepted)),
                _ => Err(PlaceError::DuplicateClientId {
                    client_order_id: id.to_string(),
                    order_id: record.order_id,
                }),
            };
        }
        let response = self
            .fund_and_send(symbol, trader, order, prepared, req)
            .await?;
        Ok(Placed::New(response))
    }

    /// The second half of placing: the money and share checks, the book,
    /// the booking. `prepared` is the symbol's word on the order.
    async fn fund_and_send(
        &mut self,
        symbol: &Symbol,
        trader: TraderId,
        order: Order,
        prepared: Prepared,
        req: PlaceRequest,
    ) -> Result<OrderResponse, PlaceError> {
        let sym = symbol.ticker;
        // A symbol quoted by synthetic liquidity has a ceiling nothing else
        // enforces: the ladder will sell what nobody holds, so a buy is
        // limited to the units no trader holds or is already bidding for.
        // Both numbers are still what they were when the symbol counted
        // them: nothing touches a book between here and the submit but this
        // job.
        //
        // Without that ladder the ceiling is not needed and would be wrong.
        // Every fill then comes from a trader who holds the units and has
        // reserved them, so a bid can no more conjure a unit than a wish
        // can; refusing it against the outstanding count would only stop
        // two players bidding for the same good at once.
        if order.side == Side::Buy {
            let (outstanding, synthetic) = symbol
                .ask(|s| (s.info.units_outstanding(), s.exchange.params().synthetic))
                .await?;
            let available = outstanding
                .saturating_sub(self.held_shares(sym))
                .saturating_sub(prepared.bid_shares);
            if synthetic && order.qty > available {
                self.orders_refused = self.orders_refused.saturating_add(1);
                return Err(PlaceError::Refused(Refused::SupplyExhausted {
                    needed: order.qty,
                    available,
                }));
            }
        }
        // A crossing order pays the taker fee out of the same cash, and it
        // is charged the moment it fills, so it is checked here rather than
        // discovered afterwards.
        let cost = prepared
            .cost_cents
            .saturating_add(self.fees.taker_cost(prepared.cost_cents));
        // Validate the order against the account that would fund it: it must
        // be active, a buy must have the cash available, and a sell the shares
        // — nothing may be sold that the trader does not hold.
        let account = self
            .account_of(trader)
            .ok_or(PlaceError::UnknownTrader(trader.0))?;
        if let Err(e) =
            self.traders[&trader].check(&self.ledger, account, sym, order.side, order.qty, cost)
        {
            self.orders_refused = self.orders_refused.saturating_add(1);
            return Err(PlaceError::Refused(e));
        }
        let book_next = symbol.ask(|s| s.exchange.book().next_order_id()).await?;
        let min_id = self.allocate_order_id(book_next);
        let display_qty = req.display_qty;
        let submitted = symbol
            .change(move |s| s.submit(order, min_id, display_qty))
            .await?;
        let Submitted {
            placement,
            next_order_id: book_next,
            clock_ms,
        } = match submitted {
            Ok(submitted) => submitted,
            Err(e) => {
                self.orders_refused = self.orders_refused.saturating_add(1);
                return Err(PlaceError::Invalid(e.to_string()));
            }
        };
        self.next_order_id = self.next_order_id.max(book_next);
        self.orders_placed = self.orders_placed.saturating_add(1);
        self.book(sym, &placement.trades);
        if placement.status == OrderStatus::Resting
            && let fehu::OrderKind::Limit { price_cents } = order.kind
            && let Some((t, account, ledger)) = self.settling(trader)
        {
            t.reserve(
                ledger,
                account,
                sym,
                order.side,
                placement.remaining,
                price_cents,
            );
        }
        let response = OrderResponse::new(sym, trader, order.side, order.qty, &placement);
        if req.logged {
            self.record_order(
                OrderRecord::new(
                    req.client_order_id,
                    trader,
                    sym,
                    &order,
                    &placement,
                    response.clone(),
                    clock_ms,
                )
                .expiring_at(prepared.expires_at_ms),
            );
        }
        Ok(response)
    }

    /// Replace a resting order with another at a new price or quantity: a
    /// cancel and a fresh order, in that order. The replacement goes to the
    /// back of the queue at its price, and if it cannot be placed — no
    /// cash, no shares, a halt — the old order is already gone.
    pub async fn amend(
        &mut self,
        symbol: &Symbol,
        trader: TraderId,
        req: Amendment,
    ) -> Result<Amended, PlaceError> {
        let sym = symbol.ticker;
        let now = self.now();
        let Amendment {
            order_id,
            price_cents,
            qty,
            post_only,
            client_order_id,
        } = req;
        let amended = symbol
            .change(move |s| {
                if s.delisted {
                    return None;
                }
                Some(s.amend(order_id, trader, price_cents, qty, post_only, now))
            })
            .await?
            .ok_or(PlaceError::UnknownSymbol)?;
        // The old order may be gone even when the checks on its replacement
        // failed: that is what an amendment means, and the reservation goes
        // with it either way.
        let (cancelled, order, prepared) = match amended {
            Ok(amended) => amended,
            Err(check) => return Err(PlaceError::Check(check)),
        };
        self.release(trader, sym, &cancelled, now.0);
        let order_response = self
            .fund_and_send(
                symbol,
                trader,
                order,
                prepared,
                PlaceRequest {
                    order,
                    client_order_id,
                    post_only,
                    day: false,
                    expires_at_ms: None,
                    display_qty: None,
                    logged: true,
                },
            )
            .await?;
        Ok(Amended {
            replaced_order_id: order_id,
            replaced_filled: cancelled.qty.saturating_sub(cancelled.outstanding()),
            order: order_response,
        })
    }

    /// Withdraw one of `trader`'s resting orders: out of the book, its
    /// reservation released, its record marked cancelled.
    pub async fn cancel(
        &mut self,
        symbol: &Symbol,
        trader: TraderId,
        order_id: u64,
    ) -> Result<Option<Resting>, fehu::CancelError> {
        let sym = symbol.ticker;
        let Ok(cancelled) = symbol
            .change(move |s| {
                if s.delisted {
                    return Ok(None);
                }
                s.cancel(order_id, trader).map(Some)
            })
            .await
        else {
            return Ok(None);
        };
        let Some(resting) = cancelled? else {
            return Ok(None);
        };
        self.release(trader, sym, &resting, self.now().0);
        Ok(Some(resting))
    }

    /// Withdraw every resting order `trader` has, in every symbol.
    pub async fn cancel_all(&mut self, trader: TraderId) -> Vec<OpenOrderDto> {
        let now_ms = self.now().0;
        let mut out = Vec::new();
        for symbol in self.symbols.clone() {
            let sym = symbol.ticker;
            let Ok(cancelled) = symbol.change(move |s| s.cancel_all(trader)).await else {
                continue;
            };
            for resting in cancelled {
                self.release(trader, sym, &resting, now_ms);
                out.push(OpenOrderDto::from_resting(sym, &resting));
            }
        }
        out
    }

    /// Every stop `trader` holds, across every symbol, oldest first within
    /// each.
    pub async fn stops_of(&self, trader: TraderId) -> Vec<StopOrder> {
        let mut stops = Vec::new();
        for symbol in &self.symbols {
            if let Ok(held) = symbol.ask(move |s| s.stops_of(trader)).await {
                stops.extend(held);
            }
        }
        stops
    }

    /// Hold a stop until the price reaches it.
    ///
    /// The account is checked here so an obviously unfundable trigger is
    /// refused while the trader is still looking at the response, but nothing
    /// is reserved: a stop that never fires costs its owner nothing, and the
    /// real check is the one made when it does fire.
    pub async fn place_stop(
        &mut self,
        symbol: &Symbol,
        req: StopRequest,
    ) -> Result<StopOrder, PlaceError> {
        let trader = TraderId(req.trader_id);
        let sym = symbol.ticker;
        let (checked, now_ms) = {
            let req = req.clone();
            symbol
                .ask_listed(move |s| (s.check_stop(&req), s.exchange.clock().0))
                .await?
                .ok_or(PlaceError::UnknownSymbol)?
        };
        checked.map_err(PlaceError::Invalid)?;
        if self.stops_of(trader).await.len() >= MAX_STOPS_PER_TRADER {
            return Err(PlaceError::Invalid(format!(
                "a trader may hold {MAX_STOPS_PER_TRADER} stops at once; cancel one first"
            )));
        }
        let stop = StopOrder {
            stop_id: self.next_stop_id,
            trader_id: req.trader_id,
            symbol: sym,
            side: req.side,
            qty: req.qty,
            stop_price_cents: req.stop_price_cents,
            limit_price_cents: req.limit_price_cents,
            tif: req.tif,
            client_order_id: req.client_order_id,
            created_at_ms: now_ms,
        };
        let account = self
            .account_of(trader)
            .ok_or(PlaceError::UnknownTrader(trader.0))?;
        let fees = self.fees;
        self.traders[&trader]
            .check(&self.ledger, account, sym, req.side, req.qty, {
                // The order it fires will take liquidity, so the advisory
                // check counts the taker fee too.
                let cost = stop.cost_cents();
                cost.saturating_add(fees.taker_cost(cost))
            })
            .map_err(PlaceError::Refused)?;
        self.next_stop_id += 1;
        let armed = stop.clone();
        symbol.change(move |s| s.arm(armed)).await?;
        Ok(stop)
    }

    /// Withdraw a held stop. `None` if the symbol holds no such stop of the
    /// trader's.
    pub async fn cancel_stop(
        &mut self,
        symbol: &Symbol,
        trader: TraderId,
        stop_id: u64,
    ) -> Option<StopOrder> {
        symbol
            .change(move |s| s.disarm(stop_id, trader))
            .await
            .ok()
            .flatten()
    }

    /// Stop trading in a symbol by hand. It stays stopped until somebody
    /// resumes it.
    pub async fn halt(&mut self, symbol: &Symbol) -> Option<SymbolStatus> {
        let (now, halts) = (self.now(), self.halts);
        let status = symbol
            .change(move |s| (!s.delisted).then(|| s.halt_by_hand(now, halts)))
            .await
            .ok()
            .flatten()?;
        self.announce(StreamMessage::Status(status));
        Some(status)
    }

    /// Start trading again, whatever stopped it. The requote's trades are
    /// booked and their fills published.
    pub async fn resume(&mut self, symbol: &Symbol) -> Option<SymbolStatus> {
        let (now, halts) = (self.now(), self.halts);
        let (status, trades) = symbol
            .change(move |s| (!s.delisted).then(|| s.resume_trading(now, halts)))
            .await
            .ok()
            .flatten()?;
        self.announce(StreamMessage::Status(status));
        self.book(symbol.ticker, &trades);
        Some(status)
    }

    /// Pay `cents_per_share` on every share of `symbol` a trader holds, and
    /// drop the price by the same amount.
    ///
    /// Both halves matter. Paying without the price move would be money from
    /// nothing — buy the day before, collect, sell the day after — so the
    /// reference and the fundamental both fall by the dividend, which is what
    /// going ex-dividend means. The shares themselves do not move: nothing is
    /// created or destroyed, so `shares_outstanding` is untouched.
    ///
    /// A frozen account is paid too: it still owns its shares.
    ///
    /// # Errors
    /// `Ok(None)` if the dividend is not between one cent and the price;
    /// [`PayoutError::Unfunded`] if the symbol's issuer wallet cannot pay it,
    /// in which case nothing at all was paid.
    pub async fn pay_dividend(
        &mut self,
        symbol: &Symbol,
        cents_per_share: i64,
        note: Option<String>,
        now_ms: i64,
    ) -> Result<Option<(Dividend, Vec<crate::events::SimEvent>)>, PayoutError> {
        let sym = symbol.ticker;
        let Some(price_cents) = symbol.ask_listed(|s| s.price_cents()).await? else {
            return Ok(None);
        };
        if cents_per_share <= 0 || cents_per_share >= price_cents {
            return Ok(None);
        }
        let owed: Vec<(AccountId, i64, u64)> = self
            .traders
            .values()
            .filter_map(|t| {
                let qty = u64::try_from(t.positions.get(sym).map_or(0, |p| p.qty)).ok()?;
                let amount = fehu::ledger::checked_notional_cents(cents_per_share, qty).ok()?;
                (qty > 0).then_some((t.account_id, amount, qty))
            })
            .collect();
        let mut paid = Dividend {
            symbol: sym,
            cents_per_share,
            price_cents,
            shares_paid: 0,
            accounts_paid: 0,
            total_cents: 0,
        };
        // One transaction: the issuer pays, every holder is credited. Either
        // it funds the whole dividend or none of it is paid — a payout that
        // reaches some holders and not others is not a dividend, and one
        // clipped at a balance cap is currency the issuer still owes.
        if !owed.is_empty() {
            let issuer = self.issuer_wallet(sym);
            let mut draft = Draft::new(Reason::Dividend)
                .at(u64::try_from(now_ms).unwrap_or(0))
                .memo(note.clone());
            for (account_id, amount, _) in &owed {
                let Some(wallet) = self.accounts.get(account_id).map(|a| a.wallet) else {
                    continue;
                };
                draft = draft.debit(issuer, *amount).credit(wallet, *amount);
            }
            let tx = match self.ledger.post(draft) {
                Ok(tx) => tx,
                Err(e) => return Err(PayoutError::Unfunded(e)),
            };
            for (account_id, amount, qty) in owed {
                let Self {
                    ledger, accounts, ..
                } = self;
                let Some(account) = accounts.get_mut(&account_id) else {
                    continue;
                };
                account.record(
                    ledger,
                    LedgerKind::Dividend,
                    tx.id,
                    amount,
                    now_ms,
                    Some(sym),
                    None,
                    note.clone(),
                );
                paid.accounts_paid += 1;
                paid.shares_paid = paid.shares_paid.saturating_add(qty);
                paid.total_cents = paid.total_cents.saturating_add(amount);
            }
        }
        // Going ex: the price drops by the dividend, and so does what the
        // price reverts to, or the market would simply pay it back.
        let ratio = cents_per_share as f64 / price_cents as f64;
        let effects = vec![
            crate::events::SimEvent::Jump { pct: -ratio },
            crate::events::SimEvent::FundamentalShift {
                delta: (1.0_f64 - ratio).ln(),
            },
        ];
        let at = self.now();
        let apply = effects.clone();
        symbol
            .change(move |s| {
                for event in &apply {
                    if let Ok(prepared) = event.prepare() {
                        // The dividend is already paid; a price event the
                        // simulator would refuse is a bug, not a request.
                        let _ = prepared.apply(s.exchange.simulator_mut(), at);
                    }
                }
            })
            .await?;
        Ok(Some((paid, effects)))
    }

    /// Push prepared simulator events into one symbol, taking effect at
    /// `at`. `Ok(None)` if the symbol is not listed.
    pub async fn apply_events(
        &mut self,
        symbol: &Symbol,
        events: Vec<crate::events::Prepared>,
        at: Timestamp,
    ) -> Result<Option<Result<(), String>>, Gone> {
        symbol
            .change(move |s| {
                if s.delisted {
                    return None;
                }
                let sim = s.exchange.simulator_mut();
                Some(
                    events
                        .into_iter()
                        .try_for_each(|p| p.apply(sim, at))
                        .map_err(|e| e.to_string()),
                )
            })
            .await
    }

    /// Push prepared simulator events into every symbol at the same
    /// simulated moment. Returns the tickers they went to.
    pub async fn apply_events_everywhere(
        &mut self,
        events: Vec<crate::events::Prepared>,
        at: Timestamp,
    ) -> Result<Vec<&'static str>, String> {
        let mut tickers = Vec::with_capacity(self.symbols.len());
        for symbol in self.symbols.clone() {
            match self.apply_events(&symbol, events.clone(), at).await {
                Ok(Some(Ok(()))) => tickers.push(symbol.ticker),
                Ok(Some(Err(e))) => return Err(e),
                Ok(None) | Err(Gone) => {}
            }
        }
        Ok(tickers)
    }

    /// List a new symbol: from this job on it is quoted, it ticks, and orders
    /// in it are accepted like any other. `state` was built and warmed up
    /// outside ([`App::prepare_listing`]); here it gets an actor and a place
    /// in the table.
    ///
    /// # Errors
    /// [`ListingError`] if the ticker is already listed, or the market
    /// already has `max_symbols` of them.
    /// Give a symbol's issuer wallet its float out of treasury, so that the
    /// dividends and the buyout it may owe are funded before it lists.
    ///
    /// Nothing is minted: an issuer's money is the world's money, set aside.
    /// A treasury too thin to cover the float funds what it can — a listing
    /// is not a payout, and an underfunded issuer refuses its own dividends
    /// loudly enough without also refusing to exist.
    fn fund_issuer(&mut self, symbol: &'static str, float_cents: i64) {
        if float_cents <= 0 {
            return;
        }
        let treasury = self.wallets.treasury;
        let issuer = self.issuer_wallet(symbol);
        let amount = float_cents.min(self.ledger.available(treasury));
        if amount <= 0 {
            return;
        }
        let _ = self.ledger.post(
            Draft::new(Reason::Transfer)
                .debit(treasury, amount)
                .credit(issuer, amount)
                .memo(Some(format!("{symbol} issuer float"))),
        );
    }

    pub fn list(&mut self, state: SymbolState) -> Result<Quote, ListingError> {
        let ticker = state.info.symbol;
        if self.symbol(ticker).is_some() {
            return Err(ListingError::AlreadyListed(ticker));
        }
        if self.symbols.len() >= self.max_symbols {
            return Err(ListingError::Full {
                max: self.max_symbols,
            });
        }
        let quote = state.quote();
        // A good's issuer pays no dividend and buys nobody out; its wallet
        // is where the money paid for its units *arrives*. Floating it out
        // of treasury would set currency aside against a payout that cannot
        // happen.
        let float = if state.info.is_good() {
            0
        } else {
            self.issuer_float_cents
        };
        self.symbols.push(Symbol::spawn(state));
        self.fund_issuer(ticker, float);
        self.publish_listings();
        self.announce(StreamMessage::Listed {
            quote: quote.clone(),
        });
        Ok(quote)
    }

    /// Create an NPC: a funded trader the world runs itself.
    ///
    /// It gets a user nobody can sign in as, an account whose wallet is an
    /// [`Npc`](WalletKind::Npc) one, `cash_cents` out of treasury, and
    /// `inventory` units of the symbol it makes a market in. None of that
    /// creates currency — the cash is a transfer, exactly as a player's
    /// opening balance is.
    ///
    /// The inventory is where the two asset kinds part. A stock's shares
    /// already exist, so the NPC is *assigned* some of the float that nobody
    /// held; a good's units do not, so they are *issued*, and the count of
    /// what has been issued goes up by exactly what the NPC was given. Both
    /// leave the audit's sentence true.
    ///
    /// # Errors
    /// [`GoodsError`] naming what was wrong. Nothing is created by a
    /// refusal.
    pub async fn create_npc(
        &mut self,
        symbol: &Symbol,
        name: Option<String>,
        policy: Policy,
        cash_cents: i64,
        inventory: u64,
        now_ms: i64,
    ) -> Result<Npc, GoodsError> {
        let sym = symbol.ticker;
        if self.npcs.len() >= MAX_NPCS {
            return Err(GoodsError::Quantity(format!(
                "this world runs {MAX_NPCS} NPCs already"
            )));
        }
        policy
            .validate()
            .map_err(|e| GoodsError::Quantity(format!("policy: {e}")))?;
        if cash_cents < 0 {
            return Err(GoodsError::Money(MoneyError::NotPositive {
                amount_cents: cash_cents,
            }));
        }
        // What the symbol can give it, before anything is created. A stock
        // can only hand over shares nobody holds; a good can issue whatever
        // its count will hold.
        let (is_good, outstanding, issued) = symbol
            .ask(|s| match &s.info.asset {
                AssetKind::Good { issued, .. } => (true, s.info.units_outstanding(), *issued),
                AssetKind::Stock {
                    shares_outstanding, ..
                } => (false, *shares_outstanding, 0),
            })
            .await?;
        if is_good {
            if issued.checked_add(inventory).is_none() {
                return Err(GoodsError::Quantity(format!(
                    "{sym} cannot issue that many more units"
                )));
            }
        } else {
            let unheld = outstanding.saturating_sub(self.held_shares(sym));
            if inventory > unheld {
                return Err(GoodsError::Catalog(CatalogError::Exhausted {
                    wanted: inventory,
                    available: unheld,
                }));
            }
        }
        let name = clean(name, 64).unwrap_or_else(|| format!("{sym} merchant"));
        let user_id = self.create_house_user(Some(name.clone()), now_ms);
        let account_id = self
            .open_account_as(
                user_id,
                Some(name.clone()),
                cash_cents,
                now_ms,
                WalletKind::Npc,
            )
            .inspect_err(|_| {
                // The faucet was dry, so the NPC does not exist. Its user
                // would otherwise be left behind with nothing attached.
                self.users.remove(&user_id);
            })?;
        let trader = self.create_trader(user_id, account_id, Some(name.clone()), now_ms);
        if inventory > 0 {
            if is_good {
                let _ = symbol
                    .change(move |s| s.info.issue(inventory))
                    .await
                    .map_err(GoodsError::from)?;
            }
            let reference = symbol.ask(|s| s.price_cents()).await?;
            if let Some(t) = self.traders.get_mut(&trader) {
                t.endow(sym, inventory, reference);
            }
        }
        let npc = Npc {
            trader,
            user_id,
            account_id,
            symbol: sym,
            name,
            policy,
            active: true,
            quoted_ref_cents: 0,
            quoted_orders: 0,
            quoted_size: 0,
        };
        self.npcs.insert(trader, npc.clone());
        Ok(npc)
    }

    /// Switch an NPC's quoting on or off. Its money and its inventory stay
    /// where they are either way; what stops is putting them on the book.
    pub async fn set_npc_active(&mut self, trader: TraderId, active: bool) -> Option<Npc> {
        let npc = self.npcs.get_mut(&trader)?;
        npc.active = active;
        if !active {
            npc.quoted_ref_cents = 0;
            npc.quoted_orders = 0;
            npc.quoted_size = 0;
        }
        let npc = npc.clone();
        if !active {
            self.cancel_all(trader).await;
        }
        Some(npc)
    }

    /// One NPC, as the API shows it: what it is, and what it has left.
    pub fn npc_view(&self, trader: TraderId) -> Option<NpcDto> {
        let npc = self.npcs.get(&trader)?;
        Some(NpcDto {
            trader_id: npc.trader.0,
            user_id: npc.user_id.0,
            account_id: npc.account_id.0,
            symbol: npc.symbol,
            name: npc.name.clone(),
            policy: npc.policy,
            active: npc.active,
            quoted_size: npc.quoted_size,
            cash_cents: self
                .accounts
                .get(&npc.account_id)
                .map_or(0, |a| a.available_cents(&self.ledger)),
            inventory: self
                .traders
                .get(&npc.trader)
                .map_or(0, |t| t.held_shares(npc.symbol)),
            reserved: self.traders.get(&npc.trader).map_or(0, |t| {
                t.reserved_shares.get(npc.symbol).copied().unwrap_or(0)
            }),
        })
    }

    /// Every NPC, as the API shows it.
    pub fn npc_views(&self) -> Vec<NpcDto> {
        self.npcs
            .keys()
            .filter_map(|trader| self.npc_view(*trader))
            .collect()
    }

    /// Redraw every NPC's quotes that the market has moved away from.
    ///
    /// Run at the end of an engine step, which is a journaled command, and
    /// reading nothing but the market and the symbols — no clock, no
    /// randomness — so a replayed step draws the same book.
    ///
    /// An NPC quotes what it can fund and what it holds and no more. The
    /// levels it cannot afford are simply refused by the same checks that
    /// refuse a player's order, which is how an empty till shows up as an
    /// empty side of the book rather than as a rule written down somewhere.
    async fn requote_npcs(&mut self) {
        let ids: Vec<TraderId> = self
            .npcs
            .values()
            .filter(|npc| npc.active)
            .map(|npc| npc.trader)
            .collect();
        for trader in ids {
            let Some(npc) = self.npcs.get(&trader).cloned() else {
                continue;
            };
            let Some(symbol) = self.symbol(npc.symbol).cloned() else {
                continue;
            };
            let Ok(Some((reference, tradable))) = symbol
                .ask_listed(|s| (s.price_cents(), s.halt.is_none()))
                .await
            else {
                continue;
            };
            if !tradable || reference <= 0 {
                continue;
            }
            let Ok(resting) = symbol.ask(move |s| resting_orders(s, trader)).await else {
                continue;
            };
            // What the world wants of this good right now. A merchant quotes
            // its policy size scaled by that, so a hyped good is quoted for
            // more of and a scandal thins the book out — the same event that
            // moves the price, arriving through somebody who paid for the
            // inventory.
            let demand_bps = self
                .world
                .multiplier_bps(Effect::Demand, npc.symbol, self.now().0);
            let size = scaled_qty(npc.policy.size, demand_bps);
            if npc.policy.still_good(npc.quoted_ref_cents, reference)
                && resting == npc.quoted_orders
                && size == npc.quoted_size
            {
                continue;
            }
            self.cancel_all(trader).await;
            for k in 0..npc.policy.levels {
                for side in [Side::Buy, Side::Sell] {
                    let price_cents = npc.policy.price_cents(side, reference, k);
                    let order = Order {
                        owner: Owner::Trader(trader),
                        side,
                        kind: fehu::OrderKind::Limit { price_cents },
                        tif: fehu::TimeInForce::Gtc,
                        qty: size,
                    };
                    // A refusal is the answer, not an error: an NPC that
                    // cannot fund this level has nothing to say at it.
                    let _ = self
                        .place(
                            &symbol,
                            trader,
                            PlaceRequest {
                                order,
                                client_order_id: None,
                                post_only: true,
                                day: false,
                                expires_at_ms: None,
                                display_qty: None,
                                logged: false,
                            },
                        )
                        .await;
                }
            }
            let resting = symbol
                .ask(move |s| resting_orders(s, trader))
                .await
                .unwrap_or(0);
            if let Some(npc) = self.npcs.get_mut(&trader) {
                npc.quoted_ref_cents = reference;
                npc.quoted_orders = resting;
                npc.quoted_size = size;
            }
        }
    }

    /// Write or replace a catalogue line.
    ///
    /// Operator authority. The symbol has to be a listed good: a line
    /// against a company would be an offer to print its shares.
    ///
    /// # Errors
    /// [`GoodsError`] naming what was wrong. Nothing is changed by a
    /// refusal.
    pub async fn set_catalog_item(
        &mut self,
        symbol: &Symbol,
        price_cents: i64,
        available: Option<u64>,
        note: Option<String>,
    ) -> Result<CatalogItem, GoodsError> {
        let sym = symbol.ticker;
        if !symbol.ask(|s| s.info.is_good()).await? {
            return Err(GoodsError::NotAGood(sym.to_string()));
        }
        self.catalog
            .set(sym, price_cents, available, note)
            .cloned()
            .map_err(GoodsError::from)
    }

    /// Take a line out of the catalogue. Units already issued off it stay in
    /// the world; what stops is the making of more.
    pub fn remove_catalog_item(&mut self, symbol: &str) -> Option<CatalogItem> {
        self.catalog.remove(symbol)
    }

    /// Buy `qty` units of a good at the catalogue price.
    ///
    /// This is the only way a unit of a good comes into existence today, and
    /// it does two separable things at once: currency moves from the buyer's
    /// wallet to the good's issuer wallet — a balanced transaction, so the
    /// supply is untouched — and units that did not exist are issued to the
    /// buyer.
    ///
    /// The order is the one the milestone-1 settlements had to learn:
    /// everything is checked, then the money is posted, and only then are
    /// the units issued. The post is the fallible half, so it goes first;
    /// by the time it has succeeded, issuing cannot fail, because the
    /// arithmetic that could have overflowed was checked before either.
    ///
    /// # Errors
    /// [`GoodsError`] naming what was wrong. A refusal moves nothing and
    /// issues nothing.
    pub async fn purchase(
        &mut self,
        symbol: &Symbol,
        trader: TraderId,
        qty: u64,
        now_ms: i64,
    ) -> Result<PurchaseReceipt, GoodsError> {
        let sym = symbol.ticker;
        if qty == 0 {
            return Err(GoodsError::Quantity(
                "a purchase is for at least one unit".into(),
            ));
        }
        // What the symbol says about itself, before anything moves: that it
        // is a good, and that it can hold the units this would issue.
        let (is_good, issued) = symbol
            .ask(|s| match &s.info.asset {
                AssetKind::Good { issued, .. } => (true, *issued),
                AssetKind::Stock { .. } => (false, 0),
            })
            .await?;
        if !is_good {
            return Err(GoodsError::NotAGood(sym.to_string()));
        }
        if issued.checked_add(qty).is_none() {
            return Err(GoodsError::Quantity(format!(
                "{sym} cannot issue that many more units"
            )));
        }
        let unit_price_cents = self.catalog.quote(sym, qty)?;
        let total_cents = fehu::ledger::checked_notional_cents(unit_price_cents, qty)
            .map_err(|e| GoodsError::Money(MoneyError::Ledger(e)))?;
        let account_id = self
            .traders
            .get(&trader)
            .ok_or(GoodsError::UnknownTrader(trader.0))?
            .account_id;
        let wallet = self.wallet_of(account_id)?;
        let issuer = self.issuer_wallet(sym);
        let tx = self.ledger.post(
            Draft::new(Reason::Purchase)
                .debit(wallet, total_cents)
                .credit(issuer, total_cents)
                .memo(Some(format!("{qty} × {sym} from the catalogue"))),
        )?;
        // Past the point of refusal. Everything from here was checked above.
        let units_outstanding = symbol
            .change(move |s| {
                s.info
                    .issue(qty)
                    .unwrap_or_else(|_| s.info.units_outstanding())
            })
            .await?;
        self.catalog.issue(sym, qty);
        let Self {
            ledger,
            accounts,
            traders,
            ..
        } = self;
        let position_qty = traders
            .get_mut(&trader)
            .zip(accounts.get_mut(&account_id))
            .map_or(0, |(t, a)| {
                t.acquire(
                    ledger,
                    a,
                    sym,
                    qty,
                    unit_price_cents,
                    tx.id,
                    now_ms,
                    Some(format!("{qty} × {sym} from the catalogue")),
                )
            });
        Ok(PurchaseReceipt {
            trader_id: trader.0,
            symbol: sym,
            qty,
            unit_price_cents,
            total_cents,
            tx_id: tx.id,
            position_qty,
            units_outstanding,
            available: self.catalog.get(sym).and_then(|item| item.available),
        })
    }

    /// Destroy `qty` units of a good the trader holds.
    ///
    /// No currency moves: a thing that has been used up is not a thing that
    /// has been sold. What changes is the world's count of what has been
    /// consumed, and the holder's position, which realises what the units
    /// cost as a loss.
    ///
    /// The units must be held *and* unreserved: a unit promised to a resting
    /// sell is spoken for, and eating it would leave an order the trader
    /// cannot fill.
    ///
    /// # Errors
    /// [`GoodsError`] naming what was wrong. A refusal destroys nothing.
    pub async fn consume(
        &mut self,
        symbol: &Symbol,
        trader: TraderId,
        qty: u64,
    ) -> Result<ConsumeReceipt, GoodsError> {
        let sym = symbol.ticker;
        if qty == 0 {
            return Err(GoodsError::Quantity(
                "consuming is of at least one unit".into(),
            ));
        }
        if !symbol.ask(|s| s.info.is_good()).await? {
            return Err(GoodsError::NotAGood(sym.to_string()));
        }
        let free = self
            .traders
            .get(&trader)
            .ok_or(GoodsError::UnknownTrader(trader.0))?
            .free_shares(sym);
        if free < qty {
            return Err(GoodsError::InsufficientUnits {
                needed: qty,
                available: free,
            });
        }
        // The holder has the units, so the world has them: the audit says
        // those are the same sentence. Nothing below can fail.
        let units_outstanding = symbol
            .change(move |s| {
                s.info
                    .consume(qty)
                    .unwrap_or_else(|_| s.info.units_outstanding())
            })
            .await?;
        let position_qty = self
            .traders
            .get_mut(&trader)
            .map_or(0, |t| t.destroy(sym, qty));
        Ok(ConsumeReceipt {
            trader_id: trader.0,
            symbol: sym,
            qty,
            position_qty,
            units_outstanding,
        })
    }

    // -----------------------------------------------------------------------
    // Recipes, jobs, budgets and rewards. See [`crate::jobs`] and
    // [`crate::rewards`].

    /// The handle for a listed good, refusing anything else.
    async fn good_handle(&self, ticker: &str) -> Result<Arc<Symbol>, JobError> {
        let handle = self
            .symbol(ticker)
            .cloned()
            .ok_or_else(|| JobError::Unknown(ticker.to_string()))?;
        if !handle.ask(|s| s.info.is_good()).await? {
            return Err(JobError::NotAGood(ticker.to_string()));
        }
        Ok(handle)
    }

    /// Write or replace a recipe.
    ///
    /// Every line is checked against the listings first: a recipe that names
    /// a company, or a ticker nobody has listed, is refused rather than
    /// written down to fail later at every job.
    ///
    /// # Errors
    /// [`JobError`] naming what was wrong. A refusal leaves the book alone.
    #[allow(clippy::too_many_arguments)]
    pub async fn set_recipe(
        &mut self,
        id: String,
        inputs: Vec<Line>,
        outputs: Vec<Line>,
        cost_cents: i64,
        duration_secs: u64,
        refund_bps: u32,
        note: Option<String>,
    ) -> Result<Recipe, JobError> {
        for line in inputs.iter().chain(outputs.iter()) {
            self.good_handle(line.symbol).await?;
        }
        self.recipes
            .set(
                id,
                inputs,
                outputs,
                cost_cents,
                duration_secs,
                refund_bps,
                note,
            )
            .cloned()
    }

    /// Take a recipe out of the book. Jobs already running finish: each one
    /// carries what it will deliver.
    pub fn remove_recipe(&mut self, id: &str) -> Option<Recipe> {
        self.recipes.remove(id)
    }

    /// Start a job: take the inputs, take the cost, and promise the outputs
    /// at an instant.
    ///
    /// The order is the settlement order the rest of the server uses.
    /// Everything is checked — the recipe, the listings, the units held free
    /// of reservations, the money — then the cost is posted, and only then
    /// are the inputs consumed. The post is the fallible half, so it goes
    /// first; by the time it has succeeded nothing left can fail.
    ///
    /// What the job will deliver is decided here and written into it: the
    /// recipe as it stands, scaled by the world's appetite for what it makes
    /// ([`crate::world`]). A recipe rewritten afterwards, or an event that
    /// lands halfway through, changes the next job and not this one.
    ///
    /// # Errors
    /// [`JobError`] naming what was wrong. A refusal takes nothing and
    /// promises nothing.
    pub async fn start_job(
        &mut self,
        recipe_id: &str,
        trader: TraderId,
        now_ms: i64,
    ) -> Result<Job, JobError> {
        let recipe = self
            .recipes
            .get(recipe_id)
            .cloned()
            .ok_or_else(|| JobError::UnknownRecipe(recipe_id.to_owned()))?;
        if self.jobs.running() >= crate::jobs::MAX_RUNNING_JOBS {
            return Err(JobError::TooManyJobs(crate::jobs::MAX_RUNNING_JOBS));
        }
        let account_id = self
            .traders
            .get(&trader)
            .ok_or(JobError::UnknownTrader(trader.0))?
            .account_id;
        let account =
            self.accounts
                .get(&account_id)
                .ok_or(JobError::Money(MoneyError::NoWallet {
                    account: account_id,
                }))?;
        account.authorise(&self.ledger, recipe.cost_cents.max(1))?;
        let wallet = self.wallet_of(account_id)?;
        // Every listing the recipe names, resolved before anything moves.
        let mut inputs = Vec::with_capacity(recipe.inputs.len());
        for line in &recipe.inputs {
            let handle = self.good_handle(line.symbol).await?;
            let free = self
                .traders
                .get(&trader)
                .map_or(0, |t| t.free_shares(line.symbol));
            if free < line.qty {
                return Err(JobError::InsufficientUnits {
                    symbol: line.symbol.to_string(),
                    needed: line.qty,
                    available: free,
                });
            }
            inputs.push((handle, *line));
        }
        // The world's appetite for what this recipe makes, read once, from
        // the instant the command was accepted at.
        let at = self.now().0;
        let yield_bps = recipe.outputs.first().map_or(BPS, |first| {
            self.world
                .multiplier_bps(Effect::Production, first.symbol, at)
        });
        let mut outputs = Vec::with_capacity(recipe.outputs.len());
        for line in &recipe.outputs {
            let handle = self.good_handle(line.symbol).await?;
            let qty = scaled_qty(line.qty, yield_bps);
            let issued = handle.ask(|s| s.info.units_outstanding()).await?;
            if issued.checked_add(qty).is_none() {
                return Err(JobError::Quantity(format!(
                    "{} cannot issue that many more units",
                    line.symbol
                )));
            }
            outputs.push(Line {
                symbol: line.symbol,
                qty,
            });
        }
        // Past the point of refusal.
        let tx_id = if recipe.cost_cents > 0 {
            let tx = self.ledger.post(
                Draft::new(Reason::JobCost)
                    .debit(wallet, recipe.cost_cents)
                    .credit(self.wallets.venue, recipe.cost_cents)
                    .memo(Some(format!("job: {}", recipe.id))),
            )?;
            self.write_entry(
                account_id,
                LedgerKind::JobCost,
                tx.id,
                -recipe.cost_cents,
                Some(format!("job: {}", recipe.id)),
                now_ms,
            );
            tx.id
        } else {
            0
        };
        let mut inputs_cost_cents = 0i64;
        for (handle, line) in inputs {
            let qty = line.qty;
            handle
                .change(move |s| {
                    s.info
                        .consume(qty)
                        .unwrap_or_else(|_| s.info.units_outstanding())
                })
                .await?;
            if let Some(t) = self.traders.get_mut(&trader) {
                // Withdrawn rather than destroyed: what the ore cost follows
                // it into the crucible and comes back out as part of what the
                // ingot cost.
                inputs_cost_cents = inputs_cost_cents.saturating_add(t.withdraw(line.symbol, qty));
            }
        }
        let id = self.jobs.take_id();
        let job = Job {
            id,
            recipe: recipe.id.clone(),
            recipe_version: recipe.version,
            trader_id: trader.0,
            account_id: account_id.0,
            inputs: recipe.inputs.clone(),
            outputs,
            yield_bps,
            cost_cents: recipe.cost_cents,
            tx_id,
            inputs_cost_cents,
            started_at_ms: at,
            due_at_ms: at.saturating_add(
                i64::try_from(recipe.duration_secs)
                    .unwrap_or(i64::MAX / 1_000)
                    .saturating_mul(1_000),
            ),
            finished_at_ms: None,
            status: JobStatus::Running,
            refunded_cents: 0,
        };
        self.jobs.insert(job.clone());
        Ok(job)
    }

    /// Stop a job before it is due.
    ///
    /// The inputs are gone: they went into the crucible when it started, and
    /// nothing here puts them back. What comes back is whatever the recipe
    /// said it would — [`Recipe::refund_bps`] of the cost, nothing by
    /// default — and never more than the venue is actually holding, which is
    /// the same rule a maker rebate learned: the venue does not pay currency
    /// it has not collected.
    ///
    /// # Errors
    /// [`JobError::UnknownJob`] for a job nobody started,
    /// [`JobError::NotRunning`] for one that has already finished.
    pub fn cancel_job(&mut self, job_id: u64, now_ms: i64) -> Result<Job, JobError> {
        let job = self
            .jobs
            .get(job_id)
            .cloned()
            .ok_or(JobError::UnknownJob(job_id))?;
        if job.status != JobStatus::Running {
            return Err(JobError::NotRunning(job_id));
        }
        let account_id = AccountId(job.account_id);
        let refund_bps = self
            .recipes
            .get(&job.recipe)
            .map_or(0, |r| i64::from(r.refund_bps));
        let asked = job.cost_cents.saturating_mul(refund_bps) / BPS;
        let refund = asked.min(self.ledger.available(self.wallets.venue)).max(0);
        let mut refunded = 0;
        if refund > 0
            && let Ok(wallet) = self.wallet_of(account_id)
            && let Ok(tx) = self.ledger.post(
                Draft::new(Reason::JobRefund)
                    .debit(self.wallets.venue, refund)
                    .credit(wallet, refund)
                    .memo(Some(format!("job {job_id} cancelled"))),
            )
        {
            let tx_id = tx.id;
            self.write_entry(
                account_id,
                LedgerKind::JobRefund,
                tx_id,
                refund,
                Some(format!("job {job_id} cancelled")),
                now_ms,
            );
            refunded = refund;
        }
        let at = self.now().0;
        let job = self
            .jobs
            .get_mut(job_id)
            .ok_or(JobError::UnknownJob(job_id))?;
        job.status = JobStatus::Cancelled;
        job.finished_at_ms = Some(at);
        job.refunded_cents = refunded;
        Ok(job.clone())
    }

    /// Deliver every job due at `at`, oldest first.
    ///
    /// Called from the engine step, which is a journaled command, so a
    /// replayed world delivers the same jobs at the same instant. A job whose
    /// good has been delisted while it ran delivers that line as nothing:
    /// there is no symbol left to issue a unit against, and the job is done
    /// either way rather than left running forever.
    pub async fn complete_jobs(&mut self, at: Timestamp) -> Vec<JobDelivery> {
        let due = self.jobs.due(at.0);
        let mut done = Vec::with_capacity(due.len());
        for id in due {
            let Some(job) = self.jobs.get(id).cloned() else {
                continue;
            };
            let total: u64 = job
                .outputs
                .iter()
                .map(|l| l.qty)
                .fold(0u64, u64::saturating_add);
            // What a unit is reckoned to have cost: what went into the job —
            // the ore and the furnace both — spread over what came out of it.
            let spent = job.cost_cents.saturating_add(job.inputs_cost_cents);
            let unit_cost = if total == 0 {
                0
            } else {
                spent / i64::try_from(total).unwrap_or(i64::MAX)
            };
            let trader = TraderId(job.trader_id);
            let mut delivered = Vec::with_capacity(job.outputs.len());
            for line in &job.outputs {
                let Some(handle) = self.symbol(line.symbol).cloned() else {
                    continue;
                };
                let qty = line.qty;
                let is_good = handle.ask(|s| s.info.is_good()).await.unwrap_or(false);
                if !is_good {
                    continue;
                }
                if handle
                    .change(move |s| s.info.issue(qty).is_ok())
                    .await
                    .unwrap_or(false)
                {
                    if let Some(t) = self.traders.get_mut(&trader) {
                        t.endow(line.symbol, qty, unit_cost);
                    }
                    delivered.push(*line);
                }
            }
            if let Some(job) = self.jobs.get_mut(id) {
                job.status = JobStatus::Done;
                job.finished_at_ms = Some(at.0);
            }
            let delivery = JobDelivery {
                job_id: id,
                trader_id: job.trader_id,
                recipe: job.recipe.clone(),
                delivered,
                at_ms: at.0,
            };
            self.announce(StreamMessage::JobDone(delivery.clone()));
            done.push(delivery);
        }
        done
    }

    /// Open a budget wallet and fund it out of treasury.
    ///
    /// A transfer, not a mint: a budget holds currency that already exists,
    /// which is what makes running out of it mean something.
    ///
    /// # Errors
    /// [`RewardError`] naming what was wrong. A refusal opens no wallet.
    pub fn create_budget(
        &mut self,
        name: String,
        cash_cents: i64,
        now_ms: i64,
    ) -> Result<Budget, RewardError> {
        if cash_cents < 0 {
            return Err(RewardError::Invalid(
                "a budget is opened with nothing or more".into(),
            ));
        }
        if cash_cents > 0 {
            check_transfer(cash_cents)?;
            let treasury = self.ledger.available(self.wallets.treasury);
            if cash_cents > treasury {
                return Err(RewardError::Money(MoneyError::Ledger(
                    LedgerError::Insufficient {
                        wallet: self.wallets.treasury,
                        needed_cents: cash_cents,
                        available_cents: treasury,
                    },
                )));
            }
        }
        let wallet = self.ledger.open(WalletKind::Budget);
        if cash_cents > 0 {
            self.ledger.post(
                Draft::new(Reason::Transfer)
                    .debit(self.wallets.treasury, cash_cents)
                    .credit(wallet, cash_cents)
                    .memo(Some(format!("budget: {name}"))),
            )?;
        }
        self.rewards
            .add_budget(Budget {
                wallet,
                name,
                created_at_ms: now_ms,
                paid_cents: 0,
                paid_count: 0,
            })
            .cloned()
    }

    /// Pay more into a budget, out of treasury.
    ///
    /// # Errors
    /// [`RewardError`] naming what was wrong. A refusal moves nothing.
    pub fn fund_budget(
        &mut self,
        wallet: WalletId,
        cash_cents: i64,
    ) -> Result<Budget, RewardError> {
        check_transfer(cash_cents)?;
        let budget = self
            .rewards
            .budget(wallet)
            .ok_or(RewardError::UnknownBudget(wallet.0))?
            .clone();
        self.ledger.post(
            Draft::new(Reason::Transfer)
                .debit(self.wallets.treasury, cash_cents)
                .credit(wallet, cash_cents)
                .memo(Some(format!("budget: {}", budget.name))),
        )?;
        Ok(budget)
    }

    /// Bring takings home: move what an issuer or the venue has collected
    /// back to treasury, all of it or `amount_cents` of it.
    ///
    /// A purchase credits the good's issuer and a fee credits the venue,
    /// and without this nothing ever moved either back, so every budget was
    /// funded by minting while the takings piled up where nothing could
    /// spend them. Posted as a transfer with a memo naming the wallet, so
    /// the flow meter counts it with the transfers and the treasury's own
    /// history says where each sweep came from. An issuer swept bare will
    /// refuse its next dividend rather than clip it, which is the
    /// operator's call to make.
    ///
    /// # Errors
    /// [`SweepError`]: the wallet is unknown or not one that collects
    /// takings, or there is less in it than was asked for. Nothing moves on
    /// a refusal.
    pub fn sweep(
        &mut self,
        wallet: WalletId,
        amount_cents: Option<i64>,
    ) -> Result<i64, SweepError> {
        let held = self
            .ledger
            .wallet(wallet)
            .ok_or(SweepError::UnknownWallet(wallet))?;
        if !matches!(held.kind, WalletKind::Issuer | WalletKind::Venue) {
            return Err(SweepError::NotTakings {
                wallet,
                kind: held.kind,
            });
        }
        let amount = amount_cents.unwrap_or_else(|| held.available_cents());
        check_transfer(amount).map_err(SweepError::Money)?;
        let name = self
            .issuers
            .iter()
            .find(|(_, id)| **id == wallet)
            .map_or_else(|| "venue".to_string(), |(sym, _)| format!("{sym} issuer"));
        self.ledger
            .post(
                Draft::new(Reason::Transfer)
                    .debit(wallet, amount)
                    .credit(self.wallets.treasury, amount)
                    .memo(Some(format!("sweep: {name}"))),
            )
            .map_err(|e| SweepError::Money(MoneyError::Ledger(e)))?;
        Ok(amount)
    }

    /// Write or replace a reward rule.
    ///
    /// # Errors
    /// [`RewardError`] naming what was wrong.
    pub fn set_reward_rule(
        &mut self,
        id: String,
        budget: WalletId,
        amount_cents: i64,
        note: Option<String>,
    ) -> Result<RewardRule, RewardError> {
        self.rewards
            .set_rule(id, budget, amount_cents, note)
            .cloned()
    }

    /// Take a reward rule out of the book. What it has paid stays paid.
    pub fn remove_reward_rule(&mut self, id: &str) -> Option<RewardRule> {
        self.rewards.remove_rule(id)
    }

    /// Pay a reward for something the game says happened.
    ///
    /// `source` is the game's own id for the event — a quest completion, a
    /// kill — and it is what stops the same thing being paid for twice by a
    /// backend that retries with a fresh key. A source id already paid gets
    /// the receipt it produced the first time, marked
    /// [`RewardReceipt::duplicate`], and nothing moves.
    ///
    /// # Errors
    /// [`RewardError`] naming what was wrong. A refusal pays nothing and
    /// remembers nothing, so it can be retried once the budget is topped up.
    pub fn pay_reward(
        &mut self,
        rule_id: &str,
        trader: TraderId,
        source: &str,
        now_ms: i64,
    ) -> Result<RewardReceipt, RewardError> {
        if let Some(paid) = self.rewards.paid(source) {
            return Ok(RewardReceipt {
                duplicate: true,
                ..paid.clone()
            });
        }
        let rule = self
            .rewards
            .rule(rule_id)
            .ok_or_else(|| RewardError::UnknownRule(rule_id.to_owned()))?
            .clone();
        let account_id = self
            .traders
            .get(&trader)
            .ok_or(RewardError::UnknownTrader(trader.0))?
            .account_id;
        let wallet = self.wallet_of(account_id)?;
        let available = self.ledger.available(rule.budget);
        if rule.amount_cents > available {
            return Err(RewardError::Exhausted {
                needed_cents: rule.amount_cents,
                available_cents: available,
            });
        }
        let memo = format!("reward {}: {source}", rule.id);
        let tx = self.ledger.post(
            Draft::new(Reason::Reward)
                .debit(rule.budget, rule.amount_cents)
                .credit(wallet, rule.amount_cents)
                .memo(Some(memo.clone())),
        )?;
        let entry = self.write_entry(
            account_id,
            LedgerKind::Reward,
            tx.id,
            rule.amount_cents,
            Some(memo),
            now_ms,
        );
        let receipt = RewardReceipt {
            rule: rule.id.clone(),
            source: source.to_owned(),
            trader_id: trader.0,
            account_id: account_id.0,
            budget: rule.budget,
            amount_cents: rule.amount_cents,
            tx_id: tx.id,
            balance_cents: entry.balance_cents,
            at_ms: now_ms,
            duplicate: false,
        };
        self.rewards.record(&receipt);
        Ok(receipt)
    }

    /// Move currency from one account to another.
    ///
    /// One balanced transaction, so the supply is untouched: this is players
    /// paying each other, not the world paying anybody. The sender must be
    /// able to spend — a frozen account cannot — and the recipient must be
    /// able to receive, which a frozen one still can.
    ///
    /// # Errors
    /// [`MoneyError`] naming what was wrong. A refusal moves nothing.
    pub fn transfer(
        &mut self,
        from: AccountId,
        to: AccountId,
        amount_cents: i64,
        memo: Option<String>,
        now_ms: i64,
    ) -> Result<(LedgerEntry, LedgerEntry), MoneyError> {
        check_transfer(amount_cents)?;
        if from == to {
            return Err(MoneyError::SameAccount { account: from });
        }
        let from_wallet = self.wallet_of(from)?;
        let to_wallet = self.wallet_of(to)?;
        self.accounts
            .get(&from)
            .ok_or(MoneyError::NoWallet { account: from })?
            .authorise(&self.ledger, amount_cents)?;
        let tx = self.ledger.post(
            Draft::new(Reason::Transfer)
                .debit(from_wallet, amount_cents)
                .credit(to_wallet, amount_cents)
                .memo(memo.clone()),
        )?;
        let sent = self.write_entry(
            from,
            LedgerKind::TransferOut,
            tx.id,
            -amount_cents,
            memo.clone(),
            now_ms,
        );
        let received = self.write_entry(
            to,
            LedgerKind::TransferIn,
            tx.id,
            amount_cents,
            memo,
            now_ms,
        );
        Ok((sent, received))
    }

    /// Every wallet in the world, in id order, with a name where there is
    /// one to give it.
    ///
    /// The names come from the four places a wallet can belong to — an
    /// account, a merchant, a budget, a symbol's payouts — and a wallet that
    /// belongs to none of them is one of the world's own, whose kind already
    /// says what it is.
    #[must_use]
    pub fn wallet_rows(&self) -> Vec<WalletRow> {
        let accounts: BTreeMap<WalletId, &Account> =
            self.accounts.values().map(|a| (a.wallet, a)).collect();
        let npcs: BTreeMap<WalletId, &Npc> = self
            .npcs
            .values()
            .filter_map(|npc| {
                let account = self.accounts.get(&npc.account_id)?;
                Some((account.wallet, npc))
            })
            .collect();
        let budgets: BTreeMap<WalletId, &Budget> =
            self.rewards.budgets().map(|b| (b.wallet, b)).collect();
        let issuers: BTreeMap<WalletId, &'static str> =
            self.issuers.iter().map(|(sym, id)| (*id, *sym)).collect();
        self.ledger
            .wallets()
            .map(|w| {
                // A merchant's wallet is also an account's, and the merchant
                // is the more useful of the two names, so it is asked first.
                let owner = npcs
                    .get(&w.id)
                    .map(|npc| npc.name.clone())
                    .or_else(|| accounts.get(&w.id).map(|a| a.name.clone()))
                    .or_else(|| budgets.get(&w.id).map(|b| b.name.clone()))
                    .or_else(|| issuers.get(&w.id).map(|s| (*s).to_string()));
                WalletRow {
                    wallet: w.id,
                    kind: w.kind.label(),
                    status: w.status.label(),
                    balance_cents: w.balance_cents(),
                    reserved_cents: w.reserved_cents(),
                    available_cents: w.available_cents(),
                    account_id: accounts.get(&w.id).map(|a| a.id.0),
                    owner,
                }
            })
            .collect()
    }

    /// Everything the operator's dashboard reads, taken at one instant.
    ///
    /// See [`OverviewDto`] for why it is one call rather than eight.
    #[must_use]
    pub fn overview(&self) -> OverviewDto {
        let at = self.now();
        let mut jobs = JobsSummary {
            held: self.jobs.len(),
            ..JobsSummary::default()
        };
        for job in self.jobs.jobs() {
            match job.status {
                JobStatus::Running => {
                    jobs.running += 1;
                    jobs.next_due_ms = Some(
                        jobs.next_due_ms
                            .map_or(job.due_at_ms, |due| due.min(job.due_at_ms)),
                    );
                }
                JobStatus::Done => jobs.done += 1,
                JobStatus::Cancelled => jobs.cancelled += 1,
            }
        }
        let mut people = PeopleSummary {
            users: self.users.len(),
            accounts: self.accounts.len(),
            traders: self.traders.len(),
            players: self.players.len(),
            ..PeopleSummary::default()
        };
        for account in self.accounts.values() {
            match self.ledger.status(account.wallet) {
                fehu::ledger::WalletStatus::Frozen => people.frozen += 1,
                fehu::ledger::WalletStatus::Closed => people.closed += 1,
                fehu::ledger::WalletStatus::Active => {}
            }
        }
        OverviewDto {
            at_ms: at.0,
            supply: SupplyDto::of(self),
            flows: self
                .ledger
                .flows()
                .iter()
                .map(|(reason, flow)| FlowDto {
                    reason: reason.label(),
                    count: flow.count,
                    cents: flow.cents,
                })
                .collect(),
            wallets: self.wallet_rows(),
            budgets: self.budget_views(),
            rules: self.rewards.rules().cloned().collect(),
            npcs: self.npc_views(),
            effects: self.world_view(at),
            modifiers: self.world.active(at.0).cloned().collect(),
            jobs,
            people,
            outbox: self.outbox.position(),
            journal_seq: self.journal.seq(),
        }
    }

    /// Every budget, with what its wallet holds now.
    #[must_use]
    pub fn budget_views(&self) -> Vec<BudgetDto> {
        self.rewards
            .budgets()
            .map(|b| BudgetDto {
                wallet: b.wallet,
                name: b.name.clone(),
                created_at_ms: b.created_at_ms,
                balance_cents: self.ledger.balance(b.wallet),
                paid_cents: b.paid_cents,
                paid_count: b.paid_count,
            })
            .collect()
    }

    /// What production and demand multiply by, per symbol, at `at`.
    #[must_use]
    pub fn world_view(&self, at: Timestamp) -> Vec<SymbolEffects> {
        self.symbols
            .iter()
            .map(|s| SymbolEffects {
                symbol: s.ticker,
                production_bps: self
                    .world
                    .multiplier_bps(Effect::Production, s.ticker, at.0),
                demand_bps: self.world.multiplier_bps(Effect::Demand, s.ticker, at.0),
            })
            .collect()
    }

    /// Delist a symbol: withdraw its book, drop its stops, buy every holder
    /// out at `cents_per_share`, and take it off the market.
    ///
    /// The order matters, and so does doing all of it. A delisting that
    /// only removed the symbol would strand cash in buy reservations that no
    /// order can ever fill, and leave traders holding shares in a company
    /// with no price. So:
    ///
    /// * every resting order is cancelled, which releases exactly what it
    ///   reserved — cash for a buy, shares for a sell — and marks its record
    ///   cancelled, as an expiry does;
    /// * every untriggered stop is dropped. A stop reserves nothing, so
    ///   there is nothing to give back;
    /// * every holder is bought out at `cents_per_share`, credited as its own
    ///   [`LedgerKind::Delisting`](crate::account::LedgerKind::Delisting)
    ///   entry, and their position is removed.
    ///
    /// `cents_per_share` defaults to the last price and may be zero: a
    /// company can be worth nothing, and a bankruptcy is a delisting that
    /// pays its holders nothing. It is money from outside the market, exactly
    /// as a dividend is — somebody bought the company — and
    /// `shares_outstanding` leaves with the listing.
    ///
    /// What stays behind is history: the fills, the ledger entries and the
    /// order records of a delisted symbol still name it, and its ticker is
    /// still registered, so re-listing it later reuses the same name. What
    /// does not stay is the position: a flat position in a symbol that no
    /// longer exists is a row nothing can price, so the ledger keeps the
    /// money and the position goes.
    ///
    /// # Errors
    /// [`DelistError`] if there is no such symbol or the price is not a
    /// price.
    pub async fn delist(
        &mut self,
        ticker: &str,
        cents_per_share: Option<i64>,
        note: Option<String>,
        now_ms: i64,
    ) -> Result<Delisting, DelistError> {
        let index = self
            .symbols
            .iter()
            .position(|s| s.ticker.eq_ignore_ascii_case(ticker))
            .ok_or(DelistError::Unknown)?;
        let symbol = Arc::clone(&self.symbols[index]);
        let sym = symbol.ticker;
        if cents_per_share.is_some_and(|c| c < 0) {
            return Err(DelistError::Price);
        }
        // What the buyout will cost, before anything is wound up. A symbol
        // whose issuer cannot buy its holders out must stay listed: winding
        // the book up first and discovering it afterwards would leave the
        // shares cancelled and unpaid for.
        if let Some(price) = cents_per_share {
            self.check_buyout(sym, price)?;
        }
        let wound = symbol
            .change(|s| s.wind_up())
            .await
            .map_err(|_| DelistError::Unknown)?;
        let cents_per_share = cents_per_share.unwrap_or(wound.last_price_cents);
        let mut orders_cancelled = 0;
        for (trader, resting) in &wound.cancelled {
            self.release(*trader, sym, resting, now_ms);
            orders_cancelled += 1;
            if let Some(order) = self.orders.get(&resting.id.0).cloned() {
                self.announce(StreamMessage::OrderExpired {
                    trader_id: trader.0,
                    order,
                });
            }
        }
        let mut paid = Delisting {
            symbol: sym,
            cents_per_share,
            last_price_cents: wound.last_price_cents,
            orders_cancelled,
            stops_cancelled: wound.stops_cancelled,
            shares_bought_out: 0,
            accounts_paid: 0,
            total_cents: 0,
        };
        // Who is owed what. The shares go either way — the company is gone —
        // but the money for them is one transaction out of the issuer's
        // wallet, so every holder is paid or the delisting does not happen.
        let owed: Vec<(TraderId, AccountId, i64, u64)> = self
            .traders
            .values()
            .filter_map(|t| {
                let qty = u64::try_from(t.positions.get(sym).map_or(0, |p| p.qty)).ok()?;
                (qty > 0).then_some(())?;
                let amount = fehu::ledger::checked_notional_cents(cents_per_share, qty).ok()?;
                Some((t.id, t.account_id, amount, qty))
            })
            .collect();
        let tx_id = if owed.iter().any(|(_, _, amount, _)| *amount > 0) {
            let issuer = self.issuer_wallet(sym);
            let mut draft = Draft::new(Reason::Delisting)
                .at(u64::try_from(now_ms).unwrap_or(0))
                .memo(note.clone());
            for (_, account_id, amount, _) in &owed {
                let Some(wallet) = self.accounts.get(account_id).map(|a| a.wallet) else {
                    continue;
                };
                draft = draft.debit(issuer, *amount).credit(wallet, *amount);
            }
            Some(self.ledger.post(draft).map_err(DelistError::Unfunded)?.id)
        } else {
            // A company can be worth nothing. Nothing moves, and a zero-cent
            // row in a ledger records only that it was.
            None
        };
        let holders: Vec<TraderId> = self.traders.keys().copied().collect();
        for id in holders {
            let Some(trader) = self.traders.get_mut(&id) else {
                continue;
            };
            // Every resting sell in this symbol was cancelled above, so
            // nothing is still promised to one.
            trader.reserved_shares.remove(sym);
            trader.positions.remove(sym);
        }
        for (_, account_id, amount, qty) in owed {
            paid.shares_bought_out = paid.shares_bought_out.saturating_add(qty);
            let (Some(tx_id), true) = (tx_id, amount > 0) else {
                continue;
            };
            let Self {
                ledger, accounts, ..
            } = self;
            let Some(account) = accounts.get_mut(&account_id) else {
                continue;
            };
            account.record(
                ledger,
                LedgerKind::Delisting,
                tx_id,
                amount,
                now_ms,
                Some(sym),
                None,
                note.clone(),
            );
            paid.accounts_paid += 1;
            paid.total_cents = paid.total_cents.saturating_add(amount);
        }
        // The book goes, and its order-id counter with it. The ids it handed
        // out are still in the log, so the counter keeps the next order from
        // reaching back into them.
        self.next_order_id = self.next_order_id.max(wound.next_order_id);
        self.symbols.remove(index);
        self.publish_listings();
        self.announce(StreamMessage::Delisted(paid.clone()));
        Ok(paid)
    }

    /// What a buyout at `cents_per_share` would cost, checked against the
    /// symbol's issuer wallet without moving anything.
    fn check_buyout(&self, sym: &str, cents_per_share: i64) -> Result<(), DelistError> {
        let Some(&issuer) = self.issuers.get(sym) else {
            // No issuer wallet yet means nothing has ever been paid out of
            // one; a buyout of nothing needs no funding.
            return if self.held_shares(sym) == 0 || cents_per_share == 0 {
                Ok(())
            } else {
                Err(DelistError::Unfunded(LedgerError::UnknownWallet(WalletId(
                    0,
                ))))
            };
        };
        let total = fehu::ledger::checked_notional_cents(cents_per_share, self.held_shares(sym))
            .map_err(DelistError::Unfunded)?;
        let available = self.ledger.available(issuer);
        if total > available {
            return Err(DelistError::Unfunded(LedgerError::Insufficient {
                wallet: issuer,
                needed_cents: total,
                available_cents: available,
            }));
        }
        Ok(())
    }

    /// Freeze an account: its resting orders and stops are withdrawn, and
    /// then nothing more leaves it.
    ///
    /// The cancels are not a courtesy — they are what makes the freeze safe.
    /// A resting order outliving a freeze would fill against a wallet that
    /// can no longer pay, and the book cannot be unwound after the fact. So
    /// the withdrawal and the freeze are one job on the market, and a frozen
    /// account has nothing outstanding to settle.
    ///
    /// Credits still land: a frozen holder is still paid their dividend,
    /// because they still own the shares it is paid on.
    pub async fn freeze_account(&mut self, account: AccountId) -> Result<(), MoneyError> {
        let wallet = self.wallet_of(account)?;
        let traders: Vec<TraderId> = self
            .traders
            .values()
            .filter(|t| t.account_id == account)
            .map(|t| t.id)
            .collect();
        for trader in traders {
            self.cancel_all(trader).await;
            for stop in self.stops_of(trader).await {
                let Some(symbol) = self.symbol(stop.symbol).cloned() else {
                    continue;
                };
                self.cancel_stop(&symbol, trader, stop.stop_id).await;
            }
        }
        self.ledger.freeze(wallet)?;
        Ok(())
    }

    /// Let a frozen account move money again.
    pub fn unfreeze_account(&mut self, account: AccountId) -> Result<(), MoneyError> {
        let wallet = self.wallet_of(account)?;
        self.ledger.unfreeze(wallet)?;
        Ok(())
    }

    /// Close an account for good.
    ///
    /// A separate transition from freezing, with a precondition instead of
    /// an authority: the wallet must be empty, holding nothing and reserving
    /// nothing, so closing can never strand currency where nothing can reach
    /// it. Withdraw the balance and cancel the orders first.
    pub fn close_account(&mut self, account: AccountId) -> Result<(), MoneyError> {
        let wallet = self.wallet_of(account)?;
        self.ledger.close(wallet)?;
        Ok(())
    }

    /// One engine step: advance every symbol to `target` — all of them at
    /// once, each on its own actor — then settle each in listing order:
    /// book the fills, review the halt, sweep the expired orders, place the
    /// stops that fired, and publish it all. Returns the ticks emitted.
    ///
    /// This is one job on the market, so no order can slip in between a
    /// book moving and the money following it.
    pub async fn step(&mut self, target: Timestamp) -> u64 {
        let halts = self.halts;
        let symbols = self.symbols.clone();
        // Post every step first, so the symbols work at once; then take the
        // results in listing order.
        let replies: Vec<_> = symbols
            .iter()
            .map(|symbol| symbol.actor.request(move |s| s.step(target, halts)))
            .collect();
        let mut total = 0;
        for (symbol, reply) in symbols.iter().zip(replies) {
            let Ok(stepped) = reply.await else {
                continue;
            };
            total += stepped.ticks;
            self.settle(symbol, stepped).await;
        }
        // What the world made while the market moved, and what the events
        // that moved it have left to say. Both before the merchants re-quote,
        // so a merchant quotes the world as it is after the step rather than
        // as it was before it.
        self.complete_jobs(target).await;
        self.world.forget(target.0);
        self.requote_npcs().await;
        total
    }

    /// Book and publish what one symbol's step did.
    async fn settle(&mut self, symbol: &Symbol, stepped: Stepped) {
        let sym = symbol.ticker;
        if let Some(tick) = stepped.tick {
            self.stream.publish(tick);
        }
        self.book(sym, &stepped.trades);
        if let Some(status) = stepped.status {
            self.announce(StreamMessage::Status(status));
        }
        self.book(sym, &stepped.resumed);
        // Before the stops, so a trigger cannot fire an order that would
        // immediately be swept.
        self.sweep_expired(symbol, stepped.clock_ms).await;
        for stop in stepped.fired {
            let trader = TraderId(stop.trader_id);
            let placed = self
                .place(
                    symbol,
                    trader,
                    PlaceRequest {
                        order: stop.order(),
                        client_order_id: stop.client_order_id.clone(),
                        post_only: false,
                        day: false,
                        expires_at_ms: None,
                        display_qty: None,
                        logged: true,
                    },
                )
                .await;
            let (order, refused) = match placed {
                Ok(placed) => (Some(placed.response().clone()), None),
                Err(e) => (None, Some(e.to_string())),
            };
            self.announce(StreamMessage::StopTriggered {
                trader_id: stop.trader_id,
                stop,
                price_cents: stepped.price_cents,
                order,
                refused,
            });
        }
    }

    /// Withdraw every resting order in `symbol` whose time is up.
    ///
    /// A day order and a good-till-date order differ only in where the
    /// deadline came from; by the time the engine sees them they are both
    /// just a resting order with an `expires_at_ms`. The sweep runs at the
    /// end of every step, on every symbol — a halted one included, because a
    /// halt stops trading, not the clock, and an order whose date has passed
    /// should not come back when the market does.
    async fn sweep_expired(&mut self, symbol: &Symbol, now_ms: i64) {
        let sym = symbol.ticker;
        let due: Vec<(u64, TraderId)> = self
            .orders
            .values()
            .filter(|o| o.symbol == sym && o.has_expired(now_ms))
            .map(|o| (o.order_id, TraderId(o.trader_id)))
            .collect();
        for (order_id, trader) in due {
            // Filled or already gone between the log and the book: the
            // record is no longer live, so nothing is owed.
            let Ok(Some(_)) = self.cancel(symbol, trader, order_id).await else {
                continue;
            };
            if let Some(order) = self.orders.get(&order_id).cloned() {
                self.announce(StreamMessage::OrderExpired {
                    trader_id: trader.0,
                    order,
                });
            }
        }
    }

    /// Every symbol as a trader's records see it, in listing order.
    pub async fn views(&self) -> Vec<SymbolView> {
        let mut views = Vec::with_capacity(self.symbols.len());
        for symbol in &self.symbols {
            if let Ok(Some(view)) = symbol.ask_listed(SymbolView::of).await {
                views.push(view);
            }
        }
        views
    }

    /// The counts `GET /api/health` reports.
    pub async fn health(&self) -> MarketHealth {
        let mut health = MarketHealth {
            symbols: self.symbols.len(),
            users: self.users.len(),
            accounts: self.accounts.len(),
            traders: self.traders.len(),
            cash_cents: self
                .accounts
                .values()
                .map(|a| a.balance_cents(&self.ledger))
                .fold(0i64, i64::saturating_add),
            orders_placed: self.orders_placed,
            orders_refused: self.orders_refused,
            fills_booked: self.fills_booked,
            settlement_failures: self.settlement_failures,
            events_logged: self.events.len(),
            ..MarketHealth::default()
        };
        for symbol in &self.symbols {
            let Ok((ticks, trades, resting, stops)) = symbol
                .ask(|s| {
                    (
                        s.ticks_total,
                        s.trades_total,
                        s.exchange
                            .book()
                            .orders()
                            .filter(|o| o.owner != fehu::Owner::Synthetic)
                            .count(),
                        s.stops.len(),
                    )
                })
                .await
            else {
                continue;
            };
            health.ticks_total += ticks;
            health.trades_total += trades;
            health.resting_orders += resting;
            health.stops_held += stops;
        }
        health
    }

    /// Everything the server would need to carry on after a restart: a
    /// copy of every symbol and of the clearing, taken in one job so
    /// nothing is halfway through anything.
    pub async fn snapshot(&self) -> Save {
        let mut symbols = Vec::with_capacity(self.symbols.len());
        let mut sim_now_ms = self.clock.now().0;
        let mut next_order_id = self.next_order_id;
        for symbol in &self.symbols {
            let Ok((save, clock_ms, book_next)) = symbol
                .ask(|s| {
                    (
                        s.to_save(),
                        s.exchange.clock().0,
                        s.exchange.book().next_order_id(),
                    )
                })
                .await
            else {
                continue;
            };
            // The furthest the market has reached: usually the clock, but an
            // engine step can leave a symbol ahead of it, and starting up
            // behind a symbol's own clock would freeze it until wall time
            // caught up.
            sim_now_ms = sim_now_ms.max(clock_ms);
            next_order_id = next_order_id.max(book_next);
            symbols.push(save);
        }
        Save {
            version: STATE_VERSION,
            saved_at_ms: wall_now_ms(),
            sim_now_ms,
            symbols,
            market: MarketSave {
                ledger: self.ledger.clone(),
                wallets: self.wallets,
                issuers: self
                    .issuers
                    .iter()
                    .map(|(sym, id)| ((*sym).to_string(), *id))
                    .collect(),
                catalog: self.catalog.clone(),
                npcs: self.npcs.values().cloned().collect(),
                recipes: self.recipes.clone(),
                jobs: self.jobs.to_saved(),
                next_job_id: self.jobs.next_id(),
                rewards: self.rewards.clone(),
                world: self.world.clone(),
                users: self.users.values().cloned().collect(),
                accounts: self.accounts.values().cloned().collect(),
                traders: self.traders.values().cloned().collect(),
                // Oldest first, so restoring in order rebuilds the same log.
                orders: self
                    .order_ids
                    .iter()
                    .filter_map(|id| self.orders.get(id))
                    .cloned()
                    .collect(),
                order_responses: self
                    .order_ids
                    .iter()
                    .filter_map(|id| {
                        let record = self.orders.get(id)?;
                        Some((*id, record.accepted.clone()?))
                    })
                    .collect(),
                events: self.events.iter().cloned().collect(),
                api_keys: self.directory.keys.pairs(),
                players: self.players.values().cloned().collect(),
                services: self.directory.services.to_saved(),
                next_service_id: self.next_service_id,
                next_user_id: self.next_user_id,
                next_account_id: self.next_account_id,
                next_trader_id: self.next_trader_id,
                next_event_id: self.next_event_id,
                next_stop_id: self.next_stop_id,
                next_order_id: next_order_id.0,
                outbox: self.outbox.clone(),
                journal_seq: self.journal.seq(),
                commands: self.commands.records(),
            },
        }
    }
}

/// How many of `trader`'s orders are resting in this symbol's book.
fn resting_orders(s: &SymbolState, trader: TraderId) -> u32 {
    let owner = Owner::Trader(trader);
    u32::try_from(
        s.exchange
            .book()
            .orders()
            .filter(|o| o.owner == owner)
            .count(),
    )
    .unwrap_or(u32::MAX)
}

/// What a market is built with.
struct MarketParts {
    ledger: Ledger,
    wallets: Wallets,
    issuers: BTreeMap<&'static str, WalletId>,
    catalog: Catalog,
    npcs: BTreeMap<TraderId, Npc>,
    recipes: RecipeBook,
    jobs: JobBook,
    rewards: RewardBook,
    world: WorldEffects,
    symbols: Vec<SymbolState>,
    directory: Directory,
    events: VecDeque<EventRecord>,
    next_event_id: u64,
    players: BTreeMap<String, Player>,
    next_service_id: u64,
    users: BTreeMap<UserId, User>,
    next_user_id: u64,
    accounts: BTreeMap<AccountId, Account>,
    next_account_id: u64,
    traders: BTreeMap<TraderId, Trader>,
    next_trader_id: u64,
    next_stop_id: u64,
    next_order_id: fehu::OrderId,
    /// Oldest first, with their accepted responses.
    orders: Vec<OrderRecord>,
    /// The journal sequence the snapshot this was built from included.
    journal_seq: u64,
    /// What each `Idempotency-Key` answered, oldest first.
    commands: Vec<crate::journal::CommandRecord>,
    /// The facts committed since the snapshot's consumer last acknowledged.
    outbox: crate::outbox::Outbox,
}

impl Market {
    /// Assemble the market and its symbol actors. Needs a runtime.
    fn build(
        parts: MarketParts,
        options: &Options,
        clock: SimClock,
        stream: Stream,
    ) -> (
        Self,
        watch::Receiver<Arc<Listings>>,
        watch::Receiver<Arc<Directory>>,
    ) {
        let symbols: Vec<Arc<Symbol>> = parts.symbols.into_iter().map(Symbol::spawn).collect();
        let (listings_tx, listings_rx) = watch::channel(Arc::new(Listings {
            symbols: symbols.clone(),
        }));
        let (directory_tx, directory_rx) = watch::channel(Arc::new(parts.directory.clone()));
        let event_cap = options.event_log.max(1);
        let mut events = parts.events;
        while events.len() > event_cap {
            events.pop_front();
        }
        let mut market = Self {
            ledger: parts.ledger,
            wallets: parts.wallets,
            issuers: parts.issuers,
            catalog: parts.catalog,
            npcs: parts.npcs,
            recipes: parts.recipes,
            jobs: parts.jobs,
            rewards: parts.rewards,
            world: parts.world,
            settlement_failures: 0,
            symbols,
            listings_tx,
            directory: parts.directory,
            directory_tx,
            stream,
            clock,
            halts: options.halts(),
            max_symbols: options.max_symbols,
            events,
            event_cap,
            next_event_id: parts.next_event_id.max(1),
            players: parts.players,
            next_service_id: parts.next_service_id.max(1),
            users: parts.users,
            next_user_id: parts.next_user_id.max(1),
            accounts: parts.accounts,
            next_account_id: parts.next_account_id.max(1),
            traders: parts.traders,
            next_trader_id: parts.next_trader_id.max(1),
            next_stop_id: parts.next_stop_id.max(1),
            next_order_id: fehu::OrderId(parts.next_order_id.0.max(1)),
            orders: BTreeMap::new(),
            order_ids: VecDeque::new(),
            client_order_ids: BTreeMap::new(),
            fill_log: options.fill_log,
            ledger_log: options.ledger_log,
            order_log: options.order_log.max(1),
            orders_placed: 0,
            orders_refused: 0,
            fills_booked: 0,
            fees: options.fees(),
            issuer_float_cents: options.issuer_float_cents,
            options: options.clone(),
            pinned_now: None,
            journal: crate::journal::Journal::at(parts.journal_seq),
            commands: crate::journal::CommandLog::new(options.command_log),
            outbox: {
                let mut outbox = parts.outbox;
                outbox.set_cap(options.outbox);
                outbox
            },
            pending_facts: Vec::new(),
        };
        market.commands.restore(parts.commands);
        // The symbols a fresh world starts with are listed by being built
        // rather than through `list`, so their issuers are funded here. A
        // restored world already has both, and funding them again would be
        // paying the same float twice.
        let tickers: Vec<&'static str> = market.symbols.iter().map(|s| s.ticker).collect();
        for ticker in tickers {
            if !market.issuers.contains_key(ticker) {
                market.fund_issuer(ticker, options.issuer_float_cents);
            }
        }
        // Through the same door as a live order, so the client-id index and
        // the eviction order come out the same.
        for record in parts.orders {
            market.record_order(record);
        }
        (market, listings_rx, directory_rx)
    }
}

/// Warm a listing up under `options`: the venue's calendar, tick and lot,
/// and `history_days` days of coarse daily bars so the chart is not empty on
/// its first morning. `0` lists a company with no past, which is what an IPO
/// is.
///
/// The calendar, tick and lot are the venue's rather than the caller's: a
/// symbol that traded on a different grid to everything beside it would not
/// be one of its listings.
///
/// # Errors
/// The first [`fehu::ConfigError`] in the requested config.
fn prepare_listing(
    options: &Options,
    mut spec: SymbolSpec,
    history_days: usize,
    now: Timestamp,
) -> Result<SymbolState, fehu::ConfigError> {
    spec.config.start_ts = Timestamp(now.0 - history_days as i64 * DAY_MS);
    spec.config.market_hours = options.market_hours;
    spec.trading.rules = fehu::MarketRules {
        tick_cents: options.tick_cents,
        lot: options.lot,
    };
    // A good is never quoted synthetically, whatever the world does with
    // its stocks: its units are counted, and a print would be one nobody
    // issued.
    spec.trading.synthetic = options.synthetic && !spec.info.is_good();
    let mut state = SymbolState::create(spec, options.max_bars, options.tape_len)?;
    state.warm_up(history_days, now);
    Ok(state)
}

/// Maps wall time to simulated time: `sim_now = sim_epoch + elapsed × scale`.
#[derive(Clone, Copy, Debug)]
pub struct SimClock {
    wall_epoch: Instant,
    sim_epoch: Timestamp,
    /// Simulated seconds per wall second.
    pub scale: f64,
}

impl SimClock {
    /// The simulated time right now.
    pub fn now(&self) -> Timestamp {
        let ms = self.wall_epoch.elapsed().as_secs_f64() * self.scale * 1000.0;
        Timestamp(self.sim_epoch.0.saturating_add(ms as i64))
    }
}

/// Most recent prints carried in one `tick` stream message.
pub const MAX_STREAM_TRADES: usize = 20;
/// Book levels per side carried in one `tick` stream message.
pub const STREAM_BOOK_DEPTH: usize = 8;

/// What goes out over the SSE stream.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamMessage {
    /// First message on every connection. Its `seq` is where the connection
    /// joins rather than a message number of its own.
    Hello {
        sim_now_ms: i64,
        time_scale: f64,
        quotes: Vec<Quote>,
        /// The earliest sequence `?since=` can still ask for.
        oldest_seq: u64,
        /// Set when this connection asked to resume from further back than
        /// the replay buffer reaches: messages were missed for good, and the
        /// client should reload its snapshots rather than trust its state.
        gap: bool,
    },
    /// The last tick of one engine step for one symbol.
    Tick {
        symbol: &'static str,
        ts_ms: i64,
        price_cents: i64,
        volume: u64,
        /// Intervals for which at least one bar closed during the step, so a
        /// client can refetch instead of extending its last bar.
        closed: Vec<Interval>,
        bid_cents: Option<i64>,
        ask_cents: Option<i64>,
        /// Top of the book after the step.
        book: BookDto,
        /// Newest prints of the step, newest first, at most
        /// [`MAX_STREAM_TRADES`].
        trades: Vec<TradeDto>,
    },
    /// An event was accepted.
    Event(EventRecord),
    /// A trader's order executed (in whole or part).
    Fill { trader_id: u64, fill: FillRecord },
    /// A symbol stopped trading, or started again.
    Status(SymbolStatus),
    /// A resting order was withdrawn by the venue rather than by its owner:
    /// it reached its expiry, or its symbol was delisted. Either way the
    /// record is cancelled and whatever it reserved has been released.
    OrderExpired { trader_id: u64, order: OrderRecord },
    /// A symbol was listed. It is tradable from this message on.
    Listed { quote: Quote },
    /// A symbol was delisted. Its book and stops are gone, every holder has
    /// been bought out, and orders in it will be refused from here on.
    Delisted(Delisting),
    /// A production job came due and delivered what it made.
    JobDone(JobDelivery),
    /// A production job started: its inputs and its cost are gone. Sent to
    /// its owner, and into the outbox, because the game backend was not
    /// necessarily the one who started it.
    JobStarted(crate::jobs::Job),
    /// A production job was cancelled before it came due, and whatever was
    /// refundable came back.
    JobCancelled(crate::jobs::Job),
    /// A reward was paid out of a budget. Never sent for a duplicate: a
    /// repeated source id moves nothing, and the outbox carries what moved.
    RewardPaid(crate::rewards::RewardReceipt),
    /// Units of a good were bought from the catalogue and paid for.
    Purchased(crate::catalog::PurchaseReceipt),
    /// Units of a good were destroyed.
    Consumed(crate::catalog::ConsumeReceipt),
    /// Currency moved between two accounts on request. Sent to both owners.
    Transferred {
        from_account_id: u64,
        to_account_id: u64,
        amount_cents: i64,
        /// The balanced transaction that moved it.
        tx_id: u64,
        memo: Option<String>,
    },
    /// The operator minted currency into an account.
    Minted {
        account_id: u64,
        entry: crate::account::LedgerEntry,
    },
    /// The operator burned currency out of an account.
    Burned {
        account_id: u64,
        entry: crate::account::LedgerEntry,
    },
    /// A dividend was paid to every holder. Public, like the event that
    /// records it: it names totals, not who was paid what.
    Dividend(Dividend),
    /// A stop fired. It is held no longer: it either became the order in
    /// `order`, or was `refused` when the account was checked again.
    StopTriggered {
        trader_id: u64,
        stop: StopOrder,
        /// The price that reached the trigger.
        price_cents: i64,
        order: Option<OrderResponse>,
        refused: Option<String>,
    },
}

impl StreamMessage {
    /// The `type` this serialises with: the tag a client routes on, and what
    /// an outbox entry is filed under.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Hello { .. } => "hello",
            Self::Tick { .. } => "tick",
            Self::Event(_) => "event",
            Self::Fill { .. } => "fill",
            Self::Status(_) => "status",
            Self::OrderExpired { .. } => "order_expired",
            Self::Listed { .. } => "listed",
            Self::Delisted(_) => "delisted",
            Self::JobDone(_) => "job_done",
            Self::StopTriggered { .. } => "stop_triggered",
            Self::JobStarted(_) => "job_started",
            Self::JobCancelled(_) => "job_cancelled",
            Self::RewardPaid(_) => "reward_paid",
            Self::Purchased(_) => "purchased",
            Self::Consumed(_) => "consumed",
            Self::Transferred { .. } => "transferred",
            Self::Minted { .. } => "minted",
            Self::Burned { .. } => "burned",
            Self::Dividend(_) => "dividend",
        }
    }

    /// Whose message this is: the user it may be shown to, or `None` for
    /// market data everyone may see. A fill is its trader's; a transfer is
    /// both its accounts' owners'. Resolved through the published
    /// [`Directory`], so every open stream can check every message without
    /// asking the market.
    ///
    /// Two users at most: a transfer has two sides. Everything else has one
    /// party or none.
    #[must_use]
    pub fn audience(&self, directory: &Directory) -> Audience {
        let trader = |id: u64| Audience::One(directory.owner_of(TraderId(id)));
        let account = |id: u64| Audience::One(directory.account_owner(AccountId(id)));
        match self {
            Self::Fill { trader_id, .. }
            | Self::StopTriggered { trader_id, .. }
            | Self::OrderExpired { trader_id, .. } => trader(*trader_id),
            Self::JobStarted(job) | Self::JobCancelled(job) => trader(job.trader_id),
            Self::RewardPaid(receipt) => trader(receipt.trader_id),
            Self::Purchased(receipt) => trader(receipt.trader_id),
            Self::Consumed(receipt) => trader(receipt.trader_id),
            Self::Minted { account_id, .. } | Self::Burned { account_id, .. } => {
                account(*account_id)
            }
            Self::Transferred {
                from_account_id,
                to_account_id,
                ..
            } => Audience::Two(
                directory.account_owner(AccountId(*from_account_id)),
                directory.account_owner(AccountId(*to_account_id)),
            ),
            Self::Hello { .. }
            | Self::Tick { .. }
            | Self::Event(_)
            | Self::Status(_)
            | Self::Listed { .. }
            | Self::Delisted(_)
            | Self::JobDone(_)
            | Self::Dividend(_) => Audience::Everyone,
        }
    }
}

/// Who a [`StreamMessage`] may be shown to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Audience {
    /// Market data: every connection.
    Everyone,
    /// One party, if the directory still knows them. `None` inside means
    /// nobody: a message about a trader or account the directory does not
    /// have is shown to no one rather than to everyone.
    One(Option<UserId>),
    /// Both sides of a transfer.
    Two(Option<UserId>, Option<UserId>),
}

impl Audience {
    /// Whether `viewer` — a stream that proved it speaks for that user, or
    /// an anonymous one — may see the message.
    #[must_use]
    pub fn admits(self, viewer: Option<UserId>) -> bool {
        match self {
            Self::Everyone => true,
            Self::One(owner) => viewer.is_some() && viewer == owner,
            Self::Two(a, b) => viewer.is_some() && (viewer == a || viewer == b),
        }
    }
}

/// A stream message with its place in the stream.
///
/// Every message the server publishes takes the next number, so a client can
/// tell a gap from a quiet market and ask for what it missed with `?since=`.
/// On the `hello` that opens a connection the number means something slightly
/// different: it is where the connection joins, so the first live message is
/// `seq + 1`.
#[derive(Clone, Debug, Serialize)]
pub struct Sequenced {
    pub seq: u64,
    #[serde(flatten)]
    pub message: StreamMessage,
}

/// The sequence counter and the bounded buffer behind `?since=`, and the
/// broadcast every open stream listens on. One actor: a message takes its
/// number and goes into the buffer in the same job, and a subscription is
/// taken in a job of its own, so nothing can slip between the replay and
/// the live feed.
struct StreamLog {
    /// The number the next published message will take.
    next_seq: u64,
    /// The most recently published messages, oldest first.
    recent: VecDeque<Sequenced>,
    cap: usize,
    tx: broadcast::Sender<Sequenced>,
}

impl StreamLog {
    fn publish(&mut self, message: StreamMessage) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        let sequenced = Sequenced { seq, message };
        if self.cap > 0 {
            while self.recent.len() >= self.cap {
                self.recent.pop_front();
            }
            self.recent.push_back(sequenced.clone());
        }
        // `Err` only means nobody is listening right now.
        let _ = self.tx.send(sequenced);
        seq
    }

    fn subscribe(&self, since: Option<u64>) -> Subscription {
        let rx = self.tx.subscribe();
        let seq = self.next_seq.saturating_sub(1);
        let oldest_seq = self.recent.front().map_or(self.next_seq, |m| m.seq);
        let (replay, gap) = match since {
            None => (Vec::new(), false),
            Some(since) => (
                self.recent
                    .iter()
                    .filter(|m| m.seq > since)
                    .cloned()
                    .collect(),
                // The buffer starts after the first message they wanted, so
                // whatever fell out of it is gone for good.
                since + 1 < oldest_seq,
            ),
        };
        Subscription {
            rx,
            seq,
            oldest_seq,
            replay,
            gap,
        }
    }
}

/// The SSE stream: where every message the server publishes goes.
///
/// Publishing is fire-and-forget and synchronous, so an actor can publish
/// from inside a job. Nothing reaches a client any other way: the number a
/// message is given is what lets a reconnecting client tell "nothing
/// happened" from "I missed something".
#[derive(Clone)]
pub struct Stream {
    actor: Actor<StreamLog>,
    tx: broadcast::Sender<Sequenced>,
}

impl Stream {
    fn new(replay: usize) -> Self {
        let (tx, _) = broadcast::channel(4096);
        Self {
            actor: Actor::spawn(StreamLog {
                // The first message published is 1, so 0 is "nothing yet"
                // and a client may ask for everything with `?since=0`.
                next_seq: 1,
                recent: VecDeque::new(),
                cap: replay,
                tx: tx.clone(),
            }),
            tx,
        }
    }

    /// Publish a message to every open stream, numbering it and keeping it
    /// in the replay buffer. Messages are numbered in the order they are
    /// published.
    pub fn publish(&self, message: StreamMessage) {
        self.actor.send(move |log| {
            log.publish(message);
        });
    }

    /// Open a stream connection, optionally asking for everything after
    /// `since`. Every message is either in `replay` or arrives on `rx`,
    /// exactly once, in order.
    pub async fn subscribe(&self, since: Option<u64>) -> Result<Subscription, Gone> {
        self.actor.call(move |log| log.subscribe(since)).await
    }

    /// Messages published since start-up.
    pub async fn published(&self) -> u64 {
        self.actor
            .call(|log| log.next_seq.saturating_sub(1))
            .await
            .unwrap_or(0)
    }

    /// Streams open right now.
    pub fn subscribers(&self) -> usize {
        self.tx.receiver_count()
    }

    /// A raw receiver on the live feed, with no replay and no filtering.
    pub fn listen(&self) -> broadcast::Receiver<Sequenced> {
        self.tx.subscribe()
    }
}

/// A new stream connection: where it joins, what it missed, and the live
/// feed from there.
pub struct Subscription {
    pub rx: broadcast::Receiver<Sequenced>,
    /// The last sequence published before this connection opened.
    pub seq: u64,
    /// The earliest sequence the replay buffer still holds. Equal to
    /// `seq + 1` when nothing is buffered.
    pub oldest_seq: u64,
    /// What `?since=` asked for and the buffer still had, oldest first.
    pub replay: Vec<Sequenced>,
    /// Set when `?since=` reached further back than the buffer goes: some
    /// messages are gone for good and the client must reload its snapshots.
    pub gap: bool,
}

/// Shared application state: the addresses of the actors, and what is
/// published for reading without one. See the module docs.
pub struct App {
    pub options: Options,
    pub clock: SimClock,
    pub started_at: SystemTime,
    /// When a move halts a symbol, and for how long.
    pub halts: HaltPolicy,
    /// The market actor. Every change to money or to a book is a job here.
    pub market: Actor<Market>,
    /// The symbol table, as the market last published it.
    listings: watch::Receiver<Arc<Listings>>,
    /// Keys and trader owners, as the market last published them.
    directory: watch::Receiver<Arc<Directory>>,
    /// The SSE stream.
    pub stream: Stream,
    /// How fast each client may change the market.
    limits: Actor<Limiter>,
    /// How much work may be in the server at once. See [`crate::limit`].
    pub admission: crate::limit::Admission,
    /// Counters for `GET /api/health`. Atomics, so nothing waits on them.
    pub metrics: Metrics,
}

impl App {
    /// Build the market and warm every symbol up to "now". This runs the
    /// simulators synchronously; with the defaults it is roughly a million
    /// ticks in total, well under a second in a release build. The actors
    /// are spawned at the end, so this needs a tokio runtime.
    pub fn new(options: Options) -> Arc<Self> {
        let now = Timestamp(options.now_ms.unwrap_or_else(wall_now_ms));
        let fine_start =
            Interval::D1.bucket(now - Duration::from_secs(options.warmup_hours * 3600));
        let start_ts = Timestamp(fine_start.0 - options.history_days as i64 * DAY_MS);
        let symbols = seeded_symbols(start_ts)
            .into_iter()
            .map(|mut spec| {
                // Every symbol trades on the same calendar, if there is one,
                // and in the same tick and lot. Both are properties of the
                // listing, so a restored market keeps the ones it was saved
                // with rather than whatever the environment now says.
                spec.config.market_hours = options.market_hours;
                spec.trading.rules = fehu::MarketRules {
                    tick_cents: options.tick_cents,
                    lot: options.lot,
                };
                spec.trading.synthetic = options.synthetic;
                let mut s = SymbolState::new(spec, options.max_bars, options.tape_len);
                s.warm_up(options.history_days, now);
                s
            })
            .collect();
        // Genesis. Every cent the world will ever hold without an operator
        // minting more, in treasury, minus the float the synthetic
        // counterparty starts with so that trading against unfunded
        // liquidity does not start in debt on the first fill.
        let mut ledger = Ledger::new();
        let wallets = Wallets::open(&mut ledger);
        let genesis = options.genesis_cents.clamp(0, MAX_BALANCE_CENTS);
        if genesis > 0 {
            ledger
                .mint(wallets.treasury, genesis, Reason::Genesis)
                .expect("a fresh ledger takes its own genesis");
            // Nothing settles against the synthetic wallet in a world with
            // no synthetic liquidity, so setting currency aside for it would
            // only take it out of treasury for good.
            let float = if options.synthetic {
                options.synthetic_float_cents.clamp(0, genesis)
            } else {
                0
            };
            if float > 0 {
                ledger
                    .post(
                        Draft::new(Reason::Transfer)
                            .debit(wallets.treasury, float)
                            .credit(wallets.synthetic, float),
                    )
                    .expect("the float was clamped to what treasury holds");
            }
        }
        let (jobs, rewards) = (
            JobBook::with_cap(options.job_log),
            RewardBook::with_cap(options.reward_log),
        );
        Self::assemble(
            options,
            now,
            MarketParts {
                ledger,
                wallets,
                issuers: BTreeMap::new(),
                catalog: Catalog::default(),
                npcs: BTreeMap::new(),
                recipes: RecipeBook::default(),
                jobs,
                rewards,
                world: WorldEffects::default(),
                symbols,
                directory: Directory::default(),
                events: VecDeque::new(),
                next_event_id: 1,
                players: BTreeMap::new(),
                next_service_id: 1,
                users: BTreeMap::new(),
                next_user_id: 1,
                accounts: BTreeMap::new(),
                next_account_id: 1,
                traders: BTreeMap::new(),
                next_trader_id: 1,
                next_stop_id: 1,
                next_order_id: fehu::OrderId(1),
                orders: Vec::new(),
                journal_seq: 0,
                commands: Vec::new(),
                outbox: crate::outbox::Outbox::default(),
            },
        )
    }

    /// Build the app from a save instead of warming up: the market carries on
    /// from the simulated time it had reached, with the same books, bars,
    /// users and money.
    ///
    /// The save must already have been validated by [`crate::save::read`]:
    /// this is the low-level constructor and checks nothing.
    pub fn restore(options: Options, save: Save) -> Arc<Self> {
        let now = Timestamp(save.sim_now_ms);
        let symbols = save
            .symbols
            .into_iter()
            .filter_map(|mut s| {
                // Every symbol in a save that `save::read` accepted carries
                // its listing; one that does not cannot be rebuilt, and there
                // is no build metadata left to guess it from.
                let info = s.info.take()?;
                Some(SymbolState::from_save(info, s, options.tape_len))
            })
            .collect();
        let m = save.market;
        let directory = Directory {
            keys: Keyring::from_pairs(m.api_keys),
            services: Services::from_saved(m.services),
            owners: m.traders.iter().map(|t| (t.id, t.user_id)).collect(),
            account_owners: m.accounts.iter().map(|a| (a.id, a.user_id)).collect(),
        };
        let responses: BTreeMap<u64, OrderResponse> = m.order_responses.into_iter().collect();
        let orders = m
            .orders
            .into_iter()
            .map(|mut record| {
                record.accepted = responses.get(&record.order_id).cloned();
                record
            })
            .collect();
        let jobs = JobBook::from_saved(m.jobs, m.next_job_id, options.job_log);
        let rewards = {
            let mut rewards = m.rewards;
            rewards.set_cap(options.reward_log);
            rewards
        };
        Self::assemble(
            options,
            now,
            MarketParts {
                ledger: m.ledger,
                wallets: m.wallets,
                issuers: m
                    .issuers
                    .into_iter()
                    .filter_map(|(sym, id)| Some((crate::symbols::lookup(&sym)?, id)))
                    .collect(),
                catalog: m.catalog,
                npcs: m.npcs.into_iter().map(|n| (n.trader, n)).collect(),
                recipes: m.recipes,
                jobs,
                rewards,
                world: m.world,
                symbols,
                directory,
                events: m.events.into_iter().collect(),
                next_event_id: m.next_event_id,
                players: m
                    .players
                    .into_iter()
                    .map(|p| (p.external_id.clone(), p))
                    .collect(),
                next_service_id: m.next_service_id.max(1),
                users: m.users.into_iter().map(|u| (u.id, u)).collect(),
                next_user_id: m.next_user_id,
                accounts: m.accounts.into_iter().map(|a| (a.id, a)).collect(),
                next_account_id: m.next_account_id,
                traders: m.traders.into_iter().map(|t| (t.id, t)).collect(),
                next_trader_id: m.next_trader_id,
                next_stop_id: m.next_stop_id,
                next_order_id: fehu::OrderId(m.next_order_id),
                orders,
                journal_seq: m.journal_seq,
                commands: m.commands,
                outbox: m.outbox,
            },
        )
    }

    /// Restore `save` and then replay the journal entries it does not
    /// already contain: the state the server last acknowledged, rather than
    /// the state it last had time to write down.
    ///
    /// Entries at or below the snapshot's sequence are already in it and are
    /// dropped. The rest are applied in order through the same code that
    /// accepted them ([`crate::journal`]), with the simulated clock started
    /// at the furthest instant they reach so that a market which ran on past
    /// its last snapshot does not come back frozen until wall time catches
    /// up with it.
    pub async fn resume(
        options: Options,
        mut save: Save,
        entries: Vec<crate::journal::JournalEntry>,
    ) -> Arc<Self> {
        let after = save.market.journal_seq;
        let entries: Vec<_> = entries.into_iter().filter(|e| e.seq > after).collect();
        save.sim_now_ms = entries
            .iter()
            .map(|e| e.at_ms)
            .fold(save.sim_now_ms, i64::max);
        let app = Self::restore(options, save);
        app.replay(entries).await;
        app
    }

    /// Apply journaled commands, in order, as one market job.
    ///
    /// One job, so nothing else reaches the market halfway through a replay
    /// and the order on disk is the order they are applied in.
    pub async fn replay(&self, entries: Vec<crate::journal::JournalEntry>) -> usize {
        if entries.is_empty() {
            return 0;
        }
        let count = entries.len();
        let first = entries[0].seq;
        let last = entries[count - 1].seq;
        let _ = self
            .market
            .call_async(move |m| {
                Box::pin(async move {
                    for entry in entries {
                        m.replay(entry).await;
                    }
                })
            })
            .await;
        tracing::info!(count, first, last, "journal replayed");
        count
    }

    /// Start writing the journal to `path`, continuing after whatever the
    /// snapshot and the replay reached.
    ///
    /// Called once, after any replay: a journal attached before one would
    /// have the replayed commands written a second time.
    ///
    /// # Errors
    /// The file could not be created or its header written.
    /// Put a funded merchant behind every symbol that has none.
    ///
    /// A world with no synthetic ladder and no merchants has an empty book,
    /// which is what kept the ladder switched on by default when milestone 3
    /// built the switch. This is the other half of that: with
    /// `FEHU_SEED_MERCHANTS_CENTS` set, the demo starts as an economy —
    /// every listing has somebody standing behind it who paid for the
    /// inventory, and `FEHU_SYNTHETIC=0` leaves a book that still has two
    /// sides.
    ///
    /// Each merchant is given `cents` out of treasury and, at the reference
    /// price, about as much stock as that would buy: one that can sell as
    /// much as it can buy quotes both sides for as long as the price holds.
    /// Every one of them is created by the same journaled
    /// [`Command::CreateNpc`](crate::journal::Command::CreateNpc) a request
    /// would send, so a replayed world seeds identically — and it is only
    /// ever called for a world that was warmed up rather than restored,
    /// because a restored one already has whatever merchants it had.
    ///
    /// Returns how many were created.
    pub async fn seed_merchants(&self, cents: i64) -> usize {
        if cents <= 0 {
            return 0;
        }
        let mut made = 0;
        for symbol in self.listings().all() {
            let ticker = symbol.ticker;
            let Ok(reference) = symbol.ask(|s| s.price_cents()).await else {
                continue;
            };
            if reference <= 0 {
                continue;
            }
            let inventory = u64::try_from(cents / reference).unwrap_or(0);
            let at = self.clock.now();
            let wall_ms = wall_now_ms();
            let command = crate::journal::Command::CreateNpc {
                symbol: ticker.to_string(),
                name: Some(format!("{ticker} Merchant")),
                policy: Policy::default(),
                cash_cents: cents,
                inventory,
            };
            let key = Some(format!("seed-merchant:{ticker}"));
            let outcome = self
                .market
                .call_async(move |m| {
                    Box::pin(async move {
                        if m.npcs.values().any(|npc| npc.symbol == ticker) {
                            return None;
                        }
                        Some(
                            m.run_command(
                                crate::journal::Principal::Operator,
                                key,
                                at,
                                wall_ms,
                                command,
                            )
                            .await,
                        )
                    })
                })
                .await;
            match outcome {
                Ok(Some(Ok(_))) => made += 1,
                Ok(Some(Err(e))) => {
                    tracing::warn!(symbol = ticker, error = %e.message(), "no merchant seeded");
                }
                Ok(None) | Err(Gone) => {}
            }
        }
        made
    }

    pub async fn attach_journal(&self, path: &std::path::Path) -> Result<(), JournalError> {
        let path = path.to_path_buf();
        self.market
            .call(move |m| {
                let seq = m.journal.seq();
                crate::journal::Journal::open(&path, seq).map(|journal| m.journal = journal)
            })
            .await
            .map_err(|Gone| {
                JournalError::Io(std::io::Error::other("the market actor has stopped"))
            })?
    }

    /// Drop the journal entries a snapshot now holds. Failing to is not
    /// fatal — the entries are simply replayed and re-applied on the next
    /// start — so it is logged rather than reported.
    pub async fn truncate_journal(&self, seq: u64) {
        let _ = self
            .market
            .call(move |m| {
                if let Err(e) = m.journal.truncate_to(seq) {
                    tracing::warn!(seq, error = %e, "the journal was not truncated");
                }
            })
            .await;
    }

    fn assemble(options: Options, now: Timestamp, parts: MarketParts) -> Arc<Self> {
        let clock = SimClock {
            wall_epoch: Instant::now(),
            sim_epoch: now,
            scale: options.time_scale,
        };
        let stream = Stream::new(options.stream_replay);
        let (market, listings, directory) = Market::build(parts, &options, clock, stream.clone());
        let limits = Actor::spawn(Limiter::new(Rate {
            per_sec: options.rate_per_sec,
            burst: options.rate_burst,
        }));
        let admission = crate::limit::Admission::new(options.max_inflight, options.max_streams);
        Arc::new(Self {
            clock,
            started_at: SystemTime::now(),
            halts: options.halts(),
            market: Actor::spawn(market),
            listings,
            directory,
            stream,
            limits,
            admission,
            metrics: Metrics::default(),
            options,
        })
    }

    /// Build a listing from `spec` without touching the market.
    ///
    /// Warming a simulator up is the slow part of listing a symbol and none
    /// of it needs the market, so it happens here — on the request's own
    /// task — and the finished symbol is handed to [`Market::list`]
    /// afterwards. `history_days` days of coarse daily bars are generated so
    /// the chart is not empty on the first morning; `0` lists a company with
    /// no past, which is what an IPO is.
    ///
    /// The calendar, tick and lot are the market's rather than the caller's:
    /// they are properties of this venue, and a symbol that traded on a
    /// different grid to everything beside it would not be one of its
    /// listings.
    ///
    /// # Errors
    /// The first [`fehu::ConfigError`] in the requested config.
    pub fn prepare_listing(
        &self,
        spec: SymbolSpec,
        history_days: usize,
        now: Timestamp,
    ) -> Result<SymbolState, fehu::ConfigError> {
        prepare_listing(&self.options, spec, history_days, now)
    }

    /// The symbol table as the market last published it. A snapshot: a
    /// symbol in it may be delisted a moment later, which its actor will
    /// say.
    pub fn listings(&self) -> Arc<Listings> {
        self.listings.borrow().clone()
    }

    /// The listing for `ticker`, if there is one.
    pub fn symbol(&self, ticker: &str) -> Option<Arc<Symbol>> {
        self.listings.borrow().get(ticker).cloned()
    }

    /// The user `key` speaks for, if it is one of ours.
    pub fn user_of(&self, key: &str) -> Option<UserId> {
        self.directory.borrow().user_of(key)
    }

    /// The service `key` speaks for, if it is one of ours. Answered from the
    /// published directory, so authorising a game-backend request sends no
    /// job anywhere.
    pub fn service_of(&self, key: &str) -> Option<ServiceAuth> {
        self.directory.borrow().service_of(key)
    }

    /// The user `trader` belongs to, if the trader exists.
    pub fn owner_of(&self, trader: TraderId) -> Option<UserId> {
        self.directory.borrow().owner_of(trader)
    }

    /// The user `account` belongs to, if the account exists.
    pub fn account_owner(&self, account: AccountId) -> Option<UserId> {
        self.directory.borrow().account_owner(account)
    }

    /// The directory as the market last published it, for a reader that
    /// will ask it several things and wants them from one snapshot.
    pub fn directory(&self) -> Arc<Directory> {
        Arc::clone(&self.directory.borrow())
    }

    /// Every symbol's quote, in listing order, each from its own actor.
    pub async fn quotes(&self) -> Vec<Quote> {
        let listings = self.listings();
        let replies: Vec<_> = listings
            .all()
            .iter()
            .map(|s| s.actor.request(|s| (!s.delisted).then(|| s.quote())))
            .collect();
        let mut quotes = Vec::with_capacity(replies.len());
        for reply in replies {
            if let Ok(Some(quote)) = reply.await {
                quotes.push(quote);
            }
        }
        quotes
    }

    /// Every symbol as a trader's records see it, in listing order, each
    /// from its own actor.
    pub async fn views(&self) -> Vec<SymbolView> {
        let listings = self.listings();
        let replies: Vec<_> = listings
            .all()
            .iter()
            .map(|s| {
                s.actor
                    .request(|s| (!s.delisted).then(|| SymbolView::of(s)))
            })
            .collect();
        let mut views = Vec::with_capacity(replies.len());
        for reply in replies {
            if let Ok(Some(view)) = reply.await {
                views.push(view);
            }
        }
        views
    }

    /// Every stop `trader` holds, across every symbol.
    pub async fn stops_of(&self, trader: TraderId) -> Vec<StopOrder> {
        self.views()
            .await
            .into_iter()
            .flat_map(|v| v.stops)
            .filter(|s| s.trader_id == trader.0)
            .collect()
    }

    /// What a symbol's trading state is right now.
    pub async fn status(&self, ticker: &str) -> Option<SymbolStatus> {
        let (now, halts) = (self.clock.now(), self.halts);
        self.symbol(ticker)?
            .ask_listed(move |s| s.status(now, halts))
            .await
            .ok()
            .flatten()
    }

    /// Everything the server would need to carry on after a restart.
    pub async fn save(&self) -> Save {
        self.market
            .call_async(|m| Box::pin(m.snapshot()))
            .await
            .expect("the market actor is running")
    }

    /// Reconcile the whole market, from one consistent snapshot of it.
    pub async fn reconcile(&self) -> crate::reconcile::Reconciliation {
        crate::reconcile::reconcile(&self.save().await)
    }

    /// Append an event to the log and publish it.
    pub async fn record(&self, rec: EventRecord) -> Result<EventRecord, Gone> {
        self.market.call(move |m| m.record(rec)).await
    }

    /// Publish a message to every open stream.
    pub fn publish(&self, message: StreamMessage) {
        self.stream.publish(message);
    }

    /// Open a stream connection, optionally asking for everything after
    /// `since`.
    pub async fn subscribe(&self, since: Option<u64>) -> Result<Subscription, Gone> {
        self.stream.subscribe(since).await
    }

    /// Spend one request's worth of a client's allowance for a request that
    /// would change something. `who` is `None` for a request with no key,
    /// which shares one bucket with every other.
    ///
    /// Timed off the wall clock rather than the simulated one: a limit is
    /// about how fast requests actually arrive, and `FEHU_TIME_SCALE` must
    /// not be able to buy a client more of them.
    pub async fn allow(&self, who: Option<UserId>) -> Decision {
        let since_start = self.started_at.elapsed().unwrap_or_default();
        let at_ms = since_start.as_millis().min(u128::from(u64::MAX)) as u64;
        self.limits
            .call(move |limits| limits.take(who, at_ms))
            .await
            .unwrap_or(Decision::Allowed)
    }

    /// Messages published to the stream since start-up.
    pub async fn published(&self) -> u64 {
        self.stream.published().await
    }

    /// A raw receiver on the live feed, with no replay and no filtering.
    pub fn listen(&self) -> broadcast::Receiver<Sequenced> {
        self.stream.listen()
    }

    /// Clients whose allowance the limiter is currently tracking.
    pub async fn tracked_clients(&self) -> usize {
        self.limits.call(|l| l.tracked()).await.unwrap_or(0)
    }
}

/// Start-up options, all overridable through `FEHU_*` environment variables.
#[derive(Clone, Debug)]
pub struct Options {
    /// Daily bars of coarse pre-history to generate. `FEHU_HISTORY_DAYS`.
    pub history_days: usize,
    /// Hours of fine 1 s ticks before "now" (rounded down to the start of that
    /// UTC day, so the intraday history is between this and this + 24 h).
    /// `FEHU_WARMUP_HOURS`.
    pub warmup_hours: u64,
    /// Simulated seconds per wall second. `FEHU_TIME_SCALE`.
    pub time_scale: f64,
    /// Simulated "now" at start-up; defaults to the wall clock. `FEHU_NOW_MS`.
    pub now_ms: Option<i64>,
    /// Completed bars retained per interval. `FEHU_MAX_BARS`.
    pub max_bars: usize,
    /// Events retained in the log. `FEHU_EVENT_LOG`.
    pub event_log: usize,
    /// Trades retained on each symbol's tape. `FEHU_TAPE`.
    pub tape_len: usize,
    /// Fills retained per trader. `FEHU_FILL_LOG`.
    pub fill_log: usize,
    /// Ledger entries retained per account. `FEHU_LEDGER_LOG`.
    pub ledger_log: usize,
    /// Orders retained in the order log. Live orders are never dropped.
    /// `FEHU_ORDER_LOG`.
    pub order_log: usize,
    /// Cash a new account is opened with, in cents.
    /// `FEHU_STARTING_CASH_CENTS`.
    ///
    /// Paid out of treasury, not minted: opening an account moves currency
    /// that already exists. A treasury with less than this left refuses the
    /// account rather than printing the difference.
    pub starting_cash_cents: i64,
    /// The world's opening supply, minted into treasury at start-up.
    /// `FEHU_GENESIS_CENTS`.
    ///
    /// Every cent the world will hold unless an operator mints more. It caps
    /// how many accounts the faucet can fund and how much an issuer can be
    /// given to pay dividends out of.
    pub genesis_cents: i64,
    /// What the synthetic counterparty starts with, out of the genesis
    /// supply. `FEHU_SYNTHETIC_FLOAT_CENTS`.
    ///
    /// The simulator's ladder and printed flow are not funded by anyone, so
    /// a wallet stands in for them. Starting it with a float means the
    /// ordinary case — players buying before they sell — settles out of real
    /// currency; past the float it goes into debt, which
    /// [`crate::reconcile`] reports rather than hides. It is retired when
    /// both sides of every fill are funded.
    pub synthetic_float_cents: i64,
    /// Whether symbols are quoted by the simulator's synthetic ladder and
    /// its printed flow. `FEHU_SYNTHETIC=0` turns it off for the whole
    /// world.
    ///
    /// On, a fresh symbol is tradable with nobody else in the market, which
    /// is what makes the demo a demo — at the price that every fill against
    /// it is currency put into a player's hands by nobody, measured as the
    /// synthetic wallet's debt. Off, the only liquidity is what somebody
    /// funded: players' orders and the NPCs the world runs
    /// ([`crate::npc`]), and the synthetic wallet stays at zero because
    /// nothing ever settles against it.
    ///
    /// A good ignores this and is never quoted synthetically: its units are
    /// counted, and a print would be a unit nobody issued.
    pub synthetic: bool,
    /// What each newly listed symbol's issuer wallet is funded with out of
    /// treasury, in cents. `FEHU_ISSUER_FLOAT_CENTS`.
    ///
    /// Dividends and delisting buyouts are paid out of it, and a payout it
    /// cannot fund is refused rather than clipped. Zero leaves issuers empty,
    /// so every payout has to be funded by an operator first.
    pub issuer_float_cents: i64,
    /// Key the game-master endpoints (pushing events into the simulation)
    /// require. `FEHU_ADMIN_KEY`; unset leaves them open, which is what a
    /// single-player game on localhost wants and a shared server does not.
    pub admin_key: Option<String>,
    /// Where the market is saved, and read back from at start-up.
    /// `FEHU_STATE_FILE`; unset means nothing is kept and every start warms
    /// up a fresh market.
    pub state_file: Option<std::path::PathBuf>,
    /// Seconds between saves. `FEHU_SAVE_SECS`.
    pub save_secs: u64,
    /// The trading calendar every symbol runs on. `FEHU_MARKET_HOURS`, as
    /// `HH:MM-HH:MM` in UTC (`09:30-16:00`); unset means the market never
    /// closes, which is what a game whose players log in at all hours wants.
    pub market_hours: Option<MarketHours>,
    /// How far the price may move from the band the day opened with before
    /// trading stops, as a fraction. `FEHU_PRICE_LIMIT_PCT`; `0` turns
    /// automatic halts off.
    pub price_limit_pct: f64,
    /// How long an automatic halt lasts, in simulated seconds.
    /// `FEHU_HALT_SECS`.
    pub halt_secs: u64,
    /// Stream messages kept for `?since=` replay. `FEHU_STREAM_REPLAY`; `0`
    /// keeps none, and every reconnect is then a gap.
    pub stream_replay: usize,
    /// How fast one client may change the market, in requests per second.
    /// `FEHU_RATE_PER_SEC`; `0` turns rate limiting off. Reads are never
    /// limited.
    pub rate_per_sec: f64,
    /// Requests one client may send at once after a quiet spell.
    /// `FEHU_RATE_BURST`.
    pub rate_burst: f64,
    /// The price step every symbol quotes and trades in, in cents.
    /// `FEHU_TICK_CENTS`; `1` allows every cent, which is the default.
    pub tick_cents: i64,
    /// The share lot every symbol trades in. `FEHU_LOT`; `1` allows every
    /// share, which is the default.
    pub lot: u64,
    /// Symbols the market may list at once. `FEHU_MAX_SYMBOLS`. Every one of
    /// them is a simulator stepped on every engine tick, so this is a bound
    /// on how much work a step is, not just on how long a list is.
    pub max_symbols: usize,
    /// What a taker pays, in basis points of the fill's notional.
    /// `FEHU_TAKER_FEE_BPS`; `0`, the default, charges nothing. Negative
    /// values are refused: the venue does not pay takers.
    pub taker_fee_bps: i64,
    /// What a maker is paid, in basis points, as a negative number.
    /// `FEHU_MAKER_FEE_BPS`; `0`, the default, pays nothing. Positive values
    /// are refused — see [`crate::trading::Fees`].
    pub maker_fee_bps: i64,
    /// `Idempotency-Key`s remembered, with the response each answered.
    /// `FEHU_COMMAND_LOG`.
    ///
    /// A retry whose key has been evicted is applied again rather than
    /// replayed, so this is how late a retry may arrive and still be free.
    /// It is saved with the market, because a snapshot may fall between any
    /// two commands.
    pub command_log: usize,
    /// What each seeded symbol's merchant is funded with at start-up, in
    /// cents. `FEHU_SEED_MERCHANTS_CENTS`; `0`, the default, seeds none.
    ///
    /// Only for a world that is being warmed up: a restored one already has
    /// the merchants it had. With this set and `FEHU_SYNTHETIC=0` the demo
    /// is an economy in which every fill has a funded counterparty — see
    /// [`App::seed_merchants`].
    pub seed_merchant_cents: i64,
    /// Finished jobs kept, with what each one delivered. `FEHU_JOB_LOG`.
    ///
    /// A *running* job is never dropped whatever this says: it is a promise
    /// the world has already taken payment for. See [`crate::jobs::JobBook`].
    pub job_log: usize,
    /// Mutating requests the server will have in flight at once.
    /// `FEHU_MAX_INFLIGHT`; `0` takes everything and queues it.
    ///
    /// Changes are serialised by the market actor and its mailbox is
    /// unbounded, so past this the honest answer is `503 overloaded` rather
    /// than a place in a queue nothing bounds. See [`crate::limit`].
    pub max_inflight: usize,
    /// Stream connections held open at once. `FEHU_MAX_STREAMS`; `0` for no
    /// bound. Each one holds a broadcast receiver and a task.
    pub max_streams: usize,
    /// Facts kept for the game backend to collect, and how far behind it
    /// may fall before they are lost. `FEHU_OUTBOX`; `0` switches the
    /// outbox off, and nothing is kept for anyone who was not listening.
    ///
    /// See [`crate::outbox`]: this is a bound on memory and therefore on how
    /// long a backend may be away, so it is generous by default.
    pub outbox: usize,
    /// Game event ids remembered, with the reward each one paid.
    /// `FEHU_REWARD_LOG`.
    ///
    /// A reward names the game's own id for what happened, and that id is
    /// what stops a quest being paid twice by a backend that retries with a
    /// fresh `Idempotency-Key`. An id that has aged out of this would be
    /// paid again, so it is how late a duplicate may arrive and still be
    /// caught. See [`crate::rewards`].
    pub reward_log: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            history_days: 365,
            warmup_hours: 72,
            time_scale: 1.0,
            now_ms: None,
            max_bars: 5_000,
            event_log: 500,
            tape_len: 2_000,
            fill_log: 500,
            ledger_log: 500,
            order_log: 2_000,
            starting_cash_cents: 10_000_000,
            // $1 trillion: enough that the demo never notices a limit, and
            // two orders of magnitude under the cap on one wallet, so
            // treasury itself can hold it.
            genesis_cents: 100_000_000_000_000,
            synthetic_float_cents: 50_000_000_000_000,
            synthetic: true,
            issuer_float_cents: 100_000_000_000,
            admin_key: None,
            state_file: None,
            save_secs: 30,
            market_hours: None,
            price_limit_pct: 0.10,
            halt_secs: 300,
            stream_replay: 1_024,
            rate_per_sec: 20.0,
            rate_burst: 40.0,
            tick_cents: 1,
            lot: 1,
            max_symbols: 32,
            taker_fee_bps: 0,
            maker_fee_bps: 0,
            command_log: 10_000,
            seed_merchant_cents: 0,
            job_log: crate::jobs::DEFAULT_JOB_LOG,
            reward_log: crate::rewards::DEFAULT_REWARD_LOG,
            outbox: crate::outbox::DEFAULT_OUTBOX,
            max_inflight: crate::limit::DEFAULT_MAX_INFLIGHT,
            max_streams: crate::limit::DEFAULT_MAX_STREAMS,
        }
    }
}

impl Options {
    /// What the venue charges, with the two restrictions the settlement path
    /// depends on applied: a taker fee is never negative and a maker fee is
    /// never positive.
    #[must_use]
    pub fn fees(&self) -> Fees {
        Fees {
            taker_bps: self.taker_fee_bps.clamp(0, 10_000),
            maker_bps: self.maker_fee_bps.clamp(-10_000, 0),
        }
    }

    /// When a move halts a symbol, and for how long.
    #[must_use]
    pub fn halts(&self) -> HaltPolicy {
        HaltPolicy {
            price_limit_pct: self.price_limit_pct.max(0.0),
            halt_secs: self.halt_secs,
        }
    }

    /// Defaults overridden by any `FEHU_*` variable that parses.
    pub fn from_env() -> Self {
        let d = Self::default();
        Self {
            history_days: env_parse("FEHU_HISTORY_DAYS", d.history_days),
            warmup_hours: env_parse("FEHU_WARMUP_HOURS", d.warmup_hours),
            time_scale: env_parse("FEHU_TIME_SCALE", d.time_scale),
            now_ms: std::env::var("FEHU_NOW_MS")
                .ok()
                .and_then(|v| v.parse().ok()),
            max_bars: env_parse("FEHU_MAX_BARS", d.max_bars),
            event_log: env_parse("FEHU_EVENT_LOG", d.event_log),
            tape_len: env_parse("FEHU_TAPE", d.tape_len),
            fill_log: env_parse("FEHU_FILL_LOG", d.fill_log),
            ledger_log: env_parse("FEHU_LEDGER_LOG", d.ledger_log),
            order_log: env_parse("FEHU_ORDER_LOG", d.order_log),
            starting_cash_cents: env_parse("FEHU_STARTING_CASH_CENTS", d.starting_cash_cents),
            genesis_cents: env_parse("FEHU_GENESIS_CENTS", d.genesis_cents)
                .clamp(0, MAX_BALANCE_CENTS),
            synthetic: env_flag("FEHU_SYNTHETIC", d.synthetic),
            synthetic_float_cents: env_parse("FEHU_SYNTHETIC_FLOAT_CENTS", d.synthetic_float_cents)
                .max(0),
            issuer_float_cents: env_parse("FEHU_ISSUER_FLOAT_CENTS", d.issuer_float_cents).max(0),
            admin_key: std::env::var("FEHU_ADMIN_KEY")
                .ok()
                .map(|k| k.trim().to_string())
                .filter(|k| !k.is_empty()),
            state_file: std::env::var_os("FEHU_STATE_FILE")
                .map(std::path::PathBuf::from)
                .filter(|p| !p.as_os_str().is_empty()),
            save_secs: env_parse("FEHU_SAVE_SECS", d.save_secs).max(1),
            market_hours: std::env::var("FEHU_MARKET_HOURS")
                .ok()
                .as_deref()
                .and_then(parse_market_hours),
            price_limit_pct: env_parse("FEHU_PRICE_LIMIT_PCT", d.price_limit_pct).max(0.0),
            halt_secs: env_parse("FEHU_HALT_SECS", d.halt_secs).max(1),
            stream_replay: env_parse("FEHU_STREAM_REPLAY", d.stream_replay),
            rate_per_sec: env_parse("FEHU_RATE_PER_SEC", d.rate_per_sec).max(0.0),
            rate_burst: env_parse("FEHU_RATE_BURST", d.rate_burst).max(0.0),
            tick_cents: env_parse("FEHU_TICK_CENTS", d.tick_cents).clamp(1, 1_000_000),
            lot: env_parse("FEHU_LOT", d.lot).clamp(1, 1_000_000),
            max_symbols: env_parse("FEHU_MAX_SYMBOLS", d.max_symbols)
                .clamp(1, crate::symbols::MAX_TICKERS),
            taker_fee_bps: env_parse("FEHU_TAKER_FEE_BPS", d.taker_fee_bps),
            maker_fee_bps: env_parse("FEHU_MAKER_FEE_BPS", d.maker_fee_bps),
            command_log: env_parse("FEHU_COMMAND_LOG", d.command_log),
            seed_merchant_cents: env_parse("FEHU_SEED_MERCHANTS_CENTS", d.seed_merchant_cents)
                .max(0),
            job_log: env_parse("FEHU_JOB_LOG", d.job_log),
            reward_log: env_parse("FEHU_REWARD_LOG", d.reward_log),
            outbox: env_parse("FEHU_OUTBOX", d.outbox),
            max_inflight: env_parse("FEHU_MAX_INFLIGHT", d.max_inflight),
            max_streams: env_parse("FEHU_MAX_STREAMS", d.max_streams),
        }
    }
}

/// A trading calendar from `HH:MM-HH:MM` (UTC, Monday to Friday). Anything
/// else — `off`, an empty string, nonsense — means no calendar at all.
fn parse_market_hours(value: &str) -> Option<MarketHours> {
    let (open, close) = value.trim().split_once('-')?;
    let secs = |hhmm: &str| -> Option<u32> {
        let (h, m) = hhmm.trim().split_once(':')?;
        let (h, m): (u32, u32) = (h.trim().parse().ok()?, m.trim().parse().ok()?);
        (h < 24 && m < 60).then_some(h * 3600 + m * 60)
    };
    let (open_secs, close_secs) = (secs(open)?, secs(close)?);
    (open_secs < close_secs).then_some(MarketHours {
        open_secs,
        close_secs,
        ..MarketHours::default()
    })
}

/// A boolean from the environment: `0`, `false`, `no` and `off` are false,
/// `1`, `true`, `yes` and `on` are true, and anything else leaves the
/// default alone.
fn env_flag(name: &str, default: bool) -> bool {
    match std::env::var(name).ok().as_deref().map(str::trim) {
        Some("0" | "false" | "no" | "off" | "FALSE" | "NO" | "OFF") => false,
        Some("1" | "true" | "yes" | "on" | "TRUE" | "YES" | "ON") => true,
        _ => default,
    }
}

fn env_parse<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Trim a user-supplied name, cap it at `max` characters, and treat an
/// empty result as absent.
fn clean(value: Option<String>, max: usize) -> Option<String> {
    value
        .map(|v| v.trim().chars().take(max).collect::<String>())
        .filter(|v| !v.is_empty())
}

/// Milliseconds since the Unix epoch on the wall clock.
pub fn wall_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}
