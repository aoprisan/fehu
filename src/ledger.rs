//! A conserved, auditable currency ledger: wallets, balanced transactions
//! and an explicit supply.
//!
//! This is the settlement layer under the exchange. [`book`](crate::book)
//! matches orders and knows nothing about money; this module holds the money
//! and knows nothing about orders. Both are pure integer arithmetic over
//! `alloc`, with no clock, no randomness and no host, so both run unchanged
//! on the `no_std` path and inside a game.
//!
//! # What conservation means here
//!
//! Every movement of currency is a [`Transaction`]: a set of signed
//! [`Posting`]s that **sums to zero**. Nothing is credited that was not
//! debited somewhere, so the ledger is conserved by construction rather than
//! by care, and the audit is a sum rather than a proof:
//!
//! ```text
//! Σ balances (every wallet but Issuance) = minted − burned
//! ```
//!
//! The only two operations that change that total are [`Reason::Mint`] and
//! [`Reason::Burn`], which post against the [`WalletKind::Issuance`] control
//! account. Issuance is the mirror of everything in circulation, so it runs
//! negative by design and is the one wallet outside the sum.
//!
//! [`WalletKind::Synthetic`] is *inside* it, and negative: that is the whole
//! point of having it. Currency handed to a player by liquidity nobody funded
//! is real currency, and the debt behind it belongs in the same sum, where an
//! audit can see it — see [`WalletKind`].
//!
//! # What it does not do
//!
//! No floating point, anywhere — currency is an integer count of cents and
//! `i64` throughout, and nothing saturates: an amount that does not fit is
//! [`LedgerError::Overflow`], refused before any wallet moves. There is no
//! rounding to get wrong because there is nothing to round.
//!
//! Reservations ([`Ledger::reserve`], [`Ledger::release`]) are amounts, not
//! handles. A resting buy order holds cash out of its wallet's available
//! balance without moving it, so a reservation is not a transaction and never
//! touches supply. Handles that name what a reservation is *for* — an order,
//! a production job — belong with the things that can hold one, and arrive
//! with them.
//!
//! # Example
//!
//! ```
//! use fehu::ledger::{Draft, Ledger, Reason, WalletKind};
//!
//! let mut ledger = Ledger::new();
//! let treasury = ledger.open(WalletKind::Treasury);
//! let alice = ledger.open(WalletKind::Player);
//!
//! // Genesis: the only currency that ever exists, minted into treasury.
//! ledger.mint(treasury, 1_000_000, Reason::Genesis).unwrap();
//! // A transfer moves it; it does not make more.
//! ledger
//!     .post(Draft::new(Reason::Transfer).debit(treasury, 250_000).credit(alice, 250_000))
//!     .unwrap();
//!
//! assert_eq!(ledger.balance(alice), 250_000);
//! assert_eq!(ledger.supply().outstanding_cents(), 1_000_000);
//! assert!(ledger.check().is_empty());
//! ```

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// Largest balance one wallet may hold: 10^15 cents, i.e. $10 trillion.
///
/// Small enough that a balance times a book-sized quantity still fits an
/// `i64`, large enough that no game will notice the ceiling.
pub const MAX_WALLET_CENTS: i64 = 1_000_000_000_000_000;

/// Which currency an amount is in.
///
/// One world has one currency today — `GAME`, counted in integer cents. The
/// field exists so that a second one is a change to this module rather than
/// to every id that crosses it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize), serde(transparent))]
pub struct CurrencyId(pub u16);

impl CurrencyId {
    /// The only currency this build implements.
    pub const GAME: Self = Self(0);
}

/// Identifies a wallet. Sequential from one and never reused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize), serde(transparent))]
pub struct WalletId(pub u64);

/// What a wallet is for.
///
/// The kind carries no policy of its own beyond the two exceptions to
/// non-negativity below; it is there so an audit can say *where* currency
/// sits, not merely how much of it there is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum WalletKind {
    /// One per account: a person's money.
    Player,
    /// Where the genesis supply lands, and what a faucet pays out of.
    Treasury,
    /// A finite pool a service credential may pay rewards from.
    Budget,
    /// A merchant or producer that holds currency and stock of its own.
    Npc,
    /// One per symbol: what funds that symbol's dividends and its buyout if
    /// it is delisted. A payout it cannot fund is refused, not clipped.
    Issuer,
    /// Where fees are collected. Nothing leaves the world through a fee.
    Venue,
    /// The mint and burn control account: the mirror of everything in
    /// circulation, and so always negative once anything has been minted.
    /// Excluded from the conservation sum for exactly that reason.
    Issuance,
    /// The counterparty for fills against liquidity nobody funded — the
    /// simulator's synthetic ladder and its printed flow.
    ///
    /// It is a placeholder, and the second wallet allowed to go negative.
    /// Its debt is not lost currency: it is precisely how much currency
    /// unfunded liquidity has put into players' hands, reported by an audit
    /// instead of quietly minted. Funding both sides of every fill retires
    /// it; until then it is measured rather than hidden.
    Synthetic,
}

impl WalletKind {
    /// This wallet may hold a negative balance.
    ///
    /// True only for [`Issuance`](Self::Issuance), whose negative balance is
    /// the supply itself, and [`Synthetic`](Self::Synthetic), whose negative
    /// balance is the debt of unfunded liquidity.
    pub fn may_be_negative(self) -> bool {
        matches!(self, Self::Issuance | Self::Synthetic)
    }

    /// This wallet's balance counts towards currency in circulation.
    ///
    /// Everything but [`Issuance`](Self::Issuance), which is the mirror of
    /// the sum rather than a term in it. A [`Synthetic`](Self::Synthetic)
    /// wallet counts: its debt is what offsets the currency it has handed
    /// out, and leaving it out would make the sum drift by exactly that
    /// amount.
    pub fn is_circulating(self) -> bool {
        self != Self::Issuance
    }

    /// A short, stable name for logs and audits.
    pub fn label(self) -> &'static str {
        match self {
            Self::Player => "player",
            Self::Treasury => "treasury",
            Self::Budget => "budget",
            Self::Npc => "npc",
            Self::Issuer => "issuer",
            Self::Venue => "venue",
            Self::Issuance => "issuance",
            Self::Synthetic => "synthetic",
        }
    }
}

