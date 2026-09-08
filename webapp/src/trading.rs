//! Traders: positions, reservations for resting orders, and the JSON shapes
//! of orders, trades and the book.
//!
//! A trader is a market-facing identity, not a wallet: its cash lives in the
//! [`Account`](crate::account::Account) it trades on, and that account's
//! money lives in the market's [`Ledger`]. So every method that touches money
//! takes both — the ledger it must post to, and the account whose history it
//! writes a row in.
//!
//! Nothing here posts a settlement. A fill moves currency between two parties
//! and the venue, and that is *one* balanced transaction; posting it once per
//! party would move it twice. [`Market::book`](crate::market::Market::book)
//! posts it, then hands each side its transaction id and asks the trader to
//! record what its share of it was — which is what [`Trader::apply_trade`]
//! does.

use std::collections::{BTreeMap, VecDeque};

use fehu::{
    Level, Order, OrderId, OrderKind, OrderStatus, Owner, Placement, Position, Resting, Side,
    TimeInForce, Trade, TraderId,
};
use serde::{Deserialize, Serialize};

use fehu::ledger::Ledger;

use crate::account::{
    Account, AccountId, AccountStatus, LedgerKind, MoneyError, UserId, notional_cents,
};
use crate::save::Symbol;

/// Which side of the book a fill came from: `maker` if the trader's order
/// was resting when it traded, `taker` if it took what was there.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Liquidity {
    Maker,
    Taker,
}

/// Who was on the other side of a fill: another trader, or the synthetic
/// liquidity the exchange quotes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Counterparty {
    Synthetic,
    Trader,
}

/// How an order was priced.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderStyle {
    Market,
    Limit,
}

/// What the venue charges for a fill, in basis points of its notional.
///
/// Maker-taker pricing, with one deliberate restriction: the taker pays and
/// the maker does not. A maker *rebate* is allowed — it only ever credits an
/// account — but a maker *fee* is not, and the reason is reservations. A
/// taker's cash is checked with the fee included in the same moment the
/// order is submitted and filled, so it can always be paid. A maker's fill
/// happens later, against a reservation made when the order was accepted;
/// charging it would mean reserving the fee too and releasing exactly the
/// same amount back across every partial fill and cancel, which rounding
/// makes a piece of work of its own. Until that is done, a positive
/// `maker_bps` is refused rather than half-implemented.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fees {
    /// Charged to whoever took liquidity. Never negative.
    pub taker_bps: i64,
    /// Paid to whoever provided it, as a negative number. Never positive.
    pub maker_bps: i64,
}

impl Fees {
    /// The fee on a fill of `notional_cents`, signed the way the ledger is:
    /// negative takes money out of the account, positive puts it in.
    ///
    /// Truncated towards zero, so the venue never rounds a fee up and a fill
    /// too small to owe a whole cent owes nothing.
    #[must_use]
    pub fn on(&self, liquidity: Liquidity, notional_cents: i64) -> i64 {
        let bps = match liquidity {
            Liquidity::Taker => self.taker_bps.max(0),
            Liquidity::Maker => self.maker_bps.min(0),
        };
        if bps == 0 {
            return 0;
        }
        // A taker's `bps` is positive and the money leaves, so the ledger
        // amount is the negative of it; a maker's is negative and the rebate
        // arrives.
        let charge = i128::from(notional_cents) * i128::from(bps) / 10_000;
        i64::try_from(-charge).unwrap_or(i64::MIN)
    }

    /// The worst a taker could be charged for a fill of `notional_cents`,
    /// as a positive number, for the cash check made before submitting.
    #[must_use]
    pub fn taker_cost(&self, notional_cents: i64) -> i64 {
        -self.on(Liquidity::Taker, notional_cents)
    }
}

/// What the venue actually charged and paid on one fill.
///
/// Not the same thing as [`Fees`], which says what it *would* charge: a
/// rebate is capped at what the venue can pay, so the two differ whenever
/// the venue is asked for more than it holds. This is the number that was
/// posted, and so the number a trader is told.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SettledFees {
    /// Charged to whoever took liquidity, signed the way the ledger is.
    pub taker_cents: i64,
    /// Paid to whoever provided it, signed the way the ledger is.
    pub maker_cents: i64,
}

impl SettledFees {
    /// This side's share of it.
    pub fn on(self, liquidity: Liquidity) -> i64 {
        match liquidity {
            Liquidity::Taker => self.taker_cents,
            Liquidity::Maker => self.maker_cents,
        }
    }
}

/// One execution from a trader's point of view.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FillRecord {
    pub id: u64,
    pub trader_id: u64,
    pub ts_ms: i64,
    #[serde(with = "crate::save::symbol")]
    pub symbol: Symbol,
    pub order_id: u64,
    pub side: Side,
    pub qty: u64,
    pub price_cents: i64,
    /// `maker` if the trader's order was resting, `taker` if it took.
    pub liquidity: Liquidity,
    pub counterparty: Counterparty,
    /// The venue's fee, signed the way the ledger is: negative was taken out
    /// of the account, positive was a rebate paid in. Its own ledger entry,
    /// never folded into the price.
    #[serde(default)]
    pub fee_cents: i64,
}

/// A market participant. No margin, no shorting: buys need cash in the
/// trader's account and sells need shares, and resting orders reserve both
/// until they fill or cancel.
///
/// It serialises whole, private fields included: the save file has to carry
/// the positions and the fill log, not a view of them.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Trader {
    pub id: TraderId,
    /// The user this trader belongs to.
    pub user_id: UserId,
    /// The account its cash moves through.
    pub account_id: AccountId,
    pub name: String,
    #[serde(with = "crate::save::symbol_map")]
    pub positions: BTreeMap<Symbol, Position>,
    /// Shares committed to resting sell orders, per symbol.
    #[serde(with = "crate::save::symbol_map")]
    pub reserved_shares: BTreeMap<Symbol, u64>,
    /// Most recent fills, oldest first.
    pub fills: VecDeque<FillRecord>,
    fill_cap: usize,
    next_fill_id: u64,
    pub created_at_ms: i64,
}

