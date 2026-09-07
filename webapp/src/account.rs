//! Users, accounts and the cash ledger.
//!
//! A **user** is the person; an **account** holds that person's money and the
//! ledger of every movement through it; a [`Trader`](crate::trading::Trader)
//! is the market-facing identity that trades on exactly one account.
//!
//! Money is always an integer number of cents — there is no floating point
//! anywhere in this module, formatting included. Every mutation is checked:
//! amounts must be positive, a balance can neither exceed
//! [`MAX_BALANCE_CENTS`] nor be spent below what an account has available,
//! and the reservations backing resting buy orders are held out of that
//! available balance until the order fills or is cancelled.

use std::collections::VecDeque;

use fehu::Side;
use serde::{Deserialize, Serialize};

/// Largest balance an account may hold: 10^15 cents, i.e. $10 trillion.
/// Small enough that a balance times a book-sized quantity still fits an
/// `i64`, large enough that no game will notice the ceiling.
pub const MAX_BALANCE_CENTS: i64 = 1_000_000_000_000_000;

/// Largest single deposit or withdrawal, in cents. Same cap as the balance.
pub const MAX_TRANSFER_CENTS: i64 = MAX_BALANCE_CENTS;

/// Identifies a user.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UserId(pub u64);

/// Identifies an account.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AccountId(pub u64);

/// The person behind one or more accounts.
#[derive(Clone, Debug, Serialize)]
pub struct User {
    pub id: UserId,
    pub name: String,
    pub email: Option<String>,
    pub created_at_ms: i64,
    /// Accounts opened for this user, oldest first.
    pub accounts: Vec<AccountId>,
}

/// Whether an account may trade and move money.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountStatus {
    /// Normal: deposits, withdrawals and orders all go through.
    #[default]
    Active,
    /// Suspended: money can still be paid in, but nothing leaves and no
    /// order is accepted.
    Frozen,
    /// Terminal: nothing moves in or out again.
    Closed,
}

impl AccountStatus {
    /// Orders may be placed.
    pub fn can_trade(self) -> bool {
        self == Self::Active
    }

    /// Money may be paid in.
    pub fn can_deposit(self) -> bool {
        self != Self::Closed
    }

    /// Money may be taken out.
    pub fn can_withdraw(self) -> bool {
        self == Self::Active
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Frozen => "frozen",
            Self::Closed => "closed",
        }
    }
}

/// Why a money movement was refused. Every amount is in cents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MoneyError {
    /// Money moves in positive amounts; zero and negative are rejected.
    NotPositive { amount_cents: i64 },
    /// Above the per-transaction cap.
    TooLarge { amount_cents: i64, max_cents: i64 },
    /// The deposit would push the balance past [`MAX_BALANCE_CENTS`].
    BalanceCap {
        balance_cents: i64,
        amount_cents: i64,
    },
    /// Not enough available cash (the balance minus what resting orders
    /// have reserved).
    Insufficient {
        needed_cents: i64,
        available_cents: i64,
    },
    /// The account's status forbids `action`.
    Status {
        status: AccountStatus,
        action: &'static str,
    },
    /// The account still has cash committed to resting orders.
    Reserved { reserved_cents: i64 },
}

impl std::fmt::Display for MoneyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            Self::NotPositive { amount_cents } => {
                write!(
                    f,
                    "amount must be a positive number of cents, got {amount_cents}"
                )
            }
            Self::TooLarge {
                amount_cents,
                max_cents,
            } => write!(
                f,
                "amount {} is above the {} limit",
                money(amount_cents),
                money(max_cents)
            ),
            Self::BalanceCap {
                balance_cents,
                amount_cents,
            } => write!(
                f,
                "{} on top of {} would exceed the {} balance limit",
                money(amount_cents),
                money(balance_cents),
                money(MAX_BALANCE_CENTS)
            ),
            Self::Insufficient {
                needed_cents,
                available_cents,
            } => write!(
                f,
                "insufficient funds: need {}, {} available",
                money(needed_cents),
                money(available_cents)
            ),
            Self::Status { status, action } => {
                write!(f, "account is {}: cannot {action}", status.label())
            }
            Self::Reserved { reserved_cents } => write!(
                f,
                "{} is reserved for resting orders: cancel them first",
                money(reserved_cents)
            ),
        }
    }
}