/// Whether a wallet may move money.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum WalletStatus {
    /// Normal: credits and debits both go through.
    #[default]
    Active,
    /// Suspended: money can still be paid in, but nothing leaves.
    ///
    /// A freeze is something done *to* a wallet, so it does not stop what the
    /// wallet is owed from arriving — a frozen holder is still paid their
    /// dividend, because they still own the shares it is paid on.
    Frozen,
    /// Terminal: nothing moves in or out again. Reaching it requires an empty
    /// wallet, so closing can never strand currency.
    Closed,
}

impl WalletStatus {
    /// Money may be paid in.
    pub fn can_credit(self) -> bool {
        self != Self::Closed
    }

    /// Money may be taken out.
    pub fn can_debit(self) -> bool {
        self == Self::Active
    }

    /// A short, stable name for logs and audits.
    pub fn label(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Frozen => "frozen",
            Self::Closed => "closed",
        }
    }
}

/// A wallet: a balance, the part of it reserved against something resting,
/// and what the wallet is allowed to do.
///
/// The balance is private because it is only ever changed by a balanced
/// [`Transaction`]; there is no way to set it, which is the point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Wallet {
    /// This wallet's id.
    pub id: WalletId,
    /// What it is for.
    pub kind: WalletKind,
    /// Which currency it counts.
    pub currency: CurrencyId,
    /// Whether it may move money.
    pub status: WalletStatus,
    balance_cents: i64,
    reserved_cents: i64,
}

impl Wallet {
    /// Everything the wallet holds.
    pub fn balance_cents(self) -> i64 {
        self.balance_cents
    }

    /// The part of the balance committed to something still resting.
    pub fn reserved_cents(self) -> i64 {
        self.reserved_cents
    }

    /// What can be spent right now: `balance − reserved`, never negative.
    pub fn available_cents(self) -> i64 {
        self.balance_cents
            .saturating_sub(self.reserved_cents)
            .max(0)
    }
}

/// One side of a [`Transaction`]: what moves, and out of or into where.
///
/// Positive credits the wallet, negative debits it. The postings of a
/// transaction sum to zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Posting {
    /// The wallet this side moves.
    pub wallet: WalletId,
    /// Signed cents: positive in, negative out.
    pub amount_cents: i64,
}

/// Why currency moved. Recorded on every transaction, and the vocabulary an
/// audit reads back.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum Reason {
    /// The world's opening supply.
    Genesis,
    /// Currency created by an operator.
    Mint,
    /// Currency destroyed by an operator.
    Burn,
    /// Moved between two wallets on request.
    Transfer,
    /// Paid to a new account out of treasury, so that opening one does not
    /// create currency.
    Faucet,
    /// Paid from a budget for something the game says happened.
    Reward,
    /// A good bought from a catalogue.
    Purchase,
    /// The venue's cut of a fill.
    Fee,
    /// The venue paying for liquidity.
    Rebate,
    /// A fill settled: the buyer's side.
    Buy,
    /// A fill settled: the seller's side.
    Sell,
    /// Paid on shares held when a dividend was declared.
    Dividend,
    /// Paid for shares taken away by a delisting.
    Delisting,
    /// What starting a production job cost.
    JobCost,
    /// What cancelling one gave back.
    JobRefund,
    /// Balances carried in from a world that predates this ledger.
    Migration,
}

impl Reason {
    /// A short, stable name for logs and audits.
    pub fn label(self) -> &'static str {
        match self {
            Self::Genesis => "genesis",
            Self::Mint => "mint",
            Self::Burn => "burn",
            Self::Transfer => "transfer",
            Self::Faucet => "faucet",
            Self::Reward => "reward",
            Self::Purchase => "purchase",
            Self::Fee => "fee",
            Self::Rebate => "rebate",
            Self::Buy => "buy",
            Self::Sell => "sell",
            Self::Dividend => "dividend",
            Self::Delisting => "delisting",
            Self::JobCost => "job_cost",
            Self::JobRefund => "job_refund",
            Self::Migration => "migration",
        }
    }

    /// This reason may change the supply, and so must post against issuance.
    pub fn is_issuance(self) -> bool {
        matches!(
            self,
            Self::Genesis | Self::Mint | Self::Burn | Self::Migration
        )
    }
}

/// A transaction that has not been posted yet: the postings, and why.
///
/// Built up and handed to [`Ledger::post`], which checks it whole and either
/// applies every posting or none. The ledger assigns the id — a draft has
/// none, because a transaction that was refused never happened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Draft {
    /// Why the currency moved.
    pub reason: Reason,
    /// The journal entry that caused it, once there is a journal. Zero means
    /// "not from a journalled command".
    pub command_id: u64,
    /// Simulated time the movement belongs to.
    pub tick: u64,
    /// The sides, in the order they were added.
    pub postings: Vec<Posting>,
    /// A human-readable note.
    pub memo: Option<String>,
}

impl Draft {
    /// An empty draft with no postings.
    pub fn new(reason: Reason) -> Self {
        Self {
            reason,
            command_id: 0,
            tick: 0,
            postings: Vec::new(),
            memo: None,
        }
    }

    /// Take `cents` out of `wallet`. Zero adds nothing: a movement of nothing
    /// is not a side.
    #[must_use]
    pub fn debit(self, wallet: WalletId, cents: i64) -> Self {
        self.posting(wallet, -cents)
    }

    /// Put `cents` into `wallet`. Zero adds nothing.
    #[must_use]
    pub fn credit(self, wallet: WalletId, cents: i64) -> Self {
        self.posting(wallet, cents)
    }

    /// Add a signed posting directly.
    #[must_use]
    pub fn posting(mut self, wallet: WalletId, amount_cents: i64) -> Self {
        if amount_cents != 0 {
            self.postings.push(Posting {
                wallet,
                amount_cents,
            });
        }
        self
    }

    /// Attach a note.
    #[must_use]
    pub fn memo(mut self, memo: Option<String>) -> Self {
        self.memo = memo;
        self
    }

