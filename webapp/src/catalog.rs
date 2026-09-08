//! The catalogue: which goods the world will make, and for how much.
//!
//! A good is listed holding nothing ([`AssetKind::Good`](crate::symbol::AssetKind)),
//! and nothing but a command brings a unit of one into existence. The
//! catalogue is the first such command: an operator writes down what a unit
//! of `ORE` costs, and a player who pays that price gets a unit that did not
//! exist before. It is the mine, the forge and the shop front the world has
//! before it has any of those things.
//!
//! Two rules keep that from being a hole in the economy.
//!
//! **The currency is not created, only moved.** A purchase debits the
//! buyer's wallet and credits the good's issuer wallet, as one balanced
//! transaction; the supply is untouched. What a purchase creates is *units*,
//! which are counted separately and by a different rule — see
//! [`AssetKind`](crate::symbol::AssetKind).
//!
//! **A line may be finite.** [`CatalogItem::available`] is how many units
//! this line may still issue; `None` is a seam that never runs out, which is
//! what a demo wants and a scarce world does not. Either way it is one
//! number, in one place, that an operator sets and an audit can read.
//!
//! Consuming is the other half: units leave the world for good, the count of
//! what has been consumed goes up, and no currency moves at all — a thing
//! that has been used up is not a thing that has been sold.
//!
//! # Where this is going
//!
//! The catalogue is the world selling to a player. An NPC merchant is a
//! *trader* selling to a player: it holds inventory, quotes it on the same
//! book through the same orders, and runs out. When merchants arrive the
//! catalogue stays as what stocks them.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::save::Symbol;

/// Lines the catalogue will hold. Each one names a listed good, and the
/// symbol table is capped well below this, so it is a bound on nonsense
/// rather than on play.
pub const MAX_CATALOG: usize = 256;

/// The longest note a catalogue line carries.
pub const MAX_NOTE_LEN: usize = 140;

/// One line of the catalogue: a good, its price, and how much of it is left
/// to make.
#[derive(Clone, Debug, Serialize, Deserialize)]
// The ticker is registered on the way in rather than borrowed from the
// input, so the derive needs no `'de: 'static`.
#[serde(bound(deserialize = ""))]
pub struct CatalogItem {
    /// The good this line sells. Always a listed [`Good`](crate::symbol::AssetKind::Good).
    #[serde(with = "crate::save::symbol")]
    pub symbol: Symbol,
    /// What one unit costs, in cents. Always positive: a free good would be
    /// a way to make units out of nothing.
    pub price_cents: i64,
    /// Units this line may still issue, or `null` for a seam that never runs
    /// out.
    pub available: Option<u64>,
    /// Units it has issued since the line was written.
    pub issued: u64,
    /// What the line is, for whoever reads the catalogue.
    pub note: Option<String>,
}

impl CatalogItem {
    /// Whether this line can still issue `qty` units.
    #[must_use]
    pub fn can_issue(&self, qty: u64) -> bool {
        self.available.is_none_or(|left| left >= qty)
    }

    /// Record `qty` units issued against this line.
    fn take(&mut self, qty: u64) {
        if let Some(left) = self.available.as_mut() {
            *left = left.saturating_sub(qty);
        }
        self.issued = self.issued.saturating_add(qty);
    }
}

/// Every line, by ticker.
///
/// Ordered by ticker, like every other symbol-keyed map in the server, so
/// what the catalogue endpoint returns is stable between calls and between
/// runs.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(bound(deserialize = ""), transparent)]
pub struct Catalog {
    #[serde(with = "crate::save::symbol_map")]
    items: BTreeMap<Symbol, CatalogItem>,
}

/// Why a catalogue change or a purchase was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CatalogError {
    /// The catalogue is full ([`MAX_CATALOG`]).
    Full,
    /// No line for this good.
    Unknown(String),
    /// The line cannot make that many more units.
    Exhausted {
        /// What was asked for.
        wanted: u64,
        /// What the line has left.
        available: u64,
    },
    /// A price of zero or less.
    Price,
}

impl std::fmt::Display for CatalogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full => write!(f, "the catalogue holds {MAX_CATALOG} lines already"),
            Self::Unknown(sym) => write!(f, "{sym} is not in the catalogue"),
            Self::Exhausted { wanted, available } => write!(
                f,
                "the catalogue has {available} unit(s) of this left, not {wanted}"
            ),
            Self::Price => write!(f, "a catalogue price must be at least one cent"),
        }
    }
}

impl std::error::Error for CatalogError {}

impl Catalog {
    /// Write or replace the line for `symbol`.
    ///
    /// Replacing keeps the count of what the line has already issued: it is
    /// a record of what happened, not a budget an operator can reset by
    /// rewriting the price.
    ///
    /// # Errors
    /// [`CatalogError::Price`] for a price below a cent, [`CatalogError::Full`]
    /// once the catalogue is at [`MAX_CATALOG`] lines.
    pub fn set(
        &mut self,
        symbol: Symbol,
        price_cents: i64,
        available: Option<u64>,
        note: Option<String>,
    ) -> Result<&CatalogItem, CatalogError> {
        if price_cents <= 0 {
            return Err(CatalogError::Price);
        }
        let issued = self.items.get(symbol).map_or(0, |item| item.issued);
        if !self.items.contains_key(symbol) && self.items.len() >= MAX_CATALOG {
            return Err(CatalogError::Full);
        }
        self.items.insert(
            symbol,
            CatalogItem {
                symbol,
                price_cents,
                available,
                issued,
                note,
            },
        );
        Ok(&self.items[symbol])
    }