impl std::error::Error for MoneyError {}

/// What a ledger entry records.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LedgerKind {
    /// The account was opened, with its starting balance.
    Open,
    /// Money paid in.
    Deposit,
    /// Money taken out.
    Withdrawal,
    /// A buy settled: cash left the account.
    Buy,
    /// A sell settled: cash came in.
    Sell,
}

/// One movement of money, in the order it happened.
#[derive(Clone, Debug, Serialize)]
pub struct LedgerEntry {
    pub id: u64,
    pub ts_ms: i64,
    pub kind: LedgerKind,
    /// Signed: positive credits the account, negative debits it.
    pub amount_cents: i64,
    /// The balance after this entry was applied.
    pub balance_cents: i64,
    /// Set on trade settlements.
    pub symbol: Option<&'static str>,
    pub order_id: Option<u64>,
    pub memo: Option<String>,
}

/// A cash account: a balance, the part of it reserved for resting buy
/// orders, and the ledger of everything that moved.
#[derive(Clone, Debug)]
pub struct Account {
    pub id: AccountId,
    pub user_id: UserId,
    pub name: String,
    pub status: AccountStatus,
    pub opened_at_ms: i64,
    /// Paid in since the account was opened, the opening balance included.
    pub deposited_cents: i64,
    /// Taken out since the account was opened.
    pub withdrawn_cents: i64,
    /// Ledger entries written since the account was opened; the ledger
    /// itself keeps only the most recent `ledger_cap` of them.
    pub entries_total: u64,
    balance_cents: i64,
    reserved_cents: i64,
    ledger: VecDeque<LedgerEntry>,
    ledger_cap: usize,
}

impl Account {
    /// Open an account for `user_id` with `deposit_cents` (zero is fine).
    pub fn open(
        id: AccountId,
        user_id: UserId,
        name: String,
        deposit_cents: i64,
        ledger_cap: usize,
        now_ms: i64,
    ) -> Result<Self, MoneyError> {
        if deposit_cents < 0 {
            return Err(MoneyError::NotPositive {
                amount_cents: deposit_cents,
            });
        }
        check_cap(deposit_cents)?;
        let mut account = Self {
            id,
            user_id,
            name,
            status: AccountStatus::Active,
            opened_at_ms: now_ms,
            deposited_cents: deposit_cents,
            withdrawn_cents: 0,
            entries_total: 0,
            balance_cents: deposit_cents,
            reserved_cents: 0,
            ledger: VecDeque::new(),
            ledger_cap: ledger_cap.max(1),
        };
        account.write(LedgerKind::Open, deposit_cents, now_ms, None, None, None);
        Ok(account)
    }

    /// Everything the account holds.
    pub fn balance_cents(&self) -> i64 {
        self.balance_cents
    }

    /// The part of the balance committed to resting buy orders.
    pub fn reserved_cents(&self) -> i64 {
        self.reserved_cents
    }

    /// What can be spent or withdrawn right now.
    pub fn available_cents(&self) -> i64 {
        self.balance_cents.saturating_sub(self.reserved_cents)
    }

    /// The most recent `limit` ledger entries, newest first.
    pub fn ledger(&self, limit: usize) -> Vec<LedgerEntry> {
        self.ledger.iter().rev().take(limit).cloned().collect()
    }

    /// Pay `amount_cents` in.
    pub fn deposit(
        &mut self,
        amount_cents: i64,
        memo: Option<String>,
        now_ms: i64,
    ) -> Result<LedgerEntry, MoneyError> {
        if !self.status.can_deposit() {
            return Err(MoneyError::Status {
                status: self.status,
                action: "take a deposit",
            });
        }
        check_positive(amount_cents)?;
        let balance =
            self.balance_cents
                .checked_add(amount_cents)
                .ok_or(MoneyError::BalanceCap {
                    balance_cents: self.balance_cents,
                    amount_cents,
                })?;
        if balance > MAX_BALANCE_CENTS {
            return Err(MoneyError::BalanceCap {
                balance_cents: self.balance_cents,
                amount_cents,
            });
        }
        self.balance_cents = balance;
        self.deposited_cents = self.deposited_cents.saturating_add(amount_cents);
        Ok(self.write(LedgerKind::Deposit, amount_cents, now_ms, None, None, memo))
    }