    /// Say which simulated instant this belongs to.
    #[must_use]
    pub fn at(mut self, tick: u64) -> Self {
        self.tick = tick;
        self
    }

    /// Say which journalled command caused it.
    #[must_use]
    pub fn caused_by(mut self, command_id: u64) -> Self {
        self.command_id = command_id;
        self
    }

    /// The postings added so far, summed. Zero for a balanced draft.
    pub fn imbalance_cents(&self) -> i128 {
        self.postings
            .iter()
            .map(|p| i128::from(p.amount_cents))
            .sum()
    }

    /// Nothing was posted.
    pub fn is_empty(&self) -> bool {
        self.postings.is_empty()
    }
}

/// A transaction the ledger accepted and applied.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Transaction {
    /// Sequential from one, never reused.
    pub id: u64,
    /// The journal entry that caused it, or zero.
    pub command_id: u64,
    /// Simulated time the movement belongs to.
    pub tick: u64,
    /// Why the currency moved.
    pub reason: Reason,
    /// The sides. They sum to zero.
    pub postings: Vec<Posting>,
    /// A human-readable note.
    pub memo: Option<String>,
}

impl Transaction {
    /// What this transaction did to `wallet`, summed over its postings.
    pub fn effect_on(&self, wallet: WalletId) -> i64 {
        self.postings
            .iter()
            .filter(|p| p.wallet == wallet)
            .map(|p| p.amount_cents)
            .sum()
    }

    /// The total moved: the sum of the credits, which for a balanced
    /// transaction is also the sum of the debits.
    pub fn volume_cents(&self) -> i64 {
        self.postings
            .iter()
            .map(|p| p.amount_cents)
            .filter(|c| *c > 0)
            .sum()
    }
}

/// How much currency has ever existed, and how much has been destroyed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Supply {
    /// Created since genesis, genesis included.
    pub minted_cents: i64,
    /// Destroyed since genesis.
    pub burned_cents: i64,
}

impl Supply {
    /// Currency in circulation: `minted − burned`. This is what every wallet
    /// but issuance must add up to.
    ///
    /// It can sit below zero, but only while unfunded liquidity is in debt
    /// for at least as much — see [`WalletKind::Synthetic`]. In a world where
    /// both sides of every fill are funded it is the plain supply.
    pub fn outstanding_cents(self) -> i64 {
        self.minted_cents.saturating_sub(self.burned_cents)
    }
}

/// Why a movement was refused. Every amount is in cents.
///
/// A refused movement changes nothing at all: the ledger validates a whole
/// transaction before it applies any part of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LedgerError {
    /// No such wallet.
    UnknownWallet(WalletId),
    /// The postings do not sum to zero.
    Unbalanced {
        /// What they sum to instead.
        imbalance_cents: i128,
    },
    /// A transaction with no postings, which cannot say anything.
    Empty,
    /// A mint or burn that does not post against issuance, or an ordinary
    /// movement that does.
    NotIssuance {
        /// The reason that was given.
        reason: Reason,
    },
    /// Two wallets in different currencies in one transaction.
    CurrencyMismatch {
        /// What the first posting was in.
        expected: CurrencyId,
        /// What a later one was in.
        found: CurrencyId,
    },
    /// The wallet's status forbids this side of the movement.
    Status {
        /// Which wallet.
        wallet: WalletId,
        /// What it is.
        status: WalletStatus,
        /// What was attempted.
        action: &'static str,
    },
    /// Not enough available currency: the balance, less what is reserved.
    Insufficient {
        /// Which wallet.
        wallet: WalletId,
        /// What the debit needed.
        needed_cents: i64,
        /// What it could have had.
        available_cents: i64,
    },
    /// The credit would push a balance past [`MAX_WALLET_CENTS`].
    BalanceCap {
        /// Which wallet.
        wallet: WalletId,
        /// What it holds now.
        balance_cents: i64,
        /// What was being added.
        amount_cents: i64,
    },
    /// An amount that has to be positive was not.
    NotPositive {
        /// What was given.
        amount_cents: i64,
    },
    /// The arithmetic does not fit an `i64`. Refused rather than saturated:
    /// a total that is wrong is worse than one that is missing.
    Overflow,
    /// A wallet was to be closed while it still held or reserved something.
    NotEmpty {
        /// Which wallet.
        wallet: WalletId,
        /// What it still holds.
        balance_cents: i64,
        /// What is still reserved out of that.
        reserved_cents: i64,
    },
    /// A closed wallet cannot be reopened.
    Closed(WalletId),
    /// A wallet allowed to hold a negative balance was asked to reserve.
    /// There is no solvency check on it for a reservation to protect.
    CannotReserve(WalletId),
}

impl core::fmt::Display for LedgerError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            Self::UnknownWallet(w) => write!(f, "no wallet {}", w.0),
            Self::Unbalanced { imbalance_cents } => write!(
                f,
                "postings do not sum to zero: they sum to {imbalance_cents}"
            ),
            Self::Empty => write!(f, "a transaction with no postings moves nothing"),
            Self::NotIssuance { reason } => write!(
                f,
                "{} must post against the issuance wallet, and only it may",
                reason.label()
            ),
            Self::CurrencyMismatch { expected, found } => write!(
                f,
                "currency {} cannot be posted against currency {}",
                found.0, expected.0
            ),
            Self::Status {
                wallet,
                status,
                action,
            } => write!(
                f,
                "wallet {} is {}: cannot {action}",
                wallet.0,
                status.label()
            ),
            Self::Insufficient {
                wallet,
                needed_cents,
                available_cents,
            } => write!(
                f,
                "wallet {} has {} available, needs {}",
                wallet.0,
                cents(available_cents),
                cents(needed_cents)
            ),
            Self::BalanceCap {
                wallet,
                balance_cents,
                amount_cents,
            } => write!(
                f,
                "{} on top of wallet {}'s {} would pass the {} limit",
                cents(amount_cents),
                wallet.0,
                cents(balance_cents),
                cents(MAX_WALLET_CENTS)
            ),
            Self::NotPositive { amount_cents } => {
                write!(f, "amount must be positive, got {amount_cents}")
            }
            Self::Overflow => write!(f, "the amount does not fit"),
            Self::NotEmpty {
                wallet,
                balance_cents,
                reserved_cents,
            } => write!(
                f,
                "wallet {} still holds {} ({} reserved)",
                wallet.0,
                cents(balance_cents),
                cents(reserved_cents)
            ),
            Self::Closed(w) => write!(f, "wallet {} is closed and cannot be reopened", w.0),
            Self::CannotReserve(w) => {
                write!(f, "wallet {} may owe, so it has nothing to reserve", w.0)
            }
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for LedgerError {}

