//! Budgets and rewards: the game paying a player for something that
//! happened outside the market.
//!
//! A quest is finished, a boss is killed, a daily login is claimed. None of
//! that happens here — Fehu owns currency, inventory, jobs and markets, and
//! the game owns everything else — so what arrives is a *result*: this
//! player, this rule, this event id. The answer has to be currency in that
//! player's wallet, and it has to be currency that already existed.
//!
//! # A reward is paid, not printed
//!
//! A [`Budget`] is a wallet of its own kind
//! ([`WalletKind::Budget`](fehu::ledger::WalletKind::Budget)), funded out of
//! treasury like everything else. Paying a reward moves currency from that
//! wallet to the player's: one balanced transaction, supply untouched, and
//! `/api/supply` reads the same before and after. A budget that has run out
//! refuses the reward rather than making up the difference — which is the
//! whole reason a budget is a wallet instead of a number in a config file.
//! An operator tops it up; nothing else does.
//!
//! # The game's event id is the key
//!
//! An `Idempotency-Key` protects a *request*: a client that retries the same
//! HTTP call gets the same answer. It does not protect an *event*: a game
//! backend that crashes after paying, restarts, and re-derives the quest
//! result will happily send a second request, with a second key, for the
//! same kill. So a reward carries the game's own id for what happened, and
//! that id is remembered with the receipt it produced. The second request
//! gets the first receipt, marked [`RewardReceipt::duplicate`], and no
//! second payment.
//!
//! The index is bounded (`FEHU_REWARD_LOG`), like the idempotency index and
//! for the same reason, and saved with the market, because a snapshot may
//! fall between any two commands. A source id that has aged out of it would
//! be paid again — so the bound is how late a duplicate may arrive and still
//! be caught, and it is deliberately large.

use std::collections::{BTreeMap, VecDeque};

use serde::{Deserialize, Serialize};

use fehu::ledger::WalletId;

/// Budgets one world may hold.
pub const MAX_BUDGETS: usize = 64;

/// Reward rules one world may hold.
pub const MAX_RULES: usize = 256;

/// Source ids remembered by default, with the receipt each one produced.
pub const DEFAULT_REWARD_LOG: usize = 10_000;

/// The longest a rule id or a source id may be.
pub const MAX_ID_LEN: usize = 64;

/// A pool a reward may be paid out of.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Budget {
    /// The wallet itself. Its balance is the authority on what is left:
    /// nothing here caches it.
    pub wallet: WalletId,
    pub name: String,
    /// Wall-clock instant it was opened.
    pub created_at_ms: i64,
    /// What has been paid out of it, and how many times.
    pub paid_cents: i64,
    pub paid_count: u64,
}

/// What a named reward is worth, and which pool it comes from.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RewardRule {
    /// A short name, uppercased: `DAILY`, `BOSS-KILL`.
    pub id: String,
    /// Bumped every time an operator rewrites the rule.
    pub version: u32,
    /// The budget it is paid from.
    pub budget: WalletId,
    /// What one payment is worth, in cents.
    pub amount_cents: i64,
    /// What the rule is for, for whoever reads it.
    pub note: Option<String>,
    /// What has been paid under it.
    pub paid_cents: i64,
    pub paid_count: u64,
}

/// What a reward paid, and to whom.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RewardReceipt {
    /// The rule that priced it.
    pub rule: String,
    /// The game's own id for what happened.
    pub source: String,
    pub trader_id: u64,
    pub account_id: u64,
    /// The budget it came out of.
    pub budget: WalletId,
    pub amount_cents: i64,
    /// The balanced transaction that moved it.
    pub tx_id: u64,
    /// What the player's account holds now.
    pub balance_cents: i64,
    /// Wall-clock instant it was paid.
    pub at_ms: i64,
    /// This is the receipt of an earlier payment, returned to a request that
    /// named a source id already paid. Nothing moved for it.
    pub duplicate: bool,
}