    /// Take the line for `symbol` out of the catalogue. Nothing already
    /// issued is affected: the units are in the world and stay there.
    pub fn remove(&mut self, symbol: &str) -> Option<CatalogItem> {
        self.items.remove(symbol)
    }

    /// The line for `symbol`, if there is one.
    #[must_use]
    pub fn get(&self, symbol: &str) -> Option<&CatalogItem> {
        self.items.get(symbol)
    }

    /// Check that `qty` units of `symbol` can be issued, and say at what
    /// price, without changing anything.
    ///
    /// # Errors
    /// [`CatalogError::Unknown`] with no such line, [`CatalogError::Exhausted`]
    /// when the line has less left than was asked for.
    pub fn quote(&self, symbol: &str, qty: u64) -> Result<i64, CatalogError> {
        let item = self
            .items
            .get(symbol)
            .ok_or_else(|| CatalogError::Unknown(symbol.to_owned()))?;
        if !item.can_issue(qty) {
            return Err(CatalogError::Exhausted {
                wanted: qty,
                available: item.available.unwrap_or(u64::MAX),
            });
        }
        Ok(item.price_cents)
    }

    /// Record `qty` units issued off the line for `symbol`. The caller has
    /// already checked it with [`Catalog::quote`] and posted the money.
    pub fn issue(&mut self, symbol: &str, qty: u64) {
        if let Some(item) = self.items.get_mut(symbol) {
            item.take(qty);
        }
    }

    /// Every line, ordered by ticker.
    pub fn items(&self) -> impl Iterator<Item = &CatalogItem> {
        self.items.values()
    }

    /// Lines written.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Nothing is on sale.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// Why a purchase or a consumption was refused.
///
/// Every one of these is checked before anything moves: a refused purchase
/// leaves no units issued and no currency moved, and a refused consumption
/// leaves the units where they were.
#[derive(Clone, Debug)]
pub enum GoodsError {
    /// No such trader.
    UnknownTrader(u64),
    /// No such symbol, or its actor is gone.
    Unknown(String),
    /// The symbol is a stock: shares are floated and sold, not made to order
    /// and used up.
    NotAGood(String),
    /// Nothing was asked for, or so much that the arithmetic will not hold
    /// it.
    Quantity(String),
    /// The catalogue would not sell it.
    Catalog(CatalogError),
    /// The ledger would not move the money.
    Money(crate::account::MoneyError),
    /// The trader does not hold that many units free of reservations.
    InsufficientUnits {
        /// What was asked for.
        needed: u64,
        /// What is held and unreserved.
        available: u64,
    },
}

impl From<CatalogError> for GoodsError {
    fn from(e: CatalogError) -> Self {
        Self::Catalog(e)
    }
}

impl From<crate::account::MoneyError> for GoodsError {
    fn from(e: crate::account::MoneyError) -> Self {
        Self::Money(e)
    }
}

impl From<fehu::ledger::LedgerError> for GoodsError {
    fn from(e: fehu::ledger::LedgerError) -> Self {
        Self::Money(crate::account::MoneyError::Ledger(e))
    }
}

impl From<crate::actor::Gone> for GoodsError {
    fn from(_: crate::actor::Gone) -> Self {
        Self::Unknown("the symbol is no longer listed".into())
    }
}

impl std::fmt::Display for GoodsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownTrader(id) => write!(f, "no trader {id}"),
            Self::Unknown(sym) => write!(f, "{sym} is not listed"),
            Self::NotAGood(sym) => write!(
                f,
                "{sym} is a stock: its shares are bought on the book, not made to order"
            ),
            Self::Quantity(why) => write!(f, "{why}"),
            Self::Catalog(e) => write!(f, "{e}"),
            Self::Money(e) => write!(f, "{e}"),
            Self::InsufficientUnits { needed, available } => write!(
                f,
                "insufficient units: need {needed}, hold {available} free of reservations"
            ),
        }
    }
}

impl std::error::Error for GoodsError {}

/// `GET /api/catalog`: what the world will make and what it charges.
#[derive(Debug, Serialize)]
pub struct CatalogResponse {
    pub items: Vec<CatalogItem>,
}

/// What a purchase did: the money, the units, and where both ended up.
#[derive(Clone, Debug, Serialize)]
pub struct PurchaseReceipt {
    pub trader_id: u64,
    pub symbol: Symbol,
    /// Units bought.
    pub qty: u64,
    pub unit_price_cents: i64,
    /// `qty × unit_price_cents`, the amount that moved.
    pub total_cents: i64,
    /// The balanced transaction that moved it.
    pub tx_id: u64,
    /// What the trader holds of the good now.
    pub position_qty: i64,
    /// Units of the good in existence now.
    pub units_outstanding: u64,
    /// What the line has left to make, or `null` for a seam.
    pub available: Option<u64>,
}

/// What consuming did. No currency moves, so there is no transaction to
/// name: units left the world and that is the whole of it.
#[derive(Clone, Debug, Serialize)]
pub struct ConsumeReceipt {
    pub trader_id: u64,
    pub symbol: Symbol,
    /// Units destroyed.
    pub qty: u64,
    /// What the trader holds of the good now.
    pub position_qty: i64,
    /// Units of the good in existence now.
    pub units_outstanding: u64,
}