/// Every wallet, the supply, and the arithmetic that keeps them in step.
///
/// Cloneable and serialisable whole, private fields included: a save file has
/// to carry the balances, not a view of them.
#[derive(Clone, Debug, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Ledger {
    wallets: BTreeMap<WalletId, Wallet>,
    supply: Supply,
    next_wallet: u64,
    next_tx: u64,
}

impl Ledger {
    /// An empty ledger: no wallets, no supply.
    pub fn new() -> Self {
        Self {
            wallets: BTreeMap::new(),
            supply: Supply::default(),
            next_wallet: 1,
            next_tx: 1,
        }
    }

    /// Open a wallet of `kind` in the only currency there is.
    pub fn open(&mut self, kind: WalletKind) -> WalletId {
        self.open_in(kind, CurrencyId::GAME)
    }

    /// Open a wallet of `kind` counting `currency`.
    pub fn open_in(&mut self, kind: WalletKind, currency: CurrencyId) -> WalletId {
        let id = WalletId(self.next_wallet);
        self.next_wallet += 1;
        self.wallets.insert(
            id,
            Wallet {
                id,
                kind,
                currency,
                status: WalletStatus::Active,
                balance_cents: 0,
                reserved_cents: 0,
            },
        );
        id
    }

    /// The wallet, if there is one.
    pub fn wallet(&self, id: WalletId) -> Option<Wallet> {
        self.wallets.get(&id).copied()
    }

    /// Every wallet, in id order.
    pub fn wallets(&self) -> impl Iterator<Item = &Wallet> {
        self.wallets.values()
    }

    /// How many wallets are open.
    pub fn len(&self) -> usize {
        self.wallets.len()
    }

    /// No wallets at all.
    pub fn is_empty(&self) -> bool {
        self.wallets.is_empty()
    }

    /// What `wallet` holds; zero if there is no such wallet.
    pub fn balance(&self, wallet: WalletId) -> i64 {
        self.wallets.get(&wallet).map_or(0, |w| w.balance_cents)
    }

    /// What `wallet` has committed to something resting.
    pub fn reserved(&self, wallet: WalletId) -> i64 {
        self.wallets.get(&wallet).map_or(0, |w| w.reserved_cents)
    }

    /// What `wallet` could spend right now.
    pub fn available(&self, wallet: WalletId) -> i64 {
        self.wallets.get(&wallet).map_or(0, |w| w.available_cents())
    }

    /// The status of `wallet`; a wallet that does not exist is treated as
    /// closed, because nothing can move through it either.
    pub fn status(&self, wallet: WalletId) -> WalletStatus {
        self.wallets
            .get(&wallet)
            .map_or(WalletStatus::Closed, |w| w.status)
    }

    /// How much currency exists.
    pub fn supply(&self) -> Supply {
        self.supply
    }

    /// The id the next transaction will take.
    pub fn next_transaction_id(&self) -> u64 {
        self.next_tx
    }

    /// Everything but issuance, added up — synthetic debt included, as a
    /// negative term. Equal to [`Supply::outstanding_cents`] in a healthy
    /// ledger, and the audit that says so is [`Ledger::check`].
    pub fn circulating_cents(&self) -> i128 {
        self.wallets
            .values()
            .filter(|w| w.kind.is_circulating())
            .map(|w| i128::from(w.balance_cents))
            .sum()
    }

    /// What unfunded liquidity has put into the world: the debt of every
    /// [`WalletKind::Synthetic`] wallet, as a positive number.
    ///
    /// Zero once both sides of every fill are funded. Until then it is the
    /// one number that says how far short of a closed economy the world is.
    pub fn synthetic_debt_cents(&self) -> i128 {
        self.wallets
            .values()
            .filter(|w| w.kind == WalletKind::Synthetic)
            .map(|w| -i128::from(w.balance_cents))
            .sum::<i128>()
            .max(0)
    }

    /// Apply `draft` whole, or refuse it and change nothing.
    ///
    /// Checks, in order: the draft is non-empty and balanced; every wallet
    /// exists and shares one currency; the reason posts against issuance if
    /// and only if it is a mint or a burn; every debit is allowed by status
    /// and covered by the available balance; every credit is allowed by
    /// status and fits under [`MAX_WALLET_CENTS`]. Only then does anything
    /// move.
    ///
    /// Returns the transaction as it was recorded, id and all. It is handed
    /// back by value rather than kept: the ledger holds balances, and the
    /// history of how they got there belongs to whatever journals it.
    pub fn post(&mut self, draft: Draft) -> Result<Transaction, LedgerError> {
        self.validate(&draft)?;
        let Draft {
            reason,
            command_id,
            tick,
            postings,
            memo,
        } = draft;
        // Everything below this line has been checked and cannot fail.
        //
        // Apply the *net* per wallet, which is what was validated. Applying
        // the postings one at a time would let a transaction that nets to
        // nothing overflow on the way through — debit a wallet `i64::MAX`,
        // credit it back — and that intermediate balance is an artefact of
        // the order the sides were written in, not something the ledger
        // should be able to see, let alone trip over.
        for (id, amount) in net_postings(&postings) {
            let wallet = self.wallets.get_mut(&id).expect("validated above");
            wallet.balance_cents += i64::try_from(amount).expect("validated above");
        }
        match reason {
            Reason::Burn => {
                // A burn credits issuance, so the credit is what was
                // destroyed.
                let burned: i64 = postings
                    .iter()
                    .filter(|p| self.wallets[&p.wallet].kind == WalletKind::Issuance)
                    .map(|p| p.amount_cents)
                    .sum();
                self.supply.burned_cents += burned;
            }
            reason if reason.is_issuance() => {
                let minted: i64 = postings
                    .iter()
                    .filter(|p| self.wallets[&p.wallet].kind == WalletKind::Issuance)
                    .map(|p| -p.amount_cents)
                    .sum();
                self.supply.minted_cents += minted;
            }
            _ => {}
        }
        let id = self.next_tx;
        self.next_tx += 1;
        Ok(Transaction {
            id,
            command_id,
            tick,
            reason,
            postings,
            memo,
        })
    }