    /// Take `amount_cents` out. Only the available balance can leave: cash
    /// reserved for resting orders stays put until they are cancelled.
    pub fn withdraw(
        &mut self,
        amount_cents: i64,
        memo: Option<String>,
        now_ms: i64,
    ) -> Result<LedgerEntry, MoneyError> {
        if !self.status.can_withdraw() {
            return Err(MoneyError::Status {
                status: self.status,
                action: "pay out a withdrawal",
            });
        }
        check_positive(amount_cents)?;
        let available = self.available_cents();
        if amount_cents > available {
            return Err(MoneyError::Insufficient {
                needed_cents: amount_cents,
                available_cents: available,
            });
        }
        self.balance_cents -= amount_cents;
        self.withdrawn_cents = self.withdrawn_cents.saturating_add(amount_cents);
        Ok(self.write(
            LedgerKind::Withdrawal,
            -amount_cents,
            now_ms,
            None,
            None,
            memo,
        ))
    }

    /// The account may place orders.
    pub fn check_tradable(&self) -> Result<(), MoneyError> {
        if self.status.can_trade() {
            Ok(())
        } else {
            Err(MoneyError::Status {
                status: self.status,
                action: "place orders",
            })
        }
    }

    /// Validate a debit of `amount_cents` before it reaches the book: the
    /// account must be tradable and hold that much available cash.
    pub fn authorise(&self, amount_cents: i64) -> Result<(), MoneyError> {
        self.check_tradable()?;
        if amount_cents < 0 {
            return Err(MoneyError::NotPositive { amount_cents });
        }
        let available = self.available_cents();
        if amount_cents > available {
            return Err(MoneyError::Insufficient {
                needed_cents: amount_cents,
                available_cents: available,
            });
        }
        Ok(())
    }

    /// Commit cash to a resting buy order. Reservations are not movements
    /// of money, so they are not written to the ledger.
    pub fn reserve(&mut self, amount_cents: i64) {
        self.reserved_cents = self
            .reserved_cents
            .saturating_add(amount_cents.max(0))
            .min(MAX_BALANCE_CENTS);
    }

    /// Give reserved cash back (the order filled, or was cancelled).
    pub fn release(&mut self, amount_cents: i64) {
        self.reserved_cents = self
            .reserved_cents
            .saturating_sub(amount_cents.max(0))
            .max(0);
    }

    /// Book a fill: a buy debits the account, a sell credits it. The order
    /// was authorised before it reached the book and a resting buy holds a
    /// reservation, so this cannot overdraw in practice; the arithmetic
    /// saturates rather than wrapping, and [`Account::issues`] reports it if
    /// it ever does.
    pub fn settle(
        &mut self,
        side: Side,
        amount_cents: i64,
        symbol: &'static str,
        order_id: u64,
        ts_ms: i64,
    ) -> LedgerEntry {
        let amount = amount_cents.max(0);
        let (kind, signed) = match side {
            Side::Buy => (LedgerKind::Buy, -amount),
            Side::Sell => (LedgerKind::Sell, amount),
        };
        self.balance_cents = self.balance_cents.saturating_add(signed);
        self.write(kind, signed, ts_ms, Some(symbol), Some(order_id), None)
    }

    /// Move the account to `status`. A closed account is terminal, and an
    /// account with cash committed to resting orders cannot be closed.
    pub fn set_status(&mut self, status: AccountStatus) -> Result<(), MoneyError> {
        if self.status == AccountStatus::Closed && status != AccountStatus::Closed {
            return Err(MoneyError::Status {
                status: self.status,
                action: "be reopened",
            });
        }
        if status == AccountStatus::Closed && self.reserved_cents > 0 {
            return Err(MoneyError::Reserved {
                reserved_cents: self.reserved_cents,
            });
        }
        self.status = status;
        Ok(())
    }

    /// The invariants that must hold for a healthy account. An account is
    /// valid when this is empty.
    pub fn issues(&self) -> Vec<String> {
        let mut issues = Vec::new();
        if self.balance_cents < 0 {
            issues.push(format!(
                "balance is negative ({})",
                money(self.balance_cents)
            ));
        }
        if self.balance_cents > MAX_BALANCE_CENTS {
            issues.push(format!(
                "balance {} is above the {} limit",
                money(self.balance_cents),
                money(MAX_BALANCE_CENTS)
            ));
        }
        if self.reserved_cents < 0 {
            issues.push(format!(
                "reserved is negative ({})",
                money(self.reserved_cents)
            ));
        }
        if self.reserved_cents > self.balance_cents {
            issues.push(format!(
                "{} is reserved against a balance of only {}",
                money(self.reserved_cents),
                money(self.balance_cents)
            ));
        }
        issues
    }