/// Why a budget, a rule or a reward was refused.
#[derive(Clone, Debug)]
pub enum RewardError {
    /// The world holds as many budgets or rules as it will.
    Full(&'static str),
    /// No budget with that wallet id.
    UnknownBudget(u64),
    /// No rule by that name.
    UnknownRule(String),
    /// No such trader.
    UnknownTrader(u64),
    /// The rule or the budget as asked for does not make sense.
    Invalid(String),
    /// The budget has less left than the rule is worth.
    Exhausted {
        /// The rule's price.
        needed_cents: i64,
        /// What the budget has.
        available_cents: i64,
    },
    /// The ledger would not move the money.
    Money(crate::account::MoneyError),
}

impl From<crate::account::MoneyError> for RewardError {
    fn from(e: crate::account::MoneyError) -> Self {
        Self::Money(e)
    }
}

impl From<fehu::ledger::LedgerError> for RewardError {
    fn from(e: fehu::ledger::LedgerError) -> Self {
        Self::Money(crate::account::MoneyError::Ledger(e))
    }
}

impl std::fmt::Display for RewardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full(what) => write!(f, "the world holds as many {what} as it will"),
            Self::UnknownBudget(id) => write!(f, "no budget {id}"),
            Self::UnknownRule(id) => write!(f, "no reward rule {id}"),
            Self::UnknownTrader(id) => write!(f, "no trader {id}"),
            Self::Invalid(why) => write!(f, "{why}"),
            Self::Exhausted {
                needed_cents,
                available_cents,
            } => write!(
                f,
                "the budget has {available_cents} cent(s) left, and this reward is {needed_cents}"
            ),
            Self::Money(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for RewardError {}

/// Clean a rule or source id: trimmed, and short enough to keep.
///
/// # Errors
/// A message saying what was wrong with it.
pub fn clean_id(id: &str, what: &str) -> Result<String, RewardError> {
    let id = id.trim();
    if id.is_empty() || id.len() > MAX_ID_LEN {
        return Err(RewardError::Invalid(format!(
            "a {what} is 1 to {MAX_ID_LEN} characters"
        )));
    }
    if id.chars().any(char::is_control) {
        return Err(RewardError::Invalid(format!(
            "a {what} holds no control characters"
        )));
    }
    Ok(id.to_owned())
}

/// The budgets, the rules, and every source id already paid.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RewardBook {
    budgets: BTreeMap<u64, Budget>,
    rules: BTreeMap<String, RewardRule>,
    /// Source id → the receipt it produced.
    paid: BTreeMap<String, RewardReceipt>,
    /// Source ids in the order they were paid, for eviction.
    order: VecDeque<String>,
    /// How many are remembered. Not saved: it is an option of the world, not
    /// a fact about it.
    #[serde(skip, default = "default_cap")]
    cap: usize,
}

fn default_cap() -> usize {
    DEFAULT_REWARD_LOG
}

impl Default for RewardBook {
    fn default() -> Self {
        Self {
            budgets: BTreeMap::new(),
            rules: BTreeMap::new(),
            paid: BTreeMap::new(),
            order: VecDeque::new(),
            cap: DEFAULT_REWARD_LOG,
        }
    }
}

impl RewardBook {
    /// A book that remembers `cap` source ids.
    #[must_use]
    pub fn with_cap(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            ..Self::default()
        }
    }

    /// Set how many source ids are remembered.
    pub fn set_cap(&mut self, cap: usize) {
        self.cap = cap.max(1);
        self.evict();
    }

    /// File a budget that has already been opened and funded.
    ///
    /// # Errors
    /// [`RewardError::Full`] once the world holds [`MAX_BUDGETS`].
    pub fn add_budget(&mut self, budget: Budget) -> Result<&Budget, RewardError> {
        if !self.budgets.contains_key(&budget.wallet.0) && self.budgets.len() >= MAX_BUDGETS {
            return Err(RewardError::Full("budgets"));
        }
        let id = budget.wallet.0;
        self.budgets.insert(id, budget);
        Ok(&self.budgets[&id])
    }

