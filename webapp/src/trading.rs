//! Traders: cash, positions, reservations for resting orders, and the JSON
//! shapes of orders, trades and the book.

use std::collections::{BTreeMap, VecDeque};

use fehu::{
    Level, OrderId, OrderKind, OrderStatus, Owner, Placement, Position, Resting, Side, TimeInForce,
    Trade, TraderId,
};
use serde::{Deserialize, Serialize};

/// Largest cash balance a trader can be created with, in cents.
pub const MAX_CASH_CENTS: i64 = 1_000_000_000_000_000;

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

/// A trader's account. No margin, no shorting: buys need cash and sells
/// need shares, and resting orders reserve them until they fill or cancel.
#[derive(Clone, Debug)]
pub struct Trader {
    pub id: TraderId,
    pub name: String,
    pub cash_cents: i64,
    /// Cash committed to resting buy orders.
    pub reserved_cents: i64,
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refused {
    /// Not enough free cash for the buy.
    InsufficientCash { needed: i64, available: i64 },
    /// Not enough free shares for the sell.
    InsufficientShares { needed: u64, available: u64 },
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InsufficientCash { needed, available } => write!(
                f,
                "insufficient cash: need {:.2}, have {:.2} free",
                *needed as f64 / 100.0,
                *available as f64 / 100.0
            ),
            Self::InsufficientShares { needed, available } => {
                write!(
                    f,
                    "insufficient shares: need {needed}, have {available} free"
                )
            }
        }
    }
}