/// Why an order was refused before reaching the exchange.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// The account would not fund the buy: frozen, closed or short of cash.
    Account(MoneyError),
    /// Not enough free shares for the sell.
    InsufficientShares { needed: u64, available: u64 },
    /// The buy asked for more shares than the symbol has left: every other
    /// share of it is already held by a trader or bid for by a resting order.
    SupplyExhausted { needed: u64, available: u64 },
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Account(e) => write!(f, "{e}"),
            Self::InsufficientShares { needed, available } => {
                write!(
                    f,
                    "insufficient shares: need {needed}, have {available} free"
                )
            }
            Self::SupplyExhausted { needed, available } => {
                write!(
                    f,
                    "not enough shares left: want {needed}, {available} of the \
                     outstanding shares are unheld"
                )
            }
        }
    }
}

impl From<MoneyError> for Refused {
    fn from(e: MoneyError) -> Self {
        Self::Account(e)
    }
}

impl Trader {
    pub fn new(
        id: TraderId,
        user_id: UserId,
        account_id: AccountId,
        name: String,
        fill_cap: usize,
        now_ms: i64,
    ) -> Self {
        Self {
            id,
            user_id,
            account_id,
            name,
            positions: BTreeMap::new(),
            reserved_shares: BTreeMap::new(),
            fills: VecDeque::new(),
            fill_cap: fill_cap.max(1),
            next_fill_id: 1,
            created_at_ms: now_ms,
        }
    }

    pub fn owner(&self) -> Owner {
        Owner::Trader(self.id)
    }

    /// Shares of `symbol` the trader owns. Positions never go short — a sell
    /// is refused unless the shares are already there — so a negative one
    /// would be a broken invariant and counts as nothing.
    pub fn held_shares(&self, symbol: &str) -> u64 {
        self.positions
            .get(symbol)
            .map_or(0, |p| u64::try_from(p.qty).unwrap_or(0))
    }

    /// Shares of `symbol` the trader can still sell: what it holds, less what
    /// its resting sells have already promised away.
    pub fn free_shares(&self, symbol: &str) -> u64 {
        self.held_shares(symbol)
            .saturating_sub(self.reserved_shares.get(symbol).copied().unwrap_or(0))
    }

    /// Validate an order against the trader's account before it reaches the
    /// exchange. `cost_cents` is the worst-case cash a buy could consume
    /// (limit: `qty × price`; market: the preview). A sell needs no cash but
    /// needs an account that is allowed to trade and, above all, the shares:
    /// there is no shorting, so `qty` may never exceed [`Trader::free_shares`]
    /// — the position less whatever earlier resting sells already promised.
    pub fn check(
        &self,
        ledger: &Ledger,
        account: &Account,
        symbol: &str,
        side: Side,
        qty: u64,
        cost_cents: i64,
    ) -> Result<(), Refused> {
        match side {
            Side::Buy => account.authorise(ledger, cost_cents)?,
            Side::Sell => {
                account.check_tradable(ledger)?;
                let available = self.free_shares(symbol);
                if qty > available {
                    return Err(Refused::InsufficientShares {
                        needed: qty,
                        available,
                    });
                }
            }
        }
        Ok(())
    }

    /// Reserve for the resting remainder of a just-submitted order: cash on
    /// the account for a buy, shares here for a sell.
    pub fn reserve(
        &mut self,
        ledger: &mut Ledger,
        account: &Account,
        symbol: &'static str,
        side: Side,
        remaining: u64,
        price_cents: i64,
    ) {
        if remaining == 0 {
            return;
        }
        match side {
            Side::Buy => account.reserve(ledger, notional_cents(price_cents, remaining)),
            Side::Sell => {
                let r = self.reserved_shares.entry(symbol).or_insert(0);
                *r = r.saturating_add(remaining);
            }
        }
    }

    /// Release the reservation of `qty` shares at `price` (a fill of a
    /// resting order, or a cancel).
    pub fn release(
        &mut self,
        ledger: &mut Ledger,
        account: &Account,
        symbol: &str,
        side: Side,
        qty: u64,
        price_cents: i64,
    ) {
        match side {
            Side::Buy => account.release(ledger, notional_cents(price_cents, qty)),
            Side::Sell => {
                if let Some(r) = self.reserved_shares.get_mut(symbol) {
                    *r = r.saturating_sub(qty);
                    if *r == 0 {
                        self.reserved_shares.remove(symbol);
                    }
                }
            }
        }
    }

    /// Put `qty` units of `symbol` into the position at `price_cents` each:
    /// a purchase from the catalogue, which is not a fill and never touches
    /// a book.
    ///
    /// The money has already moved — `tx_id` names the balanced transaction
    /// that moved it — and the units have already been issued against the
    /// symbol. This is the holder's own view of both.
    #[allow(clippy::too_many_arguments)]
    pub fn acquire(
        &mut self,
        ledger: &Ledger,
        account: &mut Account,
        symbol: &'static str,
        qty: u64,
        price_cents: i64,
        tx_id: u64,
        now_ms: i64,
        memo: Option<String>,
    ) -> i64 {
        let value = notional_cents(price_cents, qty);
        account.record(
            ledger,
            LedgerKind::Purchase,
            tx_id,
            -value,
            now_ms,
            Some(symbol),
            None,
            memo,
        );
        let pos = self.positions.entry(symbol).or_default();
        pos.apply(Side::Buy, qty, price_cents);
        pos.qty
    }

    /// Take `qty` units of `symbol` out of the position for good.
    ///
    /// Consuming is a sale at nothing: the units leave and nothing comes
    /// back, so what they cost becomes a realised loss and the account's
    /// cash is untouched. Nothing here can fail — the caller has checked
    /// that the units are held and unreserved — because the world's count of
    /// what has been consumed has already gone up.
    pub fn destroy(&mut self, symbol: &str, qty: u64) -> i64 {
        let Some(pos) = self.positions.get_mut(symbol) else {
            return 0;
        };
        pos.apply(Side::Sell, qty, 0);
        let left = pos.qty;
        if pos.qty == 0 && pos.realised_pnl_cents == 0 && pos.cash_cents == 0 {
            self.positions.remove(symbol);
        }
        left
    }

