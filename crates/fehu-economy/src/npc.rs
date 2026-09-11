//! NPC traders: the world holding inventory and quoting it.
//!
//! Before this module there were two kinds of liquidity in a book: a
//! player's resting order, and the simulator's synthetic ladder. The second
//! is not liquidity at all in an economy where currency and units are
//! counted — nobody funds it, nobody holds what it sells, and every fill
//! against it is currency put into a player's hands out of nowhere (which is
//! precisely what [`WalletKind::Synthetic`](fehu::ledger::WalletKind::Synthetic)
//! measures rather than hides).
//!
//! An NPC is the funded answer. It is a [`Trader`](crate::trading::Trader)
//! like any other: it has an account, a wallet of its own kind, positions
//! and share reservations, and it places ordinary limit orders through the
//! same command path a player's order takes. Nothing about a fill against it
//! is special, which is the point — the settlement, the fees, the audit and
//! the reconciliation all already work.
//!
//! What is different is *why* it places them. A player decides; an NPC
//! follows a [`Policy`], read once per engine step from the reference price
//! the simulator produced. So the simulator survives as what it always was —
//! a noisy reference — and game events still move prices, but through a
//! decision by somebody who paid for the inventory rather than through a
//! print nobody paid for.
//!
//! # Running out is the feature
//!
//! An NPC quotes what it can fund and what it holds, and no more. Its bid
//! disappears when its wallet is empty and its ask when its inventory is,
//! because those quotes are refused by exactly the checks that refuse a
//! player's. Scarcity is then visible in the book instead of being a rule
//! written down somewhere.
//!
//! # Producers
//!
//! A merchant sells what it was given. A *producer* makes what it sells: an
//! NPC with a [`Production`] policy watches its free stock of the symbol it
//! quotes and, when that falls to the restock line, buys the recipe's
//! inputs from the catalogue with its own cash and starts a job through
//! the same command path a player's job takes. The outputs land in its
//! inventory on the step that delivers them and are quoted like anything
//! else it holds. Its takings go to the catalogue's issuers and the venue
//! along the way, where a sweep brings them home — so the loop the
//! economy plan draws, seam to furnace to market to treasury, closes with
//! nothing minted.
//!
//! A producer that cannot afford its inputs, or whose recipe is gone, or
//! that already has as many jobs running as its policy allows, does
//! nothing that step and looks again on the next. Running out is still the
//! feature.
//!
//! # Determinism
//!
//! Quoting happens inside the engine step, which is a journaled command
//! ([`Command::Step`](crate::journal::Command::Step)). It reads the market
//! and the symbol and nothing else — no clock, no randomness — so a replayed
//! step re-quotes exactly as the original did.

use serde::{Deserialize, Serialize};

use fehu::TraderId;

use crate::account::{AccountId, UserId};
use crate::save::Symbol;

/// NPCs one world may have. Each one is re-quoted on every engine step, so
/// this is a bound on work per tick as much as on names.
pub const MAX_NPCS: usize = 64;

/// Basis points in one.
pub const BPS: i64 = 10_000;

/// How an NPC quotes: a ladder of its own, in basis points of the reference
/// price, and how far that price must move before it is worth re-drawing.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Policy {
    /// Half the spread, in basis points of the reference. The best bid sits
    /// this far below it and the best ask this far above.
    pub half_spread_bps: u32,
    /// Levels a side.
    pub levels: u32,
    /// Distance between levels, in basis points of the reference.
    pub level_step_bps: u32,
    /// Units quoted at each level.
    pub size: u64,
    /// How far the reference must move, in basis points, before the quotes
    /// are withdrawn and redrawn.
    ///
    /// Re-quoting every tick would put an order on the book four times a
    /// second per side per level and take it off again, which is a great
    /// deal of work to end up where it started. A band means the NPC leaves
    /// its quotes where they are until the market has actually moved.
    pub requote_bps: u32,
}

impl Default for Policy {
    /// A market maker with a 25 bp half-spread, five levels 25 bp apart,
    /// re-drawn when the reference has moved half a spread.
    fn default() -> Self {
        Self {
            half_spread_bps: 25,
            levels: 5,
            level_step_bps: 25,
            size: 250,
            requote_bps: 12,
        }
    }
}

impl Policy {
    /// Check every field against its documented range.
    ///
    /// # Errors
    /// A message naming the field and what it should have been.
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=5_000).contains(&self.half_spread_bps) {
            return Err("half_spread_bps must be in [1, 5000]".into());
        }
        if !(1..=20).contains(&self.levels) {
            return Err("levels must be in [1, 20]".into());
        }
        if !(1..=5_000).contains(&self.level_step_bps) {
            return Err("level_step_bps must be in [1, 5000]".into());
        }
        if self.size == 0 || self.size > 1_000_000_000 {
            return Err("size must be in [1, 10^9]".into());
        }
        if self.requote_bps > 5_000 {
            return Err("requote_bps must be at most 5000".into());
        }
        Ok(())
    }

    /// The price of level `k` on `side`, around `reference_cents`.
    ///
    /// Always at least a cent, and always on the correct side of the
    /// reference: a half-spread that rounds away to nothing would have the
    /// NPC quoting a locked market against itself.
    #[must_use]
    pub fn price_cents(&self, side: fehu::Side, reference_cents: i64, k: u32) -> i64 {
        let offset =
            i64::from(self.half_spread_bps) + i64::from(self.level_step_bps) * i64::from(k);
        let away = (reference_cents.saturating_mul(offset) / BPS).max(1);
        match side {
            fehu::Side::Buy => (reference_cents - away).max(1),
            fehu::Side::Sell => reference_cents.saturating_add(away),
        }
    }

    /// Whether quotes drawn around `quoted_cents` are still good at
    /// `reference_cents`.
    #[must_use]
    pub fn still_good(&self, quoted_cents: i64, reference_cents: i64) -> bool {
        if quoted_cents <= 0 {
            return false;
        }
        let moved = (reference_cents - quoted_cents).abs().saturating_mul(BPS);
        moved <= i64::from(self.requote_bps).saturating_mul(quoted_cents)
    }
}