    /// Everything [`post`](Self::post) checks, without applying anything.
    ///
    /// Useful where a caller must know a movement will go through before it
    /// does something else it cannot undo — winding up a symbol before its
    /// buyout, say.
    pub fn validate(&self, draft: &Draft) -> Result<(), LedgerError> {
        if draft.is_empty() {
            return Err(LedgerError::Empty);
        }
        let imbalance = draft.imbalance_cents();
        if imbalance != 0 {
            return Err(LedgerError::Unbalanced {
                imbalance_cents: imbalance,
            });
        }
        let mut currency = None;
        let mut touches_issuance = false;
        for posting in &draft.postings {
            let wallet = self
                .wallets
                .get(&posting.wallet)
                .ok_or(LedgerError::UnknownWallet(posting.wallet))?;
            match currency {
                None => currency = Some(wallet.currency),
                Some(expected) if expected != wallet.currency => {
                    return Err(LedgerError::CurrencyMismatch {
                        expected,
                        found: wallet.currency,
                    });
                }
                Some(_) => {}
            }
            touches_issuance |= wallet.kind == WalletKind::Issuance;
        }
        if touches_issuance != draft.reason.is_issuance() {
            return Err(LedgerError::NotIssuance {
                reason: draft.reason,
            });
        }
        // Net each wallet first: a transaction that both debits and credits
        // one wallet is judged on what it does to it, not on the order the
        // sides were written in.
        for (id, amount) in net_postings(&draft.postings) {
            let wallet = self.wallets.get(&id).expect("resolved above");
            let amount = i64::try_from(amount).map_err(|_| LedgerError::Overflow)?;
            let balance = wallet
                .balance_cents
                .checked_add(amount)
                .ok_or(LedgerError::Overflow)?;
            if amount < 0 {
                if !wallet.status.can_debit() {
                    return Err(LedgerError::Status {
                        wallet: id,
                        status: wallet.status,
                        action: "pay out",
                    });
                }
                // Issuance and synthetic wallets may owe — but only for what
                // they are each allowed to owe *for*. Issuance goes negative
                // because a mint is what makes it so. A synthetic wallet goes
                // negative by settling fills nobody funded, and never by
                // burning: currency it does not hold is not currency it can
                // destroy, and letting it would drive the supply below zero
                // with nothing behind the number.
                let may_owe = wallet.kind.may_be_negative()
                    && (wallet.kind == WalletKind::Issuance || !draft.reason.is_issuance());
                if !may_owe && balance < wallet.reserved_cents {
                    return Err(LedgerError::Insufficient {
                        wallet: id,
                        needed_cents: -amount,
                        available_cents: wallet.available_cents(),
                    });
                }
            } else if amount > 0 {
                if !wallet.status.can_credit() {
                    return Err(LedgerError::Status {
                        wallet: id,
                        status: wallet.status,
                        action: "take money in",
                    });
                }
                if balance > MAX_WALLET_CENTS {
                    return Err(LedgerError::BalanceCap {
                        wallet: id,
                        balance_cents: wallet.balance_cents,
                        amount_cents: amount,
                    });
                }
            }
        }
        Ok(())
    }

    /// Mint `cents` into `to`, against issuance. Operator authority; the
    /// ledger only checks the arithmetic.
    pub fn mint(
        &mut self,
        to: WalletId,
        cents: i64,
        reason: Reason,
    ) -> Result<Transaction, LedgerError> {
        if cents <= 0 {
            return Err(LedgerError::NotPositive {
                amount_cents: cents,
            });
        }
        let issuance = self.issuance()?;
        self.post(Draft::new(reason).debit(issuance, cents).credit(to, cents))
    }

    /// Burn `cents` out of `from`, against issuance.
    pub fn burn(&mut self, from: WalletId, cents: i64) -> Result<Transaction, LedgerError> {
        if cents <= 0 {
            return Err(LedgerError::NotPositive {
                amount_cents: cents,
            });
        }
        let issuance = self.issuance()?;
        self.post(
            Draft::new(Reason::Burn)
                .debit(from, cents)
                .credit(issuance, cents),
        )
    }

    /// The issuance wallet, opening one if this ledger has none.
    ///
    /// There is exactly one: the first wallet of that kind, by id.
    pub fn issuance_wallet(&mut self) -> WalletId {
        match self.issuance() {
            Ok(id) => id,
            Err(_) => self.open(WalletKind::Issuance),
        }
    }

    fn issuance(&self) -> Result<WalletId, LedgerError> {
        self.wallets
            .values()
            .find(|w| w.kind == WalletKind::Issuance)
            .map(|w| w.id)
            .ok_or(LedgerError::UnknownWallet(WalletId(0)))
    }

    /// Commit `cents` of `wallet`'s available balance to something resting.
    ///
    /// A reservation is not a movement: no currency changes hands, supply is
    /// untouched, and nothing is written to the transaction sequence.
    pub fn reserve(&mut self, wallet: WalletId, cents: i64) -> Result<(), LedgerError> {
        if cents < 0 {
            return Err(LedgerError::NotPositive {
                amount_cents: cents,
            });
        }
        let w = self
            .wallets
            .get(&wallet)
            .ok_or(LedgerError::UnknownWallet(wallet))?;
        if !w.status.can_debit() {
            return Err(LedgerError::Status {
                wallet,
                status: w.status,
                action: "commit money to an order",
            });
        }
        // A reservation exists to hold cash back from a solvency check. A
        // wallet that is allowed to owe has no such check to hold anything
        // back from, and letting it reserve would give it a promise it could
        // then be drained straight through.
        if w.kind.may_be_negative() {
            return Err(LedgerError::CannotReserve(wallet));
        }
        let reserved = w
            .reserved_cents
            .checked_add(cents)
            .ok_or(LedgerError::Overflow)?;
        if reserved > w.balance_cents {
            return Err(LedgerError::Insufficient {
                wallet,
                needed_cents: cents,
                available_cents: w.available_cents(),
            });
        }
        self.wallets
            .get_mut(&wallet)
            .expect("resolved above")
            .reserved_cents = reserved;
        Ok(())
    }