    /// Record a trade this trader took part in: the position, the reservation
    /// its resting order held, and the rows in the account's history.
    ///
    /// The currency has already moved, and so has the reservation that was
    /// holding it back — `tx_id` names the one balanced transaction that
    /// moved it, which
    /// [`Market::book`](crate::market::Market::book) posted for both sides
    /// and the venue at once. What is left is each side's own view of it, and
    /// that is what this writes. Returns the fill records created (two for a
    /// self-trade).
    pub fn apply_trade(
        &mut self,
        ledger: &mut Ledger,
        account: &mut Account,
        symbol: &'static str,
        trade: &Trade,
        fees: SettledFees,
        tx_id: u64,
    ) -> Vec<FillRecord> {
        let me = self.owner();
        let mut out = Vec::new();
        let taker_is_trader = matches!(trade.taker.owner, Owner::Trader(_));
        let maker_is_trader = matches!(trade.maker.owner, Owner::Trader(_));
        if trade.taker.owner == me {
            let fee = self.book_fill(
                ledger,
                account,
                symbol,
                trade.taker_side,
                trade,
                trade.taker.order,
                fees,
                Liquidity::Taker,
                tx_id,
            );
            out.push(self.record(
                symbol,
                trade.taker.order,
                trade.taker_side,
                trade,
                Liquidity::Taker,
                maker_is_trader,
                fee,
            ));
        }
        if trade.maker.owner == me {
            // The reservation behind this fill was released before the
            // settlement was posted — see `Market::book` for why it has to
            // be, and why it cannot be done here.
            let side = trade.taker_side.opposite();
            let fee = self.book_fill(
                ledger,
                account,
                symbol,
                side,
                trade,
                trade.maker.order,
                fees,
                Liquidity::Maker,
                tx_id,
            );
            out.push(self.record(
                symbol,
                trade.maker.order,
                side,
                trade,
                Liquidity::Maker,
                taker_is_trader,
                fee,
            ));
        }
        out
    }

    /// Write one side of a settled trade into the account's history and the
    /// trader's position: the trade, then the fee, as two rows of the one
    /// transaction. Returns the fee, signed the way the ledger is.
    #[allow(clippy::too_many_arguments)]
    fn book_fill(
        &mut self,
        ledger: &Ledger,
        account: &mut Account,
        symbol: &'static str,
        side: Side,
        trade: &Trade,
        order: OrderId,
        fees: SettledFees,
        liquidity: Liquidity,
        tx_id: u64,
    ) -> i64 {
        let value = notional_cents(trade.price_cents, trade.qty);
        let fee = fees.on(liquidity);
        let (kind, signed) = match side {
            Side::Buy => (LedgerKind::Buy, -value),
            Side::Sell => (LedgerKind::Sell, value),
        };
        // The tape stays the price and the ledger stays the money: a fee is
        // never folded into what the fill was worth. Both rows belong to the
        // one transaction, so they are written together and the running
        // balance is laid out across them.
        let mut rows = vec![(kind, signed)];
        if fee != 0 {
            rows.push((LedgerKind::Fee, fee));
        }
        account.record_split(
            ledger,
            tx_id,
            trade.ts.0,
            Some(symbol),
            Some(order.0),
            None,
            &rows,
        );
        let pos = self.positions.entry(symbol).or_default();
        pos.apply(side, trade.qty, trade.price_cents);
        if pos.qty == 0 && pos.realised_pnl_cents == 0 && pos.cash_cents == 0 {
            self.positions.remove(symbol);
        }
        fee
    }

    #[allow(clippy::too_many_arguments)]
    fn record(
        &mut self,
        symbol: &'static str,
        order: OrderId,
        side: Side,
        trade: &Trade,
        liquidity: Liquidity,
        counterparty_is_trader: bool,
        fee_cents: i64,
    ) -> FillRecord {
        let rec = FillRecord {
            id: self.next_fill_id,
            trader_id: self.id.0,
            ts_ms: trade.ts.0,
            symbol,
            order_id: order.0,
            side,
            qty: trade.qty,
            price_cents: trade.price_cents,
            liquidity,
            counterparty: if counterparty_is_trader {
                Counterparty::Trader
            } else {
                Counterparty::Synthetic
            },
            fee_cents,
        };
        self.next_fill_id += 1;
        if self.fills.len() >= self.fill_cap {
            self.fills.pop_front();
        }
        self.fills.push_back(rec.clone());
        rec
    }
}

// ---------------------------------------------------------------------------
// JSON shapes

/// Body of `POST /api/traders`. With no `user_id` a user is created for the
/// trader; with no `account_id` an account is opened for it and funded with
/// `cash_cents`.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct CreateTraderRequest {
    pub name: Option<String>,
    /// Starting cash; defaults to the server's `starting_cash_cents`.
    pub cash_cents: Option<i64>,
    /// Existing user to attach the trader to.
    pub user_id: Option<u64>,
    /// Existing account to trade on. It must belong to `user_id`, and
    /// `cash_cents` is then ignored — deposit into the account instead.
    pub account_id: Option<u64>,
    /// Email for the user created alongside the trader.
    pub email: Option<String>,
}

/// Body of `POST /api/symbols/{symbol}/orders`.
// Serialised as well as deserialised: this is what the journal writes
// down, so a replayed request is the request that was accepted.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OrderRequest {
    pub trader_id: u64,
    pub side: Side,
    pub qty: u64,
    #[serde(flatten)]
    pub kind: OrderKind,
    #[serde(default)]
    pub tif: TimeInForce,
    /// Caller-chosen id, unique per trader, that makes the submission
    /// idempotent: sending the same order twice — a retry after a timeout,
    /// say — places it once. See [`OrderRecord`].
    pub client_order_id: Option<String>,
    /// The order must rest: if it would trade on arrival it is refused
    /// instead. Only meaningful for a `gtc` limit order.
    #[serde(default)]
    pub post_only: bool,
    /// Show only this much at a time, keeping the rest back and posting the
    /// next slice — at the back of the queue for its price — as each one
    /// fills. Only a `gtc` limit order can hide anything.
    pub display_qty: Option<u64>,
    /// Simulated time at which the order is withdrawn if it is still
    /// resting. Absent leaves it resting until it fills or is cancelled.
    pub expires_at_ms: Option<i64>,
    /// A day order: withdrawn at the close of the session it was sent in.
    /// Needs a trading calendar; without one there is no close to expire at,
    /// and the order is refused rather than quietly living forever.
    #[serde(default)]
    pub day: bool,
}