/// How a producer restocks: which recipe it runs, when, and how much at a
/// time.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Production {
    /// The recipe it runs; it should make the symbol it sells.
    pub recipe: String,
    /// A job is started when its free stock of the symbol it sells is at or
    /// below this.
    pub restock_below: u64,
    /// Runs per job. See [`crate::jobs`]: a batch is one job.
    pub runs: u64,
    /// Jobs it will have in the furnace at once.
    pub max_running: u32,
}

impl Production {
    /// Check every field against its documented range. The recipe's
    /// existence is not checked here: a recipe may be written after the
    /// producer, and taken away while it runs, and either way the producer
    /// simply waits.
    ///
    /// # Errors
    /// A message naming the field and what it should have been.
    pub fn validate(&self) -> Result<(), String> {
        if self.recipe.trim().is_empty() {
            return Err("recipe must name a recipe".into());
        }
        if self.runs == 0 || self.runs > crate::jobs::MAX_JOB_RUNS {
            return Err(format!(
                "runs must be in [1, {}]",
                crate::jobs::MAX_JOB_RUNS
            ));
        }
        if !(1..=64).contains(&self.max_running) {
            return Err("max_running must be in [1, 64]".into());
        }
        Ok(())
    }
}

/// One NPC: who it is in the world, what it trades, and how it quotes.
#[derive(Clone, Debug, Serialize, Deserialize)]
// The ticker is registered on the way in rather than borrowed from the
// input, so the derive needs no `'de: 'static`.
#[serde(bound(deserialize = ""))]
pub struct Npc {
    /// The trader it places orders as.
    pub trader: TraderId,
    /// The user it belongs to. Nobody can sign in as it: the user is created
    /// without a key, so there is no credential to leak or to revoke.
    pub user_id: UserId,
    /// The account its money moves through. Its wallet is
    /// [`WalletKind::Npc`](fehu::ledger::WalletKind::Npc), so an audit can
    /// say how much of the world's currency is sitting in shop tills.
    pub account_id: AccountId,
    /// The one symbol it makes a market in.
    #[serde(with = "crate::save::symbol")]
    pub symbol: Symbol,
    pub name: String,
    pub policy: Policy,
    /// Quoting is on. An NPC that is switched off keeps its money and its
    /// inventory and simply stops putting them on the book — and, if it is a
    /// producer, stops restocking.
    pub active: bool,
    /// How it makes what it sells, if it does. `None` is a merchant.
    #[serde(default)]
    pub production: Option<Production>,
    /// The reference price its resting quotes were drawn around, or `0` if
    /// it has none out.
    pub quoted_ref_cents: i64,
    /// How many orders were actually resting when it last quoted.
    ///
    /// This is the other half of the re-quote decision, and the half that
    /// makes a merchant behave like one. The band on the reference stops it
    /// redrawing a book that has not moved; this stops it *not* redrawing
    /// one that has been eaten. Fewer orders resting than it left behind
    /// means something filled — or that it has just come into cash or stock
    /// it could not quote before — so it goes again, once, and records what
    /// stood up this time. Levels it still cannot fund are refused and
    /// simply not counted, so an NPC that can afford nothing settles at zero
    /// rather than trying every tick forever.
    #[serde(default)]
    pub quoted_orders: u32,
    /// The size it quoted at each level last time.
    ///
    /// The third half of the re-quote decision, and the one the world's
    /// appetite moves: a game event that changes demand
    /// ([`Effect::Demand`](crate::world::Effect::Demand)) changes what the
    /// policy size comes out as, and a merchant whose quotes are the wrong
    /// size for the world is redrawn even if the price has not moved and
    /// nothing has been eaten.
    #[serde(default)]
    pub quoted_size: u64,
    /// Its free stock of the symbol when it last quoted.
    ///
    /// The fourth half of the re-quote decision, for stock that arrives
    /// without an order going: a job delivering, a purchase, an endowment.
    /// A fill changes the resting count and is caught above; a delivery
    /// changes nothing on the book, and a producer whose ingots came out of
    /// the furnace and were never offered would be a furnace for nothing.
    #[serde(default)]
    pub quoted_stock: u64,
}

/// `GET /api/npcs`: who the world is trading as.
#[derive(Debug, Serialize)]
pub struct NpcsResponse {
    pub npcs: Vec<NpcDto>,
}

/// One NPC, as the API shows it: what it is, and what it has left.
#[derive(Clone, Debug, Serialize)]
pub struct NpcDto {
    pub trader_id: u64,
    pub user_id: u64,
    pub account_id: u64,
    #[serde(with = "crate::save::symbol")]
    pub symbol: Symbol,
    pub name: String,
    pub policy: Policy,
    pub active: bool,
    /// How it restocks, if it is a producer.
    pub production: Option<Production>,
    /// What it is quoting at each level now: its policy size, scaled by what
    /// the world wants of this good.
    pub quoted_size: u64,
    /// Currency it can still bid with.
    pub cash_cents: i64,
    /// Units it holds.
    pub inventory: u64,
    /// Units it has already promised to resting sells.
    pub reserved: u64,
}