    /// Give `cents` of `wallet`'s reservation back: the order filled, or was
    /// cancelled. Releasing more than is held releases what is held.
    pub fn release(&mut self, wallet: WalletId, cents: i64) {
        if let Some(w) = self.wallets.get_mut(&wallet) {
            w.reserved_cents = w.reserved_cents.saturating_sub(cents.max(0)).max(0);
        }
    }

    /// Freeze `wallet`: credits still land, debits stop.
    ///
    /// Refused for a closed wallet, which is terminal.
    pub fn freeze(&mut self, wallet: WalletId) -> Result<(), LedgerError> {
        self.set_status(wallet, WalletStatus::Frozen)
    }

    /// Unfreeze `wallet`. Refused for a closed wallet.
    pub fn unfreeze(&mut self, wallet: WalletId) -> Result<(), LedgerError> {
        self.set_status(wallet, WalletStatus::Active)
    }

    /// Close `wallet` for good.
    ///
    /// Requires it to be empty — no balance and nothing reserved — so that
    /// closing can never strand currency where nothing can reach it. That is
    /// what makes closing different in kind from freezing, and why they are
    /// not one call with a parameter.
    pub fn close(&mut self, wallet: WalletId) -> Result<(), LedgerError> {
        let w = self
            .wallets
            .get(&wallet)
            .ok_or(LedgerError::UnknownWallet(wallet))?;
        if w.balance_cents != 0 || w.reserved_cents != 0 {
            return Err(LedgerError::NotEmpty {
                wallet,
                balance_cents: w.balance_cents,
                reserved_cents: w.reserved_cents,
            });
        }
        self.wallets
            .get_mut(&wallet)
            .expect("resolved above")
            .status = WalletStatus::Closed;
        Ok(())
    }

    fn set_status(&mut self, wallet: WalletId, status: WalletStatus) -> Result<(), LedgerError> {
        let w = self
            .wallets
            .get_mut(&wallet)
            .ok_or(LedgerError::UnknownWallet(wallet))?;
        if w.status == WalletStatus::Closed {
            return Err(LedgerError::Closed(wallet));
        }
        w.status = status;
        Ok(())
    }

    /// Every broken invariant, in plain words. Empty for a healthy ledger.
    ///
    /// This is the audit the plan asks a reconcile endpoint to report: the
    /// conservation sum, non-negativity outside the two wallets allowed to
    /// owe, and reservations that fit inside the balances backing them.
    pub fn check(&self) -> Vec<String> {
        let mut issues = Vec::new();
        let circulating = self.circulating_cents();
        let outstanding = i128::from(self.supply.outstanding_cents());
        if circulating != outstanding {
            issues.push(format!(
                "circulating balances total {circulating} but minted minus burned is {outstanding}"
            ));
        }
        if self.supply.minted_cents < 0 || self.supply.burned_cents < 0 {
            issues.push(format!(
                "supply is negative: minted {} burned {}",
                self.supply.minted_cents, self.supply.burned_cents
            ));
        }
        // More may be burned than was minted, but only to the extent that
        // unfunded liquidity put currency into the world in the first place:
        // that debt is the slack, and past it the number is wrong.
        let debt = self.synthetic_debt_cents();
        if i128::from(self.supply.burned_cents) > i128::from(self.supply.minted_cents) + debt {
            issues.push(format!(
                "more has been burned ({}) than was ever minted ({}) or owed by unfunded \
                 liquidity ({debt})",
                self.supply.burned_cents, self.supply.minted_cents
            ));
        }
        let issuance: i128 = self
            .wallets
            .values()
            .filter(|w| w.kind == WalletKind::Issuance)
            .map(|w| i128::from(w.balance_cents))
            .sum();
        if issuance != -outstanding {
            issues.push(format!(
                "issuance holds {issuance}, which is not the mirror of the {outstanding} \
                 in circulation"
            ));
        }
        if self
            .wallets
            .values()
            .filter(|w| w.kind == WalletKind::Issuance)
            .count()
            > 1
        {
            issues.push("more than one issuance wallet".into());
        }
        for wallet in self.wallets.values() {
            if wallet.balance_cents < 0 && !wallet.kind.may_be_negative() {
                issues.push(format!(
                    "wallet {} ({}) is overdrawn by {}",
                    wallet.id.0,
                    wallet.kind.label(),
                    cents(-wallet.balance_cents)
                ));
            }
            if wallet.balance_cents > MAX_WALLET_CENTS {
                issues.push(format!(
                    "wallet {} holds {}, past the {} limit",
                    wallet.id.0,
                    cents(wallet.balance_cents),
                    cents(MAX_WALLET_CENTS)
                ));
            }
            if wallet.reserved_cents < 0 {
                issues.push(format!(
                    "wallet {} reserves a negative {}",
                    wallet.id.0,
                    cents(wallet.reserved_cents)
                ));
            }
            // A wallet allowed to owe reserves nothing, so this compares
            // what is held against what backs it without tripping over the
            // negative balance that issuance is supposed to have.
            if wallet.reserved_cents > 0 && wallet.reserved_cents > wallet.balance_cents {
                issues.push(format!(
                    "wallet {} reserves {} against a balance of {}",
                    wallet.id.0,
                    cents(wallet.reserved_cents),
                    cents(wallet.balance_cents)
                ));
            }
        }
        issues
    }

    /// Every invariant holds.
    pub fn is_valid(&self) -> bool {
        self.check().is_empty()
    }
}