/// Body of `PATCH /api/symbols/{symbol}/orders/{order_id}`: a new price, a
/// new quantity, or both.
///
/// An amendment is a cancel and a fresh order, so the amended order goes to
/// the back of the queue at its price — the same as anywhere else that does
/// not have a true in-place amend.
// Serialised as well as deserialised: this is what the journal writes
// down, so a replayed request is the request that was accepted.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AmendRequest {
    pub trader_id: u64,
    /// New limit price; unchanged if absent.
    pub price_cents: Option<i64>,
    /// New quantity; what is still resting if absent.
    pub qty: Option<u64>,
    /// A `client_order_id` for the replacement order.
    pub client_order_id: Option<String>,
    /// The replacement must rest.
    #[serde(default)]
    pub post_only: bool,
}

/// Response to an amendment: the order that was withdrawn, and the one that
/// took its place.
#[derive(Clone, Debug, Serialize)]
pub struct AmendResponse {
    /// The order that was cancelled to make way.
    pub replaced_order_id: u64,
    /// Shares of the replaced order that had already filled.
    pub replaced_filled: u64,
    #[serde(flatten)]
    pub order: OrderResponse,
}

impl OrderStyle {
    /// How `order` is priced, and at what price if it says.
    fn of(order: &Order) -> (Self, Option<i64>) {
        match order.kind {
            OrderKind::Market => (Self::Market, None),
            OrderKind::Limit { price_cents } => (Self::Limit, Some(price_cents)),
        }
    }
}

/// Longest `client_order_id` the server keeps.
pub const MAX_CLIENT_ORDER_ID: usize = 64;

/// Stops one trader may hold at once, across every symbol. A stop costs
/// nothing to keep — it reserves neither cash nor shares — so without a cap
/// one client could fill the market's memory with triggers that never fire.
pub const MAX_STOPS_PER_TRADER: usize = 100;

/// A trigger held aside until the price touches it.
///
/// A stop is not an order: it rests nowhere, reserves nothing and takes no
/// queue position, and the book has never heard of it. When the last price
/// reaches `stop_price_cents` the engine submits it like any other order —
/// which is also when the account is checked for the second time, because
/// the money may have moved since the stop was accepted.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StopOrder {
    pub stop_id: u64,
    pub trader_id: u64,
    #[serde(with = "crate::save::symbol")]
    pub symbol: Symbol,
    pub side: Side,
    pub qty: u64,
    /// A buy fires at or above this price, a sell at or below.
    pub stop_price_cents: i64,
    /// The limit the order carries when it fires. `null` fires a market
    /// order — a plain stop rather than a stop-limit.
    pub limit_price_cents: Option<i64>,
    /// The time in force of the order it fires, not of the trigger: a stop
    /// is held until it fires or is cancelled, whatever this says.
    pub tif: TimeInForce,
    /// Handed to the order the trigger places, so a stop is as idempotent as
    /// anything else the trader sends.
    pub client_order_id: Option<String>,
    pub created_at_ms: i64,
}

impl StopOrder {
    /// The order this fires.
    #[must_use]
    pub fn order(&self) -> Order {
        Order {
            owner: Owner::Trader(TraderId(self.trader_id)),
            side: self.side,
            kind: match self.limit_price_cents {
                Some(price_cents) => OrderKind::Limit { price_cents },
                None => OrderKind::Market,
            },
            tif: self.tif,
            qty: self.qty,
        }
    }

    /// Whether a last price of `price_cents` reaches the trigger.
    #[must_use]
    pub fn triggered_by(&self, price_cents: i64) -> bool {
        match self.side {
            Side::Buy => price_cents >= self.stop_price_cents,
            Side::Sell => price_cents <= self.stop_price_cents,
        }
    }

    /// Worst-case cash the order it fires could consume, for the check made
    /// when the stop is accepted. A stop-market has no limit to work from,
    /// so the trigger price stands in for one.
    #[must_use]
    pub fn cost_cents(&self) -> i64 {
        let price = self.limit_price_cents.unwrap_or(self.stop_price_cents);
        i64::try_from(i128::from(price) * i128::from(self.qty)).unwrap_or(i64::MAX)
    }
}

/// Body of `POST /api/symbols/{symbol}/stops`.
// Serialised as well as deserialised: this is what the journal writes
// down, so a replayed request is the request that was accepted.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StopRequest {
    pub trader_id: u64,
    pub side: Side,
    pub qty: u64,
    /// The price that fires it: above the market for a buy, below for a sell.
    pub stop_price_cents: i64,
    /// The limit the fired order carries. Absent makes it a stop-market.
    pub limit_price_cents: Option<i64>,
    #[serde(default)]
    pub tif: TimeInForce,
    /// Passed on to the order the trigger places.
    pub client_order_id: Option<String>,
}

/// One submitted order for as long as the log keeps it: what was asked for,
/// what has happened to it since, and the id the caller gave it.
///
/// The book itself only knows orders while they rest, so this is where an
/// order that has filled or been cancelled can still be looked up
/// (`GET /api/orders/{id}`, `GET /api/traders/{id}/orders`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OrderRecord {
    pub order_id: u64,
    /// The caller's own id for this order, if it gave one.
    pub client_order_id: Option<String>,
    pub trader_id: u64,
    #[serde(with = "crate::save::symbol")]
    pub symbol: Symbol,
    pub side: Side,
    pub kind: OrderStyle,
    /// The limit price; `null` for a market order.
    pub price_cents: Option<i64>,
    pub tif: TimeInForce,
    /// Shares asked for.
    pub qty: u64,
    /// Shares executed so far.
    pub filled: u64,
    /// Shares not executed: resting, or withdrawn when cancelled.
    pub remaining: u64,
    /// `resting` while it is live, then `filled` or `cancelled`.
    pub status: OrderStatus,
    /// Cash moved by the fills so far.
    pub notional_cents: i64,
    /// `notional_cents / filled`.
    pub avg_price_cents: Option<f64>,
    pub submitted_at_ms: i64,
    pub updated_at_ms: i64,
    /// Simulated time this order is withdrawn at if it is still resting.
    /// The engine sweeps for these at the end of every step.
    #[serde(default)]
    pub expires_at_ms: Option<i64>,
    /// The response the submission returned, replayed verbatim if the same
    /// `client_order_id` arrives again. Not part of the record's own JSON,
    /// but it is saved, so a retry across a restart still replays.
    #[serde(skip_serializing, default)]
    pub accepted: Option<OrderResponse>,
}