    /// The budget with that wallet id.
    #[must_use]
    pub fn budget(&self, wallet: WalletId) -> Option<&Budget> {
        self.budgets.get(&wallet.0)
    }

    /// Every budget, ordered by wallet id.
    pub fn budgets(&self) -> impl Iterator<Item = &Budget> {
        self.budgets.values()
    }

    /// Write or replace a rule. Replacing bumps its version and keeps what
    /// it has already paid: that is a record of what happened, not a budget
    /// an operator can reset by rewriting the price.
    ///
    /// # Errors
    /// [`RewardError::UnknownBudget`] when no such budget is open,
    /// [`RewardError::Invalid`] for an amount below a cent,
    /// [`RewardError::Full`] once the world holds [`MAX_RULES`].
    pub fn set_rule(
        &mut self,
        id: String,
        budget: WalletId,
        amount_cents: i64,
        note: Option<String>,
    ) -> Result<&RewardRule, RewardError> {
        if !self.budgets.contains_key(&budget.0) {
            return Err(RewardError::UnknownBudget(budget.0));
        }
        if amount_cents <= 0 {
            return Err(RewardError::Invalid(
                "a reward is at least one cent: a rule that pays nothing is not a reward".into(),
            ));
        }
        if !self.rules.contains_key(&id) && self.rules.len() >= MAX_RULES {
            return Err(RewardError::Full("reward rules"));
        }
        let (version, paid_cents, paid_count) = self
            .rules
            .get(&id)
            .map_or((1, 0, 0), |r| (r.version + 1, r.paid_cents, r.paid_count));
        self.rules.insert(
            id.clone(),
            RewardRule {
                id: id.clone(),
                version,
                budget,
                amount_cents,
                note,
                paid_cents,
                paid_count,
            },
        );
        Ok(&self.rules[&id])
    }

    /// Take a rule out of the book. What it has already paid stays paid.
    pub fn remove_rule(&mut self, id: &str) -> Option<RewardRule> {
        self.rules.remove(id)
    }

    /// The rule by that name.
    #[must_use]
    pub fn rule(&self, id: &str) -> Option<&RewardRule> {
        self.rules.get(id)
    }

    /// Every rule, ordered by id.
    pub fn rules(&self) -> impl Iterator<Item = &RewardRule> {
        self.rules.values()
    }

    /// The receipt an earlier payment for `source` produced, if there was
    /// one.
    #[must_use]
    pub fn paid(&self, source: &str) -> Option<&RewardReceipt> {
        self.paid.get(source)
    }

    /// Record a payment: against the rule, against the budget, and against
    /// the source id that will not be paid twice.
    pub fn record(&mut self, receipt: &RewardReceipt) {
        if let Some(rule) = self.rules.get_mut(&receipt.rule) {
            rule.paid_cents = rule.paid_cents.saturating_add(receipt.amount_cents);
            rule.paid_count += 1;
        }
        if let Some(budget) = self.budgets.get_mut(&receipt.budget.0) {
            budget.paid_cents = budget.paid_cents.saturating_add(receipt.amount_cents);
            budget.paid_count += 1;
        }
        if self
            .paid
            .insert(receipt.source.clone(), receipt.clone())
            .is_none()
        {
            self.order.push_back(receipt.source.clone());
        }
        self.evict();
    }

    /// Source ids remembered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.paid.len()
    }

    /// Nothing has ever been paid.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.paid.is_empty()
    }

    fn evict(&mut self) {
        while self.order.len() > self.cap {
            if let Some(oldest) = self.order.pop_front() {
                self.paid.remove(&oldest);
            }
        }
    }
}