/// What a set of postings does to each wallet it names, summed in `i128` so
/// that the total is exact whatever order the sides were written in.
fn net_postings(postings: &[Posting]) -> BTreeMap<WalletId, i128> {
    let mut net = BTreeMap::<WalletId, i128>::new();
    for posting in postings {
        *net.entry(posting.wallet).or_default() += i128::from(posting.amount_cents);
    }
    net
}

/// `price_cents × qty` as an exact `i64`, or [`LedgerError::Overflow`].
///
/// The authoritative counterpart to a saturating notional: a product that
/// does not fit is refused before anything moves, rather than clamped into a
/// number that is merely wrong.
pub fn checked_notional_cents(price_cents: i64, qty: u64) -> Result<i64, LedgerError> {
    let product = i128::from(price_cents) * i128::from(qty);
    i64::try_from(product).map_err(|_| LedgerError::Overflow)
}

/// Format cents for a human, by integer division only: `123456` → `1234.56`.
pub fn cents(amount: i64) -> String {
    let sign = if amount < 0 { "-" } else { "" };
    let abs = amount.unsigned_abs();
    format!("{sign}{}.{:02}", abs / 100, abs % 100)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world() -> (Ledger, WalletId, WalletId, WalletId) {
        let mut ledger = Ledger::new();
        let issuance = ledger.issuance_wallet();
        let treasury = ledger.open(WalletKind::Treasury);
        let alice = ledger.open(WalletKind::Player);
        ledger.mint(treasury, 1_000_000, Reason::Genesis).unwrap();
        (ledger, issuance, treasury, alice)
    }

    #[test]
    fn genesis_mints_against_issuance_and_nothing_else_does() {
        let (ledger, issuance, treasury, _) = world();
        assert_eq!(ledger.balance(treasury), 1_000_000);
        assert_eq!(ledger.balance(issuance), -1_000_000);
        assert_eq!(ledger.supply().outstanding_cents(), 1_000_000);
        assert_eq!(ledger.circulating_cents(), 1_000_000);
        assert!(ledger.check().is_empty(), "{:?}", ledger.check());
    }

    #[test]
    fn a_transfer_moves_currency_without_making_any() {
        let (mut ledger, _, treasury, alice) = world();
        let before = ledger.supply();
        ledger
            .post(
                Draft::new(Reason::Transfer)
                    .debit(treasury, 400_000)
                    .credit(alice, 400_000),
            )
            .unwrap();
        assert_eq!(ledger.balance(alice), 400_000);
        assert_eq!(ledger.balance(treasury), 600_000);
        assert_eq!(ledger.supply(), before, "a transfer is not a mint");
        assert!(ledger.check().is_empty());
    }

    #[test]
    fn an_unbalanced_or_mislabelled_draft_is_refused() {
        let (mut ledger, issuance, treasury, alice) = world();
        assert!(matches!(
            ledger
                .post(
                    Draft::new(Reason::Transfer)
                        .debit(treasury, 10)
                        .credit(alice, 9)
                )
                .unwrap_err(),
            LedgerError::Unbalanced {
                imbalance_cents: -1
            }
        ));
        assert!(matches!(
            ledger.post(Draft::new(Reason::Transfer)).unwrap_err(),
            LedgerError::Empty
        ));
        // Only a mint or a burn may touch issuance...
        assert!(matches!(
            ledger
                .post(
                    Draft::new(Reason::Transfer)
                        .debit(issuance, 10)
                        .credit(alice, 10)
                )
                .unwrap_err(),
            LedgerError::NotIssuance { .. }
        ));
        // ...and one that does not touch it is not a mint.
        assert!(matches!(
            ledger
                .post(
                    Draft::new(Reason::Mint)
                        .debit(treasury, 10)
                        .credit(alice, 10)
                )
                .unwrap_err(),
            LedgerError::NotIssuance { .. }
        ));
        assert!(ledger.check().is_empty(), "nothing was applied");
        assert_eq!(ledger.balance(alice), 0);
    }

    #[test]
    fn a_refused_transaction_changes_nothing() {
        let (mut ledger, _, treasury, alice) = world();
        let venue = ledger.open(WalletKind::Venue);
        // The second debit is the one that cannot be covered, so the whole
        // draft must be refused — including the first, valid, side.
        let err = ledger
            .post(
                Draft::new(Reason::Transfer)
                    .debit(treasury, 100)
                    .credit(alice, 100)
                    .debit(venue, 500)
                    .credit(treasury, 500),
            )
            .unwrap_err();
        assert!(matches!(err, LedgerError::Insufficient { .. }));
        assert_eq!(ledger.balance(alice), 0);
        assert_eq!(ledger.balance(treasury), 1_000_000);
        assert_eq!(ledger.balance(venue), 0);
    }

    #[test]
    fn a_wallet_is_judged_on_the_net_of_its_postings() {
        let (mut ledger, _, treasury, alice) = world();
        // Alice is empty, but this hands her 100 before taking 100 away.
        ledger
            .post(
                Draft::new(Reason::Transfer)
                    .credit(alice, 100)
                    .debit(alice, 100)
                    .debit(treasury, 100)
                    .credit(treasury, 100),
            )
            .unwrap();
        assert_eq!(ledger.balance(alice), 0);
    }

    #[test]
    fn burning_takes_currency_out_of_the_world() {
        let (mut ledger, issuance, treasury, _) = world();
        ledger.burn(treasury, 250_000).unwrap();
        assert_eq!(ledger.supply().burned_cents, 250_000);
        assert_eq!(ledger.supply().outstanding_cents(), 750_000);
        assert_eq!(ledger.balance(issuance), -750_000);
        assert!(ledger.check().is_empty());
        assert!(matches!(
            ledger.burn(treasury, 0).unwrap_err(),
            LedgerError::NotPositive { .. }
        ));
    }

    #[test]
    fn reservations_hold_cash_back_without_moving_it() {
        let (mut ledger, _, treasury, alice) = world();
        ledger
            .post(
                Draft::new(Reason::Faucet)
                    .debit(treasury, 10_000)
                    .credit(alice, 10_000),
            )
            .unwrap();
        let tx = ledger.next_transaction_id();
        ledger.reserve(alice, 4_000).unwrap();
        assert_eq!(ledger.available(alice), 6_000);
        assert_eq!(ledger.balance(alice), 10_000, "a reservation moves nothing");
        assert_eq!(ledger.next_transaction_id(), tx, "and posts nothing");
        assert!(matches!(
            ledger.reserve(alice, 6_001).unwrap_err(),
            LedgerError::Insufficient { .. }
        ));
        // Only the available part can be spent.
        assert!(matches!(
            ledger
                .post(
                    Draft::new(Reason::Transfer)
                        .debit(alice, 6_001)
                        .credit(treasury, 6_001)
                )
                .unwrap_err(),
            LedgerError::Insufficient { .. }
        ));
        ledger
            .post(
                Draft::new(Reason::Transfer)
                    .debit(alice, 6_000)
                    .credit(treasury, 6_000),
            )
            .unwrap();
        ledger.release(alice, 99_999);
        assert_eq!(
            ledger.reserved(alice),
            0,
            "releasing more releases what is held"
        );
        assert!(ledger.check().is_empty());
    }

    #[test]
    fn freezing_stops_debits_and_lets_credits_land() {
        let (mut ledger, _, treasury, alice) = world();
        ledger
            .post(
                Draft::new(Reason::Faucet)
                    .debit(treasury, 500)
                    .credit(alice, 500),
            )
            .unwrap();
        ledger.freeze(alice).unwrap();
        assert!(matches!(
            ledger
                .post(Draft::new(Reason::Buy).debit(alice, 1).credit(treasury, 1))
                .unwrap_err(),
            LedgerError::Status { .. }
        ));
        // A dividend still reaches a frozen holder: the shares are theirs.
        ledger
            .post(
                Draft::new(Reason::Dividend)
                    .debit(treasury, 7)
                    .credit(alice, 7),
            )
            .unwrap();
        assert_eq!(ledger.balance(alice), 507);
        assert!(ledger.reserve(alice, 1).is_err(), "and cannot commit money");
        ledger.unfreeze(alice).unwrap();
        ledger.reserve(alice, 1).unwrap();
    }

    #[test]
    fn closing_is_not_freezing_and_needs_an_empty_wallet() {
        let (mut ledger, _, treasury, alice) = world();
        ledger
            .post(
                Draft::new(Reason::Faucet)
                    .debit(treasury, 500)
                    .credit(alice, 500),
            )
            .unwrap();
        assert!(matches!(
            ledger.close(alice).unwrap_err(),
            LedgerError::NotEmpty { .. }
        ));
        ledger
            .post(
                Draft::new(Reason::Transfer)
                    .debit(alice, 500)
                    .credit(treasury, 500),
            )
            .unwrap();
        ledger.reserve(alice, 0).unwrap();
        ledger.close(alice).unwrap();
        assert!(matches!(
            ledger.unfreeze(alice).unwrap_err(),
            LedgerError::Closed(_)
        ));
        assert!(matches!(
            ledger
                .post(
                    Draft::new(Reason::Dividend)
                        .debit(treasury, 1)
                        .credit(alice, 1)
                )
                .unwrap_err(),
            LedgerError::Status { .. }
        ));
    }

    #[test]
    fn the_balance_cap_and_overflow_are_refused_not_clamped() {
        let mut ledger = Ledger::new();
        let issuance = ledger.issuance_wallet();
        let treasury = ledger.open(WalletKind::Treasury);
        ledger
            .mint(treasury, MAX_WALLET_CENTS, Reason::Genesis)
            .unwrap();
        assert!(matches!(
            ledger.mint(treasury, 1, Reason::Mint).unwrap_err(),
            LedgerError::BalanceCap { .. }
        ));
        assert_eq!(ledger.balance(treasury), MAX_WALLET_CENTS);
        assert_eq!(ledger.balance(issuance), -MAX_WALLET_CENTS);
        assert_eq!(
            checked_notional_cents(i64::MAX, 2).unwrap_err(),
            LedgerError::Overflow,
            "a notional that does not fit is refused, not saturated"
        );
        assert_eq!(checked_notional_cents(8_420, 100).unwrap(), 842_000);
    }

    #[test]
    fn a_synthetic_wallet_may_owe_and_the_debt_is_reported() {
        let (mut ledger, _, _, alice) = world();
        let synthetic = ledger.open(WalletKind::Synthetic);
        // Alice sells to liquidity nobody funded: the currency is real, and
        // so is the debt behind it.
        ledger
            .post(
                Draft::new(Reason::Sell)
                    .debit(synthetic, 30_000)
                    .credit(alice, 30_000),
            )
            .unwrap();
        assert_eq!(ledger.balance(synthetic), -30_000);
        assert_eq!(ledger.synthetic_debt_cents(), 30_000);
        assert!(
            ledger.check().is_empty(),
            "conservation still holds: {:?}",
            ledger.check()
        );
        assert_eq!(
            ledger.circulating_cents(),
            i128::from(ledger.supply().outstanding_cents())
        );
    }

    #[test]
    fn check_reports_a_ledger_that_has_been_tampered_with() {
        let (mut ledger, _, treasury, _) = world();
        ledger.wallets.get_mut(&treasury).unwrap().balance_cents += 1;
        assert!(!ledger.is_valid());
        ledger.wallets.get_mut(&treasury).unwrap().balance_cents -= 1;
        ledger.wallets.get_mut(&treasury).unwrap().reserved_cents = MAX_WALLET_CENTS;
        assert!(!ledger.is_valid());
    }

    #[test]
    fn unknown_wallets_and_currency_mismatches_are_refused() {
        let (mut ledger, _, treasury, _) = world();
        let other = ledger.open_in(WalletKind::Player, CurrencyId(7));
        assert!(matches!(
            ledger
                .post(
                    Draft::new(Reason::Transfer)
                        .debit(treasury, 1)
                        .credit(WalletId(9_999), 1)
                )
                .unwrap_err(),
            LedgerError::UnknownWallet(_)
        ));
        assert!(matches!(
            ledger
                .post(
                    Draft::new(Reason::Transfer)
                        .debit(treasury, 1)
                        .credit(other, 1)
                )
                .unwrap_err(),
            LedgerError::CurrencyMismatch { .. }
        ));
    }
}