impl OrderRecord {
    /// Record a just-accepted order and the response it produced.
    pub fn new(
        client_order_id: Option<String>,
        trader: TraderId,
        symbol: &'static str,
        order: &Order,
        placement: &Placement,
        response: OrderResponse,
        ts_ms: i64,
    ) -> Self {
        let (kind, price_cents) = OrderStyle::of(order);
        Self {
            order_id: placement.id.0,
            client_order_id,
            trader_id: trader.0,
            symbol,
            side: order.side,
            kind,
            price_cents,
            tif: order.tif,
            qty: order.qty,
            filled: placement.filled,
            remaining: placement.remaining,
            status: placement.status,
            notional_cents: placement.notional_cents(),
            avg_price_cents: placement.avg_price_cents(),
            submitted_at_ms: ts_ms,
            updated_at_ms: ts_ms,
            expires_at_ms: None,
            accepted: Some(response),
        }
    }

    /// Withdraw this order at `at_ms` if it is still resting then.
    #[must_use]
    pub fn expiring_at(mut self, at_ms: Option<i64>) -> Self {
        self.expires_at_ms = at_ms;
        self
    }

    /// This order is live and its time is up at `now_ms`.
    #[must_use]
    pub fn has_expired(&self, now_ms: i64) -> bool {
        self.is_live() && self.expires_at_ms.is_some_and(|at| at <= now_ms)
    }

    /// The order is neither filled nor cancelled: the book still has it.
    pub fn is_live(&self) -> bool {
        self.status == OrderStatus::Resting
    }

    /// Book an execution against a resting order. Fills of the submission
    /// itself are already in the placement, so only later ones land here.
    pub fn fill(&mut self, qty: u64, price_cents: i64, ts_ms: i64) {
        if !self.is_live() {
            return;
        }
        self.filled = self.filled.saturating_add(qty);
        self.remaining = self.remaining.saturating_sub(qty);
        self.notional_cents = self
            .notional_cents
            .saturating_add(notional_cents(price_cents, qty));
        self.avg_price_cents =
            (self.filled > 0).then(|| self.notional_cents as f64 / self.filled as f64);
        if self.remaining == 0 {
            self.status = OrderStatus::Filled;
        }
        self.updated_at_ms = ts_ms;
    }

    /// The remainder was withdrawn from the book.
    pub fn cancel(&mut self, remaining: u64, ts_ms: i64) {
        if !self.is_live() {
            return;
        }
        self.remaining = remaining;
        self.status = OrderStatus::Cancelled;
        self.updated_at_ms = ts_ms;
    }

    /// The submission this order was accepted with matches `other` — the same
    /// order, sent twice.
    pub fn matches(&self, order: &Order, symbol: &str) -> bool {
        let (kind, price_cents) = OrderStyle::of(order);
        self.symbol == symbol
            && self.trader_id == order.owner.trader().map_or(0, |t| t.0)
            && self.side == order.side
            && self.kind == kind
            && self.price_cents == price_cents
            && self.tif == order.tif
            && self.qty == order.qty
    }
}

/// One trade as it appears on the tape and in order responses.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct TradeDto {
    pub ts_ms: i64,
    pub price_cents: i64,
    pub qty: u64,
    pub taker_side: Side,
    pub taker_order_id: u64,
    pub maker_order_id: u64,
    /// Trader ids, `null` for synthetic liquidity.
    pub taker_trader: Option<u64>,
    pub maker_trader: Option<u64>,
    /// The maker was hidden liquidity: the visible book was exhausted.
    pub hidden: bool,
}

impl From<&Trade> for TradeDto {
    fn from(t: &Trade) -> Self {
        Self {
            ts_ms: t.ts.0,
            price_cents: t.price_cents,
            qty: t.qty,
            taker_side: t.taker_side,
            taker_order_id: t.taker.order.0,
            maker_order_id: t.maker.order.0,
            taker_trader: t.taker.owner.trader().map(|t| t.0),
            maker_trader: t.maker.owner.trader().map(|t| t.0),
            hidden: t.maker.order == OrderId::HIDDEN,
        }
    }
}

/// A resting order as reported to its owner.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct OpenOrderDto {
    pub symbol: &'static str,
    pub order_id: u64,
    pub trader_id: u64,
    pub side: Side,
    pub price_cents: i64,
    pub qty: u64,
    /// Everything still open: what is on show plus, for an iceberg, what is
    /// not.
    pub remaining: u64,
    /// The slice an iceberg shows at a time; `null` for an ordinary order.
    pub display_qty: Option<u64>,
    /// What is on show right now. Equal to `remaining` unless this is an
    /// iceberg with something still held back.
    pub shown_qty: u64,
    pub ts_ms: i64,
}

impl OpenOrderDto {
    pub fn from_resting(symbol: &'static str, r: &Resting) -> Self {
        Self {
            symbol,
            order_id: r.id.0,
            trader_id: r.owner.trader().map_or(0, |t| t.0),
            side: r.side,
            price_cents: r.price_cents,
            qty: r.qty,
            remaining: r.outstanding(),
            display_qty: (r.display > 0).then_some(r.display),
            shown_qty: r.remaining,
            ts_ms: r.ts.0,
        }
    }
}