/// `GET /api/budgets`: what the world has set aside, and what it will pay.
#[derive(Debug, Serialize)]
pub struct BudgetsResponse {
    pub budgets: Vec<BudgetDto>,
    pub rules: Vec<RewardRule>,
}

/// One budget, as the API shows it: what it is and what is left in it.
#[derive(Clone, Debug, Serialize)]
pub struct BudgetDto {
    pub wallet: WalletId,
    pub name: String,
    pub created_at_ms: i64,
    /// What the wallet holds now. The authority on whether the next reward
    /// can be paid.
    pub balance_cents: i64,
    pub paid_cents: i64,
    pub paid_count: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget(id: u64) -> Budget {
        Budget {
            wallet: WalletId(id),
            name: format!("budget {id}"),
            created_at_ms: 0,
            paid_cents: 0,
            paid_count: 0,
        }
    }

    fn receipt(source: &str, amount_cents: i64) -> RewardReceipt {
        RewardReceipt {
            rule: "DAILY".into(),
            source: source.into(),
            trader_id: 1,
            account_id: 1,
            budget: WalletId(10),
            amount_cents,
            tx_id: 1,
            balance_cents: amount_cents,
            at_ms: 0,
            duplicate: false,
        }
    }

    #[test]
    fn a_rule_needs_a_budget_and_a_price() {
        let mut book = RewardBook::default();
        assert!(
            book.set_rule("DAILY".into(), WalletId(10), 100, None)
                .is_err(),
            "a rule paid out of nothing is not a rule"
        );
        book.add_budget(budget(10)).unwrap();
        assert!(
            book.set_rule("DAILY".into(), WalletId(10), 0, None)
                .is_err()
        );
        let rule = book
            .set_rule("DAILY".into(), WalletId(10), 100, None)
            .unwrap();
        assert_eq!(rule.version, 1);
    }

    #[test]
    fn rewriting_a_rule_keeps_what_it_has_paid() {
        let mut book = RewardBook::default();
        book.add_budget(budget(10)).unwrap();
        book.set_rule("DAILY".into(), WalletId(10), 100, None)
            .unwrap();
        book.record(&receipt("quest-1", 100));
        let rule = book
            .set_rule("DAILY".into(), WalletId(10), 250, None)
            .unwrap();
        assert_eq!(rule.version, 2);
        assert_eq!(rule.amount_cents, 250);
        assert_eq!(rule.paid_cents, 100, "history is not a budget line");
        assert_eq!(rule.paid_count, 1);
    }

    #[test]
    fn a_source_id_is_remembered_with_its_receipt() {
        let mut book = RewardBook::default();
        book.add_budget(budget(10)).unwrap();
        book.set_rule("DAILY".into(), WalletId(10), 100, None)
            .unwrap();
        assert!(book.paid("quest-1").is_none());
        book.record(&receipt("quest-1", 100));
        assert_eq!(book.paid("quest-1").unwrap().amount_cents, 100);
        assert_eq!(book.budget(WalletId(10)).unwrap().paid_count, 1);
    }

    #[test]
    fn the_index_is_bounded_oldest_first() {
        let mut book = RewardBook::with_cap(2);
        book.add_budget(budget(10)).unwrap();
        for i in 0..4 {
            book.record(&receipt(&format!("quest-{i}"), 100));
        }
        assert_eq!(book.len(), 2);
        assert!(book.paid("quest-0").is_none());
        assert!(book.paid("quest-3").is_some());
        assert_eq!(
            book.budget(WalletId(10)).unwrap().paid_count,
            4,
            "what was paid is still counted after the id is forgotten"
        );
    }

    #[test]
    fn an_id_is_trimmed_and_bounded() {
        assert_eq!(clean_id(" quest-1 ", "source id").unwrap(), "quest-1");
        assert!(clean_id("", "source id").is_err());
        assert!(clean_id(&"x".repeat(MAX_ID_LEN + 1), "source id").is_err());
        assert!(clean_id("a\nb", "source id").is_err());
    }
}
