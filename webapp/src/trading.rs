//! Traders: positions, reservations for resting orders, and the JSON shapes
//! of orders, trades and the book.
//!
//! A trader is a market-facing identity, not a wallet: its cash lives in the
//! [`Account`](crate::account::Account) it trades on, so every method that
//! touches money takes that account and books the movement through it.

use std::collections::{BTreeMap, VecDeque};

use fehu::{
    Level, Order, OrderId, OrderKind, OrderStatus, Owner, Placement, Position, Resting, Side,
    TimeInForce, Trade, TraderId,
};
use serde::{Deserialize, Serialize};

use crate::account::{Account, AccountId, AccountStatus, MoneyError, UserId, notional_cents};

/// One execution from a trader's point of view.
#[derive(Clone, Debug, Serialize)]
pub struct FillRecord {
    pub id: u64,
    pub trader_id: u64,
    pub ts_ms: i64,
    pub symbol: &'static str,
    pub order_id: u64,
    pub side: Side,
    pub qty: u64,
    pub price_cents: i64,
    /// `"maker"` if the trader's order was resting, `"taker"` if it took.
    pub liquidity: &'static str,
    /// `"synthetic"` or `"trader"`.
    pub counterparty: &'static str,
}

/// A market participant. No margin, no shorting: buys need cash in the
/// trader's account and sells need shares, and resting orders reserve both
/// until they fill or cancel.
#[derive(Clone, Debug)]
pub struct Trader {
    pub id: TraderId,
    /// The user this trader belongs to.
    pub user_id: UserId,
    /// The account its cash moves through.
    pub account_id: AccountId,
    pub name: String,
    pub positions: BTreeMap<&'static str, Position>,
    /// Shares committed to resting sell orders, per symbol.
    pub reserved_shares: BTreeMap<&'static str, u64>,
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
        account: &Account,
        symbol: &str,
        side: Side,
        qty: u64,
        cost_cents: i64,
    ) -> Result<(), Refused> {
        match side {
            Side::Buy => account.authorise(cost_cents)?,
            Side::Sell => {
                account.check_tradable()?;
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
        account: &mut Account,
        symbol: &'static str,
        side: Side,
        remaining: u64,
        price_cents: i64,
    ) {
        if remaining == 0 {
            return;
        }
        match side {
            Side::Buy => account.reserve(notional_cents(price_cents, remaining)),
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
        account: &mut Account,
        symbol: &str,
        side: Side,
        qty: u64,
        price_cents: i64,
    ) {
        match side {
            Side::Buy => account.release(notional_cents(price_cents, qty)),
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

    /// Book a trade this trader took part in: cash moves through `account`,
    /// the position and the fill log are updated here. Returns the fill
    /// records created (two for a self-trade).
    pub fn apply_trade(
        &mut self,
        account: &mut Account,
        symbol: &'static str,
        trade: &Trade,
    ) -> Vec<FillRecord> {
        let me = self.owner();
        let mut out = Vec::new();
        let taker_is_trader = matches!(trade.taker.owner, Owner::Trader(_));
        let maker_is_trader = matches!(trade.maker.owner, Owner::Trader(_));
        if trade.taker.owner == me {
            self.book_fill(account, symbol, trade.taker_side, trade, trade.taker.order);
            out.push(self.record(
                symbol,
                trade.taker.order,
                trade.taker_side,
                trade,
                "taker",
                maker_is_trader,
            ));
        }
        if trade.maker.owner == me {
            let side = trade.taker_side.opposite();
            self.release(account, symbol, side, trade.qty, trade.price_cents);
            self.book_fill(account, symbol, side, trade, trade.maker.order);
            out.push(self.record(
                symbol,
                trade.maker.order,
                side,
                trade,
                "maker",
                taker_is_trader,
            ));
        }
        out
    }

    fn book_fill(
        &mut self,
        account: &mut Account,
        symbol: &'static str,
        side: Side,
        trade: &Trade,
        order: OrderId,
    ) {
        let value = notional_cents(trade.price_cents, trade.qty);
        account.settle(side, value, symbol, order.0, trade.ts.0);
        let pos = self.positions.entry(symbol).or_default();
        pos.apply(side, trade.qty, trade.price_cents);
        if pos.qty == 0 && pos.realised_pnl_cents == 0 && pos.cash_cents == 0 {
            self.positions.remove(symbol);
        }
    }

    fn record(
        &mut self,
        symbol: &'static str,
        order: OrderId,
        side: Side,
        trade: &Trade,
        liquidity: &'static str,
        counterparty_is_trader: bool,
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
                "trader"
            } else {
                "synthetic"
            },
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
#[derive(Clone, Debug, Deserialize)]
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
}

/// Longest `client_order_id` the server keeps.
pub const MAX_CLIENT_ORDER_ID: usize = 64;

/// One submitted order for as long as the log keeps it: what was asked for,
/// what has happened to it since, and the id the caller gave it.
///
/// The book itself only knows orders while they rest, so this is where an
/// order that has filled or been cancelled can still be looked up
/// (`GET /api/orders/{id}`, `GET /api/traders/{id}/orders`).
#[derive(Clone, Debug, Serialize)]
pub struct OrderRecord {
    pub order_id: u64,
    /// The caller's own id for this order, if it gave one.
    pub client_order_id: Option<String>,
    pub trader_id: u64,
    pub symbol: &'static str,
    pub side: Side,
    /// `"market"` or `"limit"`.
    pub kind: &'static str,
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
    /// The response the submission returned, replayed verbatim if the same
    /// `client_order_id` arrives again. Not part of the record's own JSON.
    #[serde(skip)]
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
        let (kind, price_cents) = match order.kind {
            OrderKind::Market => ("market", None),
            OrderKind::Limit { price_cents } => ("limit", Some(price_cents)),
        };
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
            accepted: Some(response),
        }
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
        let (kind, price_cents) = match order.kind {
            OrderKind::Market => ("market", None),
            OrderKind::Limit { price_cents } => ("limit", Some(price_cents)),
        };
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
#[derive(Clone, Copy, Debug, Serialize)]
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
    pub remaining: u64,
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
            remaining: r.remaining,
            ts_ms: r.ts.0,
        }
    }
}

/// Response to a submitted order.
#[derive(Clone, Debug, Serialize)]
pub struct OrderResponse {
    pub symbol: &'static str,
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
    use crate::account::AccountId;
    use fehu::{Party, Timestamp};

    const ME: TraderId = TraderId(1);

    /// A trader and the account it trades on, funded with `cash_cents`.
    fn trader(cash_cents: i64) -> (Trader, Account) {
        let account = Account::open(AccountId(1), UserId(1), "main".into(), cash_cents, 100, 0)
            .expect("valid opening balance");
        (
            Trader::new(ME, UserId(1), AccountId(1), "p".into(), 10, 0),
            account,
        )
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
        let (mut t, mut a) = trader(100_000);
        assert!(t.check(&a, "ACME", Side::Buy, 10, 100_001).is_err());
        assert!(t.check(&a, "ACME", Side::Sell, 1, 0).is_err());
        t.check(&a, "ACME", Side::Buy, 10, 50_000).unwrap();
        // Rest a buy of 10 @ 5000.
        t.reserve(&mut a, "ACME", Side::Buy, 10, 5_000);
        assert_eq!(a.available_cents(), 50_000);
        // Half of it fills as maker.
        let fills = t.apply_trade(
            &mut a,
            "ACME",
            &trade(Owner::Synthetic, Owner::Trader(ME), Side::Sell, 5, 5_000),
        );
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].liquidity, "maker");
        assert_eq!(fills[0].side, Side::Buy);
        assert_eq!(a.balance_cents(), 75_000);
        assert_eq!(a.reserved_cents(), 25_000);
        assert_eq!(a.available_cents(), 50_000);
        assert_eq!(t.positions["ACME"].qty, 5);
        // Cancel the rest.
        t.release(&mut a, "ACME", Side::Buy, 5, 5_000);
        assert_eq!(a.reserved_cents(), 0);
        // Sell 3 as taker at 6000.
        assert!(t.check(&a, "ACME", Side::Sell, 6, 0).is_err());
        t.check(&a, "ACME", Side::Sell, 3, 0).unwrap();
        let fills = t.apply_trade(
            &mut a,
            "ACME",
            &trade(Owner::Trader(ME), Owner::Synthetic, Side::Sell, 3, 6_000),
        );
        assert_eq!(fills[0].liquidity, "taker");
        assert_eq!(a.balance_cents(), 93_000);
        assert_eq!(t.positions["ACME"].qty, 2);
        assert_eq!(t.positions["ACME"].realised_pnl_cents, 3_000);
        // A self-trade books both sides and nets to nothing.
        t.reserve(&mut a, "ACME", Side::Sell, 2, 7_000);
        assert_eq!(t.free_shares("ACME"), 0);
        let fills = t.apply_trade(
            &mut a,
            "ACME",
            &trade(Owner::Trader(ME), Owner::Trader(ME), Side::Buy, 2, 7_000),
        );
        assert_eq!(fills.len(), 2);
        assert_eq!(a.balance_cents(), 93_000);
        assert_eq!(t.positions["ACME"].qty, 2);
        assert_eq!(t.free_shares("ACME"), 2);
        // Every movement is on the ledger, and the account stays valid.
        assert!(a.is_valid());
        assert_eq!(a.ledger(100).len(), 5, "open + four settlements");
    }

    #[test]
    fn a_sell_can_never_exceed_what_is_held() {
        let (mut t, mut a) = trader(1_000_000);
        // Buy 100 as taker.
        t.apply_trade(
            &mut a,
            "ACME",
            &trade(Owner::Trader(ME), Owner::Synthetic, Side::Buy, 100, 5_000),
        );
        assert_eq!(t.held_shares("ACME"), 100);
        assert_eq!(t.free_shares("ACME"), 100);
        assert!(t.check(&a, "ACME", Side::Sell, 101, 0).is_err());
        t.check(&a, "ACME", Side::Sell, 100, 0).unwrap();
        // 60 of them rest in a sell, so only 40 are still sellable.
        t.reserve(&mut a, "ACME", Side::Sell, 60, 6_000);
        assert_eq!(t.held_shares("ACME"), 100, "reserving sells nothing");
        assert_eq!(t.free_shares("ACME"), 40);
        assert_eq!(
            t.check(&a, "ACME", Side::Sell, 41, 0).unwrap_err(),
            Refused::InsufficientShares {
                needed: 41,
                available: 40,
            }
        );
        t.check(&a, "ACME", Side::Sell, 40, 0).unwrap();
        // Another symbol is a separate pot, empty here.
        assert_eq!(t.free_shares("NBLA"), 0);
        assert!(t.check(&a, "NBLA", Side::Sell, 1, 0).is_err());
    }

    #[test]
    fn holdings_add_up_across_traders() {
        let (mut one, mut a) = trader(1_000_000);
        let mut two = Trader::new(TraderId(2), UserId(1), AccountId(1), "two".into(), 10, 0);
        for t in [&mut one, &mut two] {
            t.apply_trade(
                &mut a,
                "ACME",
                &trade(Owner::Trader(t.id), Owner::Synthetic, Side::Buy, 50, 4_000),
            );
        }
        two.reserve(&mut a, "ACME", Side::Sell, 20, 5_000);

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
        let (t, mut a) = trader(100_000);
        a.set_status(AccountStatus::Frozen).unwrap();
        assert!(matches!(
            t.check(&a, "ACME", Side::Buy, 1, 1).unwrap_err(),
            Refused::Account(MoneyError::Status { .. })
        ));
        assert!(matches!(
            t.check(&a, "ACME", Side::Sell, 1, 0).unwrap_err(),
            Refused::Account(MoneyError::Status { .. })
        ));
    }
}