    /// Every invariant holds.
    pub fn is_valid(&self) -> bool {
        self.issues().is_empty()
    }

    /// Append an entry describing a balance change that has already been
    /// applied.
    fn write(
        &mut self,
        kind: LedgerKind,
        amount_cents: i64,
        ts_ms: i64,
        symbol: Option<&'static str>,
        order_id: Option<u64>,
        memo: Option<String>,
    ) -> LedgerEntry {
        self.entries_total += 1;
        let entry = LedgerEntry {
            id: self.entries_total,
            ts_ms,
            kind,
            amount_cents,
            balance_cents: self.balance_cents,
            symbol,
            order_id,
            memo,
        };
        if self.ledger.len() >= self.ledger_cap {
            self.ledger.pop_front();
        }
        self.ledger.push_back(entry.clone());
        entry
    }
}

/// `price_cents × qty`, saturating instead of overflowing. All integer:
/// the product is computed in `i128` and clamped back into `i64`.
pub fn notional_cents(price_cents: i64, qty: u64) -> i64 {
    let product = i128::from(price_cents) * i128::from(qty);
    i64::try_from(product).unwrap_or(if product.is_negative() {
        i64::MIN
    } else {
        i64::MAX
    })
}

/// Format cents for a human, by integer division only: `123456` → `1234.56`.
pub fn money(cents: i64) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    let abs = cents.unsigned_abs();
    format!("{sign}{}.{:02}", abs / 100, abs % 100)
}

fn check_positive(amount_cents: i64) -> Result<(), MoneyError> {
    if amount_cents <= 0 {
        return Err(MoneyError::NotPositive { amount_cents });
    }
    check_cap(amount_cents)
}