/// Response to a submitted order.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OrderResponse {
    #[serde(with = "crate::save::symbol")]
    pub symbol: Symbol,
    pub trader_id: u64,
    pub order_id: u64,
    pub side: Side,
    pub qty: u64,
    pub filled: u64,
    pub remaining: u64,
    pub status: OrderStatus,
    pub avg_price_cents: Option<f64>,
    pub notional_cents: i64,
    /// Price impact the exchange will apply on the next tick, as a log move
    /// of the reference price.
    pub trades: Vec<TradeDto>,
}

impl OrderResponse {
    pub fn new(
        symbol: &'static str,
        trader: TraderId,
        side: Side,
        qty: u64,
        p: &Placement,
    ) -> Self {
        Self {
            symbol,
            trader_id: trader.0,
            order_id: p.id.0,
            side,
            qty,
            filled: p.filled,
            remaining: p.remaining,
            status: p.status,
            avg_price_cents: p.avg_price_cents(),
            notional_cents: p.notional_cents(),
            trades: p.trades.iter().map(TradeDto::from).collect(),
        }
    }
}

/// Top of the book on both sides.
#[derive(Clone, Debug, Serialize)]
pub struct BookDto {
    pub bids: Vec<Level>,
    pub asks: Vec<Level>,
}

/// One position in a portfolio.
#[derive(Clone, Debug, Serialize)]
pub struct PositionDto {
    pub symbol: &'static str,
    pub qty: i64,
    pub avg_cost_cents: Option<f64>,
    pub mark_cents: i64,
    pub market_value_cents: i64,
    pub unrealised_pnl_cents: i64,
    pub realised_pnl_cents: i64,
    /// Shares promised to resting sell orders.
    pub reserved_shares: u64,
    /// `qty − reserved_shares`: the most this trader may still sell.
    pub free_shares: u64,
}

/// What one user owns of one symbol: their traders' positions in it, added
/// up. A user can never sell more of a symbol than `free_shares` here, and
/// no single trader more than its own share of it.
#[derive(Clone, Debug, Serialize)]
pub struct HoldingDto {
    pub symbol: &'static str,
    /// Shares owned.
    pub qty: u64,
    /// Of those, promised to resting sell orders.
    pub reserved_shares: u64,
    /// `qty − reserved_shares`: what the user can still sell.
    pub free_shares: u64,
    /// Cost basis of the open position, in cents.
    pub cost_cents: i64,
    /// `cost_cents / qty`.
    pub avg_cost_cents: Option<f64>,
    /// The symbol's reference price.
    pub mark_cents: i64,
    pub market_value_cents: i64,
    pub unrealised_pnl_cents: i64,
    pub realised_pnl_cents: i64,
    /// The user's traders holding this symbol, by id.
    pub traders: Vec<u64>,
}

impl HoldingDto {
    /// An empty holding of `symbol`, to be filled with [`HoldingDto::add`]
    /// and closed off with [`HoldingDto::mark`].
    pub fn empty(symbol: &'static str) -> Self {
        Self {
            symbol,
            qty: 0,
            reserved_shares: 0,
            free_shares: 0,
            cost_cents: 0,
            avg_cost_cents: None,
            mark_cents: 0,
            market_value_cents: 0,
            unrealised_pnl_cents: 0,
            realised_pnl_cents: 0,
            traders: Vec::new(),
        }
    }

    /// Add what one trader holds of the symbol.
    pub fn add(&mut self, trader: &Trader) {
        let held = trader.held_shares(self.symbol);
        let Some(position) = trader.positions.get(self.symbol) else {
            return;
        };
        self.qty = self.qty.saturating_add(held);
        self.reserved_shares = self
            .reserved_shares
            .saturating_add(held.saturating_sub(trader.free_shares(self.symbol)));
        self.cost_cents = self.cost_cents.saturating_add(position.cost_cents);
        self.realised_pnl_cents = self
            .realised_pnl_cents
            .saturating_add(position.realised_pnl_cents);
        self.traders.push(trader.id.0);
    }

    /// Value the holding at `mark_cents` once every trader has been added.
    pub fn mark(&mut self, mark_cents: i64) {
        self.free_shares = self.qty.saturating_sub(self.reserved_shares);
        self.mark_cents = mark_cents;
        self.market_value_cents = notional_cents(mark_cents, self.qty);
        self.unrealised_pnl_cents = self.market_value_cents.saturating_sub(self.cost_cents);
        self.avg_cost_cents = (self.qty > 0).then(|| self.cost_cents as f64 / self.qty as f64);
    }
}

/// `GET /api/users/{id}/holdings`: every share the user owns.
#[derive(Clone, Debug, Serialize)]
pub struct UserHoldingsResponse {
    pub user_id: u64,
    /// Shares owned across every symbol and every trader of the user.
    pub shares_owned: u64,
    /// Of those, promised to resting sell orders.
    pub reserved_shares: u64,
    /// What the user could sell right now.
    pub free_shares: u64,
    /// The holdings at the reference prices.
    pub market_value_cents: i64,
    /// One entry per symbol the user holds, by ticker.
    pub holdings: Vec<HoldingDto>,
}

impl UserHoldingsResponse {
    /// Total up `holdings` for one user.
    pub fn new(user_id: u64, holdings: Vec<HoldingDto>) -> Self {
        let mut out = Self {
            user_id,
            shares_owned: 0,
            reserved_shares: 0,
            free_shares: 0,
            market_value_cents: 0,
            holdings,
        };
        for h in &out.holdings {
            out.shares_owned = out.shares_owned.saturating_add(h.qty);
            out.reserved_shares = out.reserved_shares.saturating_add(h.reserved_shares);
            out.free_shares = out.free_shares.saturating_add(h.free_shares);
            out.market_value_cents = out.market_value_cents.saturating_add(h.market_value_cents);
        }
        out
    }
}

/// One trader's stake in a symbol, for `GET /api/symbols/{sym}/shares`.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct HolderDto {
    pub trader_id: u64,
    pub user_id: u64,
    pub qty: u64,
    pub reserved_shares: u64,
    pub free_shares: u64,
}

impl HolderDto {
    /// `trader`'s stake in `symbol`, or `None` if it holds none of it.
    pub fn new(trader: &Trader, symbol: &str) -> Option<Self> {
        let qty = trader.held_shares(symbol);
        if qty == 0 {
            return None;
        }
        let free = trader.free_shares(symbol);
        Some(Self {
            trader_id: trader.id.0,
            user_id: trader.user_id.0,
            qty,
            reserved_shares: qty.saturating_sub(free),
            free_shares: free,
        })
    }
}