impl Trader {
    pub fn new(id: TraderId, name: String, cash_cents: i64, fill_cap: usize, now_ms: i64) -> Self {
        Self {
            id,
            name,
            cash_cents,
            reserved_cents: 0,
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

    /// Cash not committed to resting buys.
    pub fn free_cash_cents(&self) -> i64 {
        self.cash_cents.saturating_sub(self.reserved_cents)
    }

    /// Shares of `symbol` not committed to resting sells.
    pub fn free_shares(&self, symbol: &str) -> u64 {
        let held = self
            .positions
            .get(symbol)
            .map_or(0, |p| u64::try_from(p.qty).unwrap_or(0));
        held.saturating_sub(self.reserved_shares.get(symbol).copied().unwrap_or(0))
    }

    /// Check that an order can be afforded. `cost_cents` is the worst-case
    /// cash a buy could consume (limit: `qty × price`; market: the preview).
    pub fn check(
        &self,
        symbol: &str,
        side: Side,
        qty: u64,
        cost_cents: i64,
    ) -> Result<(), Refused> {
        match side {
            Side::Buy => {
                let available = self.free_cash_cents();
                if cost_cents > available {
                    return Err(Refused::InsufficientCash {
                        needed: cost_cents,
                        available,
                    });
                }
            }
            Side::Sell => {
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

    /// Reserve for the resting remainder of a just-submitted order.
    pub fn reserve(&mut self, symbol: &'static str, side: Side, remaining: u64, price_cents: i64) {
        if remaining == 0 {
            return;
        }
        match side {
            Side::Buy => {
                self.reserved_cents = self
                    .reserved_cents
                    .saturating_add(cents(price_cents, remaining));
            }
            Side::Sell => {
                let r = self.reserved_shares.entry(symbol).or_insert(0);
                *r = r.saturating_add(remaining);
            }
        }
    }

    /// Release the reservation of `qty` shares at `price` (a fill of a
    /// resting order, or a cancel).
    pub fn release(&mut self, symbol: &str, side: Side, qty: u64, price_cents: i64) {
        match side {
            Side::Buy => {
                self.reserved_cents = (self.reserved_cents - cents(price_cents, qty)).max(0);
            }
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

    /// Book a trade this trader took part in. Returns the fill records
    /// created (two for a self-trade).
    pub fn apply_trade(&mut self, symbol: &'static str, trade: &Trade) -> Vec<FillRecord> {
        let me = self.owner();
        let mut out = Vec::new();
        let taker_is_trader = matches!(trade.taker.owner, Owner::Trader(_));
        let maker_is_trader = matches!(trade.maker.owner, Owner::Trader(_));
        if trade.taker.owner == me {
            self.book_fill(symbol, trade.taker_side, trade);
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
            self.release(symbol, side, trade.qty, trade.price_cents);
            self.book_fill(symbol, side, trade);
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

    fn book_fill(&mut self, symbol: &'static str, side: Side, trade: &Trade) {
        let value = cents(trade.price_cents, trade.qty);
        self.cash_cents = match side {
            Side::Buy => self.cash_cents.saturating_sub(value),
            Side::Sell => self.cash_cents.saturating_add(value),
        };
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

fn cents(price_cents: i64, qty: u64) -> i64 {
    i64::try_from(i128::from(price_cents) * i128::from(qty)).unwrap_or(i64::MAX)
}

// ---------------------------------------------------------------------------
// JSON shapes

/// Body of `POST /api/traders`.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct CreateTraderRequest {
    pub name: Option<String>,
    /// Starting cash; defaults to the server's `starting_cash_cents`.
    pub cash_cents: Option<i64>,
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
    pub reserved_shares: u64,
}

/// `GET /api/traders/{id}`.
#[derive(Clone, Debug, Serialize)]
pub struct PortfolioDto {
    pub id: u64,
    pub name: String,
    pub created_at_ms: i64,
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
}

/// `GET /api/traders` row.
#[derive(Clone, Debug, Serialize)]
pub struct TraderSummary {
    pub id: u64,
    pub name: String,
    pub cash_cents: i64,
    pub equity_cents: i64,
    pub positions: usize,
    pub open_orders: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use fehu::{Party, Timestamp};

    const ME: TraderId = TraderId(1);

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
        let mut t = Trader::new(ME, "p".into(), 100_000, 10, 0);
        assert!(t.check("ACME", Side::Buy, 10, 100_001).is_err());
        assert!(t.check("ACME", Side::Sell, 1, 0).is_err());
        t.check("ACME", Side::Buy, 10, 50_000).unwrap();
        // Rest a buy of 10 @ 5000.
        t.reserve("ACME", Side::Buy, 10, 5_000);
        assert_eq!(t.free_cash_cents(), 50_000);
        // Half of it fills as maker.
        let fills = t.apply_trade(
            "ACME",
            &trade(Owner::Synthetic, Owner::Trader(ME), Side::Sell, 5, 5_000),
        );
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].liquidity, "maker");
        assert_eq!(fills[0].side, Side::Buy);
        assert_eq!(t.cash_cents, 75_000);
        assert_eq!(t.reserved_cents, 25_000);
        assert_eq!(t.free_cash_cents(), 50_000);
        assert_eq!(t.positions["ACME"].qty, 5);
        // Cancel the rest.
        t.release("ACME", Side::Buy, 5, 5_000);
        assert_eq!(t.reserved_cents, 0);
        // Sell 3 as taker at 6000.
        assert!(t.check("ACME", Side::Sell, 6, 0).is_err());
        t.check("ACME", Side::Sell, 3, 0).unwrap();
        let fills = t.apply_trade(
            "ACME",
            &trade(Owner::Trader(ME), Owner::Synthetic, Side::Sell, 3, 6_000),
        );
        assert_eq!(fills[0].liquidity, "taker");
        assert_eq!(t.cash_cents, 93_000);
        assert_eq!(t.positions["ACME"].qty, 2);
        assert_eq!(t.positions["ACME"].realised_pnl_cents, 3_000);
        // A self-trade books both sides and nets to nothing.
        t.reserve("ACME", Side::Sell, 2, 7_000);
        assert_eq!(t.free_shares("ACME"), 0);
        let fills = t.apply_trade(
            "ACME",
            &trade(Owner::Trader(ME), Owner::Trader(ME), Side::Buy, 2, 7_000),
        );
        assert_eq!(fills.len(), 2);
        assert_eq!(t.cash_cents, 93_000);
        assert_eq!(t.positions["ACME"].qty, 2);
        assert_eq!(t.free_shares("ACME"), 2);
    }
}