fn check_cap(amount_cents: i64) -> Result<(), MoneyError> {
    if amount_cents > MAX_TRANSFER_CENTS {
        return Err(MoneyError::TooLarge {
            amount_cents,
            max_cents: MAX_TRANSFER_CENTS,
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// JSON shapes

/// Body of `POST /api/users`.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct CreateUserRequest {
    pub name: Option<String>,
    pub email: Option<String>,
}

/// Body of `POST /api/users/{id}/accounts`.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct OpenAccountRequest {
    pub name: Option<String>,
    /// Opening balance; defaults to the server's `starting_cash_cents`.
    pub cash_cents: Option<i64>,
}

/// Body of `POST /api/accounts/{id}/deposit` and `.../withdraw`.
#[derive(Clone, Debug, Deserialize)]
pub struct TransferRequest {
    /// Cents to move. Must be a positive integer.
    pub amount_cents: i64,
    pub memo: Option<String>,
}

/// Body of `POST /api/accounts/{id}/status`.
#[derive(Clone, Copy, Debug, Deserialize)]
pub struct StatusRequest {
    pub status: AccountStatus,
}

/// `GET /api/users/{id}`.
#[derive(Clone, Debug, Serialize)]
pub struct UserDto {
    pub id: u64,
    pub name: String,
    pub email: Option<String>,
    pub created_at_ms: i64,
    pub accounts: Vec<u64>,
    pub traders: Vec<u64>,
    /// Every account's balance added up.
    pub balance_cents: i64,
    /// Shares owned across every symbol and every trader of the user.
    pub shares_owned: u64,
    /// Those shares at the reference prices.
    pub holdings_value_cents: i64,
}

/// `GET /api/accounts/{id}`.
#[derive(Clone, Debug, Serialize)]
pub struct AccountDto {
    pub id: u64,
    pub user_id: u64,
    pub name: String,
    pub status: AccountStatus,
    pub opened_at_ms: i64,
    pub balance_cents: i64,
    /// Held against resting buy orders.
    pub reserved_cents: i64,
    /// `balance − reserved`: what an order or a withdrawal can use.
    pub available_cents: i64,
    pub deposited_cents: i64,
    pub withdrawn_cents: i64,
    pub entries_total: u64,
    /// The trader that trades on this account, if one was created for it.
    pub trader_id: Option<u64>,
    /// Every invariant holds.
    pub valid: bool,
}

impl AccountDto {
    pub fn new(a: &Account, trader_id: Option<u64>) -> Self {
        Self {
            id: a.id.0,
            user_id: a.user_id.0,
            name: a.name.clone(),
            status: a.status,
            opened_at_ms: a.opened_at_ms,
            balance_cents: a.balance_cents(),
            reserved_cents: a.reserved_cents(),
            available_cents: a.available_cents(),
            deposited_cents: a.deposited_cents,
            withdrawn_cents: a.withdrawn_cents,
            entries_total: a.entries_total,
            trader_id,
            valid: a.is_valid(),
        }
    }
}

/// `GET /api/accounts/{id}/validate`: the account's invariants, checked.
#[derive(Clone, Debug, Serialize)]
pub struct AccountCheck {
    pub account_id: u64,
    pub status: AccountStatus,
    /// `issues` is empty.
    pub valid: bool,
    /// Every broken invariant, in plain words.
    pub issues: Vec<String>,
    pub balance_cents: i64,
    pub reserved_cents: i64,
    pub available_cents: i64,
    /// Orders are accepted right now.
    pub can_trade: bool,
    pub can_deposit: bool,
    pub can_withdraw: bool,
}

impl AccountCheck {
    pub fn new(a: &Account) -> Self {
        let issues = a.issues();
        Self {
            account_id: a.id.0,
            status: a.status,
            valid: issues.is_empty(),
            issues,
            balance_cents: a.balance_cents(),
            reserved_cents: a.reserved_cents(),
            available_cents: a.available_cents(),
            can_trade: a.status.can_trade(),
            can_deposit: a.status.can_deposit(),
            can_withdraw: a.status.can_withdraw(),
        }
    }
}

/// `GET /api/accounts/{id}/ledger`, and the response to a deposit or a
/// withdrawal: the account after the movement, plus the entries.
#[derive(Clone, Debug, Serialize)]
pub struct LedgerResponse {
    pub account: AccountDto,
    /// Newest first.
    pub entries: Vec<LedgerEntry>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(balance_cents: i64) -> Account {
        Account::open(
            AccountId(1),
            UserId(1),
            "main".into(),
            balance_cents,
            10,
            1_000,
        )
        .unwrap()
    }

    #[test]
    fn opening_writes_the_first_entry() {
        let a = account(250_000);
        assert_eq!(a.balance_cents(), 250_000);
        assert_eq!(a.available_cents(), 250_000);
        assert_eq!(a.deposited_cents, 250_000);
        let ledger = a.ledger(10);
        assert_eq!(ledger.len(), 1);
        assert_eq!(ledger[0].kind, LedgerKind::Open);
        assert_eq!(ledger[0].balance_cents, 250_000);
        assert!(a.is_valid());
        assert!(
            Account::open(AccountId(2), UserId(1), "x".into(), -1, 10, 0).is_err(),
            "a negative opening balance is refused"
        );
    }

    #[test]
    fn deposits_and_withdrawals_are_validated() {
        let mut a = account(1_000);
        assert_eq!(
            a.deposit(0, None, 1).unwrap_err(),
            MoneyError::NotPositive { amount_cents: 0 }
        );
        assert_eq!(
            a.deposit(-5, None, 1).unwrap_err(),
            MoneyError::NotPositive { amount_cents: -5 }
        );
        assert!(matches!(
            a.deposit(MAX_TRANSFER_CENTS + 1, None, 1).unwrap_err(),
            MoneyError::TooLarge { .. }
        ));
        let entry = a.deposit(2_500, Some("top-up".into()), 7).unwrap();
        assert_eq!(entry.amount_cents, 2_500);
        assert_eq!(entry.balance_cents, 3_500);
        assert_eq!(entry.memo.as_deref(), Some("top-up"));
        assert_eq!(a.balance_cents(), 3_500);
        assert_eq!(a.deposited_cents, 3_500);

        assert_eq!(
            a.withdraw(3_501, None, 8).unwrap_err(),
            MoneyError::Insufficient {
                needed_cents: 3_501,
                available_cents: 3_500,
            }
        );
        let entry = a.withdraw(500, None, 8).unwrap();
        assert_eq!(entry.amount_cents, -500);
        assert_eq!(a.balance_cents(), 3_000);
        assert_eq!(a.withdrawn_cents, 500);
        assert!(a.is_valid());
    }

    #[test]
    fn deposits_cannot_pass_the_balance_cap() {
        let mut a = account(MAX_BALANCE_CENTS);
        assert!(matches!(
            a.deposit(1, None, 1).unwrap_err(),
            MoneyError::BalanceCap { .. }
        ));
        assert_eq!(a.balance_cents(), MAX_BALANCE_CENTS);
    }

    #[test]
    fn reservations_hold_cash_back() {
        let mut a = account(10_000);
        a.reserve(4_000);
        assert_eq!(a.reserved_cents(), 4_000);
        assert_eq!(a.available_cents(), 6_000);
        a.authorise(6_000).unwrap();
        assert!(a.authorise(6_001).is_err());
        assert!(a.withdraw(6_001, None, 1).is_err());
        a.withdraw(6_000, None, 1).unwrap();
        assert_eq!(a.balance_cents(), 4_000);
        assert_eq!(a.available_cents(), 0);
        // The reserved buy fills: release, then settle.
        a.release(4_000);
        let entry = a.settle(Side::Buy, 4_000, "ACME", 42, 9);
        assert_eq!(entry.kind, LedgerKind::Buy);
        assert_eq!(entry.amount_cents, -4_000);
        assert_eq!(entry.symbol, Some("ACME"));
        assert_eq!(entry.order_id, Some(42));
        assert_eq!(a.balance_cents(), 0);
        assert!(a.is_valid());
        // Releasing more than is held cannot make the reservation negative.
        a.release(1_000);
        assert_eq!(a.reserved_cents(), 0);
    }

    #[test]
    fn settling_a_sell_credits_the_account() {
        let mut a = account(0);
        let entry = a.settle(Side::Sell, 12_345, "NBLA", 7, 3);
        assert_eq!(entry.kind, LedgerKind::Sell);
        assert_eq!(a.balance_cents(), 12_345);
        assert_eq!(a.ledger(10)[0].balance_cents, 12_345);
    }

    #[test]
    fn status_gates_money_and_trading() {
        let mut a = account(1_000);
        a.set_status(AccountStatus::Frozen).unwrap();
        assert!(a.check_tradable().is_err());
        assert!(a.authorise(1).is_err());
        assert!(a.withdraw(1, None, 1).is_err());
        // A frozen account still takes money in.
        a.deposit(1, None, 1).unwrap();

        a.set_status(AccountStatus::Active).unwrap();
        a.reserve(500);
        assert!(matches!(
            a.set_status(AccountStatus::Closed).unwrap_err(),
            MoneyError::Reserved { .. }
        ));
        a.release(500);
        a.set_status(AccountStatus::Closed).unwrap();
        assert!(a.deposit(1, None, 1).is_err());
        assert!(matches!(
            a.set_status(AccountStatus::Active).unwrap_err(),
            MoneyError::Status { .. }
        ));
    }

    #[test]
    fn the_ledger_is_capped_but_keeps_counting() {
        let mut a = account(0);
        for i in 1..=20 {
            a.deposit(100, None, i).unwrap();
        }
        let ledger = a.ledger(100);
        assert_eq!(ledger.len(), 10, "capped at ledger_cap");
        assert_eq!(ledger[0].id, 21, "newest first, opening entry included");
        assert_eq!(a.entries_total, 21);
        assert_eq!(a.balance_cents(), 2_000);
    }

    #[test]
    fn issues_report_broken_invariants() {
        let mut a = account(100);
        a.reserve(500);
        assert!(!a.is_valid());
        assert_eq!(a.issues().len(), 1);
        a.settle(Side::Sell, 1_000, "ACME", 1, 1);
        assert!(a.is_valid());
    }

    #[test]
    fn money_and_notional_are_integer_only() {
        assert_eq!(money(0), "0.00");
        assert_eq!(money(5), "0.05");
        assert_eq!(money(123_456), "1234.56");
        assert_eq!(money(-99), "-0.99");
        assert_eq!(notional_cents(8_420, 100), 842_000);
        assert_eq!(notional_cents(i64::MAX, 2), i64::MAX, "saturates");
    }
}