/// `GET /api/traders/{id}`.
#[derive(Clone, Debug, Serialize)]
pub struct PortfolioDto {
    pub id: u64,
    pub user_id: u64,
    pub account_id: u64,
    pub name: String,
    pub created_at_ms: i64,
    pub account_status: AccountStatus,
    /// The account's balance.
    pub cash_cents: i64,
    pub reserved_cents: i64,
    pub free_cash_cents: i64,
    /// Cash plus positions at the reference price.
    pub equity_cents: i64,
    pub realised_pnl_cents: i64,
    pub unrealised_pnl_cents: i64,
    pub positions: Vec<PositionDto>,
    pub open_orders: Vec<OpenOrderDto>,
    /// Triggers waiting for a price, oldest first. Not orders: they rest
    /// nowhere and reserve nothing until they fire.
    pub stops: Vec<StopOrder>,
    /// Newest first.
    pub fills: Vec<FillRecord>,
    /// The key that proves a request speaks for the trader's user, shown **once**:
    /// in the response that created them, and `null` everywhere after. Send
    /// it as `Authorization: Bearer <key>`.
    pub api_key: Option<String>,
}

/// `GET /api/traders` row.
#[derive(Clone, Debug, Serialize)]
pub struct TraderSummary {
    pub id: u64,
    pub user_id: u64,
    pub account_id: u64,
    pub name: String,
    pub account_status: AccountStatus,
    pub cash_cents: i64,
    pub equity_cents: i64,
    pub positions: usize,
    pub open_orders: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{AccountId, settlement_draft};
    use fehu::ledger::{Draft, Reason, WalletId, WalletKind};
    use fehu::{Party, Timestamp};

    const ME: TraderId = TraderId(1);

    /// The ledger these tests settle in, and the wallets a fill needs
    /// besides the trader's: the venue that takes the fee, and the stand-in
    /// for liquidity nobody funded.
    struct World {
        ledger: Ledger,
        venue: WalletId,
        synthetic: WalletId,
    }

    /// A world, a trader, and the account it trades on, funded with
    /// `cash_cents` out of treasury.
    fn trader(cash_cents: i64) -> (World, Trader, Account) {
        let mut ledger = Ledger::new();
        let treasury = ledger.open(WalletKind::Treasury);
        let venue = ledger.open(WalletKind::Venue);
        let synthetic = ledger.open(WalletKind::Synthetic);
        ledger
            .mint(treasury, 1_000_000_000, Reason::Genesis)
            .unwrap();
        let wallet = ledger.open(WalletKind::Player);
        let mut account = Account::open(AccountId(1), UserId(1), "main".into(), wallet, 100, 0);
        if cash_cents > 0 {
            let tx = ledger
                .post(
                    Draft::new(Reason::Faucet)
                        .debit(treasury, cash_cents)
                        .credit(wallet, cash_cents),
                )
                .unwrap();
            account.record(
                &ledger,
                LedgerKind::Deposit,
                tx.id,
                cash_cents,
                0,
                None,
                None,
                None,
            );
        }
        let world = World {
            ledger,
            venue,
            synthetic,
        };
        (
            world,
            Trader::new(ME, UserId(1), AccountId(1), "p".into(), 10, 0),
            account,
        )
    }

    /// Settle a trade the way [`crate::market::Market::book`] does — one
    /// balanced transaction for both sides and the venue — then record the
    /// trader's view of it.
    ///
    /// The venue charges nothing here, which is what most of these check;
    /// fees have their own tests over the HTTP surface.
    fn apply(
        w: &mut World,
        trader: &mut Trader,
        account: &mut Account,
        symbol: &'static str,
        trade: &Trade,
    ) -> Vec<FillRecord> {
        let fees = SettledFees::default();
        let wallet_of = |owner: Owner| match owner {
            Owner::Trader(_) => account.wallet,
            _ => w.synthetic,
        };
        // The maker's reservation goes back before the settlement is posted,
        // exactly as `Market::book` does it: the cash a resting buy holds is
        // the cash that pays for its own fill.
        if trade.maker.owner == trader.owner() {
            let side = trade.taker_side.opposite();
            trader.release(
                &mut w.ledger,
                account,
                symbol,
                side,
                trade.qty,
                trade.price_cents,
            );
        }
        let value = notional_cents(trade.price_cents, trade.qty);
        let tx = w
            .ledger
            .post(settlement_draft(
                wallet_of(trade.taker.owner),
                wallet_of(trade.maker.owner),
                w.venue,
                trade.taker_side,
                value,
                0,
                trade.ts.0,
            ))
            .expect("the fixture funds every fill it books");
        trader.apply_trade(&mut w.ledger, account, symbol, trade, fees, tx.id)
    }

    fn trade(taker: Owner, maker: Owner, side: Side, qty: u64, price: i64) -> Trade {
        Trade {
            ts: Timestamp(0),
            price_cents: price,
            qty,
            taker_side: side,
            taker: Party {
                order: OrderId(10),
                owner: taker,
            },
            maker: Party {
                order: OrderId(11),
                owner: maker,
            },
        }
    }

    #[test]
    fn reservations_and_fills() {
        let (mut w, mut t, mut a) = trader(100_000);
        assert!(
            t.check(&w.ledger, &a, "ACME", Side::Buy, 10, 100_001)
                .is_err()
        );
        assert!(t.check(&w.ledger, &a, "ACME", Side::Sell, 1, 0).is_err());
        t.check(&w.ledger, &a, "ACME", Side::Buy, 10, 50_000)
            .unwrap();
        // Rest a buy of 10 @ 5000.
        t.reserve(&mut w.ledger, &a, "ACME", Side::Buy, 10, 5_000);
        assert_eq!(a.available_cents(&w.ledger), 50_000);
        // Half of it fills as maker.
        let fills = apply(
            &mut w,
            &mut t,
            &mut a,
            "ACME",
            &trade(Owner::Synthetic, Owner::Trader(ME), Side::Sell, 5, 5_000),
        );
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].liquidity, Liquidity::Maker);
        assert_eq!(fills[0].side, Side::Buy);
        assert_eq!(a.balance_cents(&w.ledger), 75_000);
        assert_eq!(a.reserved_cents(&w.ledger), 25_000);
        assert_eq!(a.available_cents(&w.ledger), 50_000);
        assert_eq!(t.positions["ACME"].qty, 5);
        // Cancel the rest.
        t.release(&mut w.ledger, &a, "ACME", Side::Buy, 5, 5_000);
        assert_eq!(a.reserved_cents(&w.ledger), 0);
        // Sell 3 as taker at 6000.
        assert!(t.check(&w.ledger, &a, "ACME", Side::Sell, 6, 0).is_err());
        t.check(&w.ledger, &a, "ACME", Side::Sell, 3, 0).unwrap();
        let fills = apply(
            &mut w,
            &mut t,
            &mut a,
            "ACME",
            &trade(Owner::Trader(ME), Owner::Synthetic, Side::Sell, 3, 6_000),
        );
        assert_eq!(fills[0].liquidity, Liquidity::Taker);
        assert_eq!(a.balance_cents(&w.ledger), 93_000);
        assert_eq!(t.positions["ACME"].qty, 2);
        assert_eq!(t.positions["ACME"].realised_pnl_cents, 3_000);
        // A self-trade books both sides and nets to nothing.
        t.reserve(&mut w.ledger, &a, "ACME", Side::Sell, 2, 7_000);
        assert_eq!(t.free_shares("ACME"), 0);
        let fills = apply(
            &mut w,
            &mut t,
            &mut a,
            "ACME",
            &trade(Owner::Trader(ME), Owner::Trader(ME), Side::Buy, 2, 7_000),
        );
        assert_eq!(fills.len(), 2);
        assert_eq!(a.balance_cents(&w.ledger), 93_000);
        assert_eq!(t.positions["ACME"].qty, 2);
        assert_eq!(t.free_shares("ACME"), 2);
        // Every movement is on the ledger, and the account stays valid.
        assert!(a.is_valid(&w.ledger));
        assert_eq!(
            a.ledger(100).len(),
            6,
            "open, the faucet that funded it, and four settlements"
        );
    }

    #[test]
    fn a_sell_can_never_exceed_what_is_held() {
        let (mut w, mut t, mut a) = trader(1_000_000);
        // Buy 100 as taker.
        apply(
            &mut w,
            &mut t,
            &mut a,
            "ACME",
            &trade(Owner::Trader(ME), Owner::Synthetic, Side::Buy, 100, 5_000),
        );
        assert_eq!(t.held_shares("ACME"), 100);
        assert_eq!(t.free_shares("ACME"), 100);
        assert!(t.check(&w.ledger, &a, "ACME", Side::Sell, 101, 0).is_err());
        t.check(&w.ledger, &a, "ACME", Side::Sell, 100, 0).unwrap();
        // 60 of them rest in a sell, so only 40 are still sellable.
        t.reserve(&mut w.ledger, &a, "ACME", Side::Sell, 60, 6_000);
        assert_eq!(t.held_shares("ACME"), 100, "reserving sells nothing");
        assert_eq!(t.free_shares("ACME"), 40);
        assert_eq!(
            t.check(&w.ledger, &a, "ACME", Side::Sell, 41, 0)
                .unwrap_err(),
            Refused::InsufficientShares {
                needed: 41,
                available: 40,
            }
        );
        t.check(&w.ledger, &a, "ACME", Side::Sell, 40, 0).unwrap();
        // Another symbol is a separate pot, empty here.
        assert_eq!(t.free_shares("NBLA"), 0);
        assert!(t.check(&w.ledger, &a, "NBLA", Side::Sell, 1, 0).is_err());
    }

    #[test]
    fn holdings_add_up_across_traders() {
        let (mut w, mut one, mut a) = trader(1_000_000);
        let mut two = Trader::new(TraderId(2), UserId(1), AccountId(1), "two".into(), 10, 0);
        for t in [&mut one, &mut two] {
            apply(
                &mut w,
                t,
                &mut a,
                "ACME",
                &trade(Owner::Trader(t.id), Owner::Synthetic, Side::Buy, 50, 4_000),
            );
        }
        two.reserve(&mut w.ledger, &a, "ACME", Side::Sell, 20, 5_000);

        let mut h = HoldingDto::empty("ACME");
        h.add(&one);
        h.add(&two);
        h.mark(6_000);
        assert_eq!(h.qty, 100);
        assert_eq!(h.reserved_shares, 20);
        assert_eq!(h.free_shares, 80, "what the user could sell right now");
        assert_eq!(h.cost_cents, 400_000);
        assert_eq!(h.avg_cost_cents, Some(4_000.0));
        assert_eq!(h.market_value_cents, 600_000);
        assert_eq!(h.unrealised_pnl_cents, 200_000);
        assert_eq!(h.traders, vec![1, 2]);

        let totals = UserHoldingsResponse::new(1, vec![h]);
        assert_eq!(totals.shares_owned, 100);
        assert_eq!(totals.free_shares, 80);
        assert_eq!(totals.market_value_cents, 600_000);

        // A trader holding nothing of the symbol is not a holder of it.
        assert!(HolderDto::new(&one, "NBLA").is_none());
        let holder = HolderDto::new(&two, "ACME").expect("two holds ACME");
        assert_eq!(
            (holder.qty, holder.reserved_shares, holder.free_shares),
            (50, 20, 30)
        );
    }

    #[test]
    fn a_frozen_account_cannot_trade_at_all() {
        let (w, t, a) = trader(100_000);
        let mut w = w;
        w.ledger.freeze(a.wallet).unwrap();
        assert!(matches!(
            t.check(&w.ledger, &a, "ACME", Side::Buy, 1, 1).unwrap_err(),
            Refused::Account(e) if e.is_forbidden()
        ));
        assert!(matches!(
            t.check(&w.ledger, &a, "ACME", Side::Sell, 1, 0).unwrap_err(),
            Refused::Account(e) if e.is_forbidden()
        ));
    }
}
