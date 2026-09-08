//! Users, accounts and the per-account view of the cash ledger.
//!
//! A **user** is the person; an **account** is that person's place in the
//! world's currency ledger, and the record of every movement through it; a
//! [`Trader`](crate::trading::Trader) is the market-facing identity that
//! trades on exactly one account.
//!
//! # Where the money actually is
//!
//! An account does not hold a balance. Its money is a
//! [`Player`](fehu::ledger::WalletKind::Player) wallet in the one
//! [`Ledger`](fehu::ledger::Ledger) the market owns, and every movement
//! through it is a balanced transaction there — so currency cannot be
//! created by paying it in, or destroyed by taking it out, and the world's
//! books add up whatever the accounts do. What lives here is the account's
//! *identity* and its *history*: who owns it, which wallet is theirs, and
//! the capped run of [`LedgerEntry`] rows that says how the balance got where
//! it is.
//!
//! That is why nothing in this module moves money by itself. Every mutation
//! takes the ledger it must post to, so there is no path that writes an entry
//! without a transaction behind it, and none that posts a transaction without
//! writing the entry. [`Market`](crate::market::Market) owns both and is the
//! only caller.
//!
//! Money is always an integer number of cents — there is no floating point
//! anywhere in this module, formatting included.

use std::collections::VecDeque;

use fehu::Side;
use fehu::ledger::{Draft, Ledger, LedgerError, Reason, WalletId, WalletStatus};
use serde::{Deserialize, Serialize};

use crate::save::Symbol;

/// Largest balance an account may hold: 10^15 cents, i.e. $10 trillion.
///
/// The ledger's own limit, re-exported because it is what the API documents
/// and what the tests name.
pub use fehu::ledger::MAX_WALLET_CENTS as MAX_BALANCE_CENTS;

/// Largest single mint or burn, in cents. Same cap as the balance.
pub const MAX_TRANSFER_CENTS: i64 = MAX_BALANCE_CENTS;

/// Whether an account may trade and move money.
///
/// This is the status of the account's wallet, under the name the API has
/// always used for it. There is one status, not two that can disagree.
pub type AccountStatus = WalletStatus;

/// Identifies a user.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UserId(pub u64);

/// Identifies an account.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AccountId(pub u64);

/// The person behind one or more accounts.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct User {
    pub id: UserId,
    pub name: String,
    pub email: Option<String>,
    pub created_at_ms: i64,
    /// Accounts opened for this user, oldest first.
    pub accounts: Vec<AccountId>,
}

/// Why a money movement was refused. Every amount is in cents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MoneyError {
    /// Money moves in positive amounts; zero and negative are rejected.
    NotPositive { amount_cents: i64 },
    /// Above the per-transaction cap.
    TooLarge { amount_cents: i64, max_cents: i64 },
    /// The account has no wallet, or names one the ledger does not have.
    /// A bug rather than a request the caller got wrong.
    NoWallet { account: AccountId },
    /// The ledger refused the movement. It says why, and nothing moved.
    Ledger(LedgerError),
}

impl From<LedgerError> for MoneyError {
    fn from(e: LedgerError) -> Self {
        Self::Ledger(e)
    }
}

impl MoneyError {
    /// The caller asked for something the world would not allow, as opposed
    /// to something it could not afford. Used to pick a status code.
    pub fn is_forbidden(self) -> bool {
        matches!(
            self,
            Self::Ledger(LedgerError::Status { .. } | LedgerError::Closed(_))
        )
    }
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
            Self::NoWallet { account } => {
                write!(f, "account {} has no wallet in the ledger", account.0)
            }
            Self::Ledger(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for MoneyError {}

/// What a ledger entry records.
///
/// These are the account's-eye view of a [`Reason`](fehu::ledger::Reason):
/// what happened, in the words the API has always used. A deposit is a mint
/// and a withdrawal is a burn, because currency now has to come from and go
/// somewhere; both keep their names, since to the account holder they are
/// still money paid in and money taken out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LedgerKind {
    /// The account was opened, with its starting balance.
    Open,
    /// Money paid in: minted by an operator, or paid out of treasury by the
    /// faucet that funds a new account.
    Deposit,
    /// Money taken out, and burned.
    Withdrawal,
    /// A buy settled: cash left the account.
    Buy,
    /// A sell settled: cash came in.
    Sell,
    /// The venue's fee on a fill, or the rebate it paid for providing
    /// liquidity. Always its own entry: the tape stays the price and the
    /// ledger stays the money.
    Fee,
    /// A dividend paid on shares held when it was declared.
    Dividend,
    /// A delisting bought the holder out: the shares are gone and this is
    /// what they were worth.
    Delisting,
}

/// One movement of money, in the order it happened.
#[derive(Clone, Debug, Serialize, Deserialize)]
// `symbol` is one of the build's tickers, interned on the way in rather than
// borrowed from the input, so the derive needs no `'de: 'static`.
#[serde(bound(deserialize = ""))]
pub struct LedgerEntry {
    pub id: u64,
    pub ts_ms: i64,
    pub kind: LedgerKind,
    /// The ledger transaction this entry is one side of. Every entry has
    /// one, so any row here can be traced to the balanced set of postings
    /// that produced it.
    pub tx_id: u64,
    /// Signed: positive credits the account, negative debits it.
    pub amount_cents: i64,
    /// The balance after this entry was applied.
    pub balance_cents: i64,
    /// Set on trade settlements.
    #[serde(with = "crate::save::symbol_opt")]
    pub symbol: Option<Symbol>,
    pub order_id: Option<u64>,
    pub memo: Option<String>,
}

/// A cash account: whose it is, which wallet holds its money, and the ledger
/// of everything that moved through it.
///
/// It serialises whole, private fields included: the save file has to carry
/// the history, not a view of it. The balance is not here — it is in the
/// ledger, which the save file carries too.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Account {
    pub id: AccountId,
    pub user_id: UserId,
    pub name: String,
    /// The wallet this account's money lives in.
    pub wallet: WalletId,
    pub opened_at_ms: i64,
    /// Paid in since the account was opened, the opening balance included.
    pub deposited_cents: i64,
    /// Taken out since the account was opened.
    pub withdrawn_cents: i64,
    /// Ledger entries written since the account was opened; the ledger
    /// itself keeps only the most recent `ledger_cap` of them.
    pub entries_total: u64,
    ledger: VecDeque<LedgerEntry>,
    ledger_cap: usize,
}

impl Account {
    /// Open an account for `user_id` against `wallet`, with no money in it.
    ///
    /// An account is opened empty because opening one cannot be allowed to
    /// create currency. Funding it is a separate, balanced movement — see
    /// [`Market::fund_account`](crate::market::Market::fund_account).
    pub fn open(
        id: AccountId,
        user_id: UserId,
        name: String,
        wallet: WalletId,
        ledger_cap: usize,
        now_ms: i64,
    ) -> Self {
        let mut account = Self {
            id,
            user_id,
            name,
            wallet,
            opened_at_ms: now_ms,
            deposited_cents: 0,
            withdrawn_cents: 0,
            entries_total: 0,
            ledger: VecDeque::new(),
            ledger_cap: ledger_cap.max(1),
        };
        account.write(LedgerKind::Open, 0, 0, 0, now_ms, None, None, None);
        account
    }

    /// Everything the account holds.
    pub fn balance_cents(&self, ledger: &Ledger) -> i64 {
        ledger.balance(self.wallet)
    }

    /// The part of the balance committed to resting buy orders.
    pub fn reserved_cents(&self, ledger: &Ledger) -> i64 {
        ledger.reserved(self.wallet)
    }

    /// What can be spent or withdrawn right now.
    pub fn available_cents(&self, ledger: &Ledger) -> i64 {
        ledger.available(self.wallet)
    }

    /// Whether the account may trade and move money.
    pub fn status(&self, ledger: &Ledger) -> AccountStatus {
        ledger.status(self.wallet)
    }

    /// The most recent `limit` ledger entries, newest first.
    pub fn ledger(&self, limit: usize) -> Vec<LedgerEntry> {
        self.ledger.iter().rev().take(limit).cloned().collect()
    }

    /// The account may place orders.
    pub fn check_tradable(&self, ledger: &Ledger) -> Result<(), MoneyError> {
        let status = self.status(ledger);
        if status.can_debit() {
            Ok(())
        } else {
            Err(MoneyError::Ledger(LedgerError::Status {
                wallet: self.wallet,
                status,
                action: "place orders",
            }))
        }
    }

    /// Validate a debit of `amount_cents` before it reaches the book: the
    /// account must be tradable and hold that much available cash.
    pub fn authorise(&self, ledger: &Ledger, amount_cents: i64) -> Result<(), MoneyError> {
        self.check_tradable(ledger)?;
        if amount_cents < 0 {
            return Err(MoneyError::NotPositive { amount_cents });
        }
        let available = self.available_cents(ledger);
        if amount_cents > available {
            return Err(MoneyError::Ledger(LedgerError::Insufficient {
                wallet: self.wallet,
                needed_cents: amount_cents,
                available_cents: available,
            }));
        }
        Ok(())
    }

    /// Commit cash to a resting buy order. Reservations are not movements of
    /// money, so they are not written to the ledger and cannot fail for want
    /// of one; a reservation the wallet cannot back is simply not taken, and
    /// [`crate::reconcile`] is what notices if that ever matters.
    pub fn reserve(&self, ledger: &mut Ledger, amount_cents: i64) {
        let _ = ledger.reserve(self.wallet, amount_cents.max(0));
    }

    /// Give reserved cash back (the order filled, or was cancelled).
    pub fn release(&self, ledger: &mut Ledger, amount_cents: i64) {
        ledger.release(self.wallet, amount_cents.max(0));
    }

    /// Record a movement this account was one side of.
    ///
    /// The transaction has already been posted; this writes the account's
    /// view of it. Splitting the two is what lets one balanced transaction —
    /// a fill, its fee, and the counterparty's side — leave a row in each
    /// account it touched and none in the ones it did not.
    #[allow(clippy::too_many_arguments)]
    pub fn record(
        &mut self,
        ledger: &Ledger,
        kind: LedgerKind,
        tx_id: u64,
        amount_cents: i64,
        ts_ms: i64,
        symbol: Option<&'static str>,
        order_id: Option<u64>,
        memo: Option<String>,
    ) -> LedgerEntry {
        self.record_split(
            ledger,
            tx_id,
            ts_ms,
            symbol,
            order_id,
            memo,
            &[(kind, amount_cents)],
        )
        .pop()
        .expect("one row in, one row out")
    }

    /// Record several rows of the *same* transaction, in order.
    ///
    /// A fill and the fee on it are one transaction but two rows, because the
    /// tape stays the price and the ledger stays the money. That makes the
    /// running balance a question: the ledger only knows where the account
    /// ended up, and both rows would otherwise claim that same closing
    /// figure while carrying only part of the movement between them — which
    /// is exactly what [`Account::ledger_issues`] calls an inconsistent
    /// balance, and rightly.
    ///
    /// So the rows are laid out backwards from where the account actually
    /// ended up: the last one closes on the ledger's balance, and each
    /// earlier one on that less what came after it. The history then adds up
    /// forwards, row by row, to a number that is really there.
    #[allow(clippy::too_many_arguments)]
    pub fn record_split(
        &mut self,
        ledger: &Ledger,
        tx_id: u64,
        ts_ms: i64,
        symbol: Option<&'static str>,
        order_id: Option<u64>,
        memo: Option<String>,
        rows: &[(LedgerKind, i64)],
    ) -> Vec<LedgerEntry> {
        let closing = self.balance_cents(ledger);
        let total: i64 = rows
            .iter()
            .map(|(_, amount)| *amount)
            .fold(0, i64::saturating_add);
        let mut balance = closing.saturating_sub(total);
        let mut out = Vec::with_capacity(rows.len());
        for (kind, amount) in rows {
            balance = balance.saturating_add(*amount);
            out.push(self.write(
                *kind,
                tx_id,
                *amount,
                balance,
                ts_ms,
                symbol,
                order_id,
                memo.clone(),
            ));
        }
        out
    }

    /// The invariants that must hold for a healthy account. An account is
    /// valid when this is empty.
    ///
    /// The balance and reservation invariants belong to the ledger and are
    /// checked there ([`Ledger::check`](fehu::ledger::Ledger::check)); what
    /// is checked here is the account's link to it.
    pub fn issues(&self, ledger: &Ledger) -> Vec<String> {
        let mut issues = Vec::new();
        let Some(wallet) = ledger.wallet(self.wallet) else {
            issues.push(format!("wallet {} is not in the ledger", self.wallet.0));
            return issues;
        };
        if wallet.balance_cents() < 0 {
            issues.push(format!(
                "balance is negative ({})",
                money(wallet.balance_cents())
            ));
        }
        if wallet.reserved_cents() > wallet.balance_cents() {
            issues.push(format!(
                "{} is reserved against a balance of only {}",
                money(wallet.reserved_cents()),
                money(wallet.balance_cents())
            ));
        }
        issues
    }

    /// Check the retained ledger's arithmetic and its link to the current
    /// balance. Evicted history cannot be audited; its closing balance is
    /// the opening anchor of the retained window.
    pub fn ledger_issues(&self, ledger: &Ledger) -> Vec<String> {
        let mut issues = Vec::new();
        let Some(first) = self.ledger.front() else {
            issues.push("ledger is empty".into());
            return issues;
        };
        let mut balance = i128::from(first.balance_cents) - i128::from(first.amount_cents);
        if first.id == 1 && balance != 0 {
            issues.push("opening ledger balance is not zero".into());
        }
        let mut previous_id = first.id.checked_sub(1);
        for entry in &self.ledger {
            if previous_id.and_then(|id| id.checked_add(1)) != Some(entry.id) {
                issues.push("ledger entry ids are not consecutive".into());
            }
            balance += i128::from(entry.amount_cents);
            if balance != i128::from(entry.balance_cents) {
                issues.push(format!(
                    "ledger entry {} has an inconsistent balance",
                    entry.id
                ));
            }
            previous_id = Some(entry.id);
        }
        if previous_id != Some(self.entries_total) {
            issues.push("ledger entry count does not match its last id".into());
        }
        if balance != i128::from(self.balance_cents(ledger)) {
            issues.push("ledger does not reconcile to the account balance".into());
        }
        issues
    }

    /// Every invariant holds.
    pub fn is_valid(&self, ledger: &Ledger) -> bool {
        self.issues(ledger).is_empty()
    }

    /// Append an entry describing a balance change that has already been
    /// applied.
    #[allow(clippy::too_many_arguments)]
    fn write(
        &mut self,
        kind: LedgerKind,
        tx_id: u64,
        amount_cents: i64,
        balance_cents: i64,
        ts_ms: i64,
        symbol: Option<&'static str>,
        order_id: Option<u64>,
        memo: Option<String>,
    ) -> LedgerEntry {
        self.entries_total += 1;
        match kind {
            LedgerKind::Deposit => {
                self.deposited_cents = self.deposited_cents.saturating_add(amount_cents.max(0));
            }
            LedgerKind::Withdrawal => {
                self.withdrawn_cents = self.withdrawn_cents.saturating_add((-amount_cents).max(0));
            }
            _ => {}
        }
        let entry = LedgerEntry {
            id: self.entries_total,
            ts_ms,
            kind,
            tx_id,
            amount_cents,
            balance_cents,
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

/// One side of a settlement, ready to be posted and recorded.
///
/// A fill moves currency between two parties and the venue in a single
/// balanced transaction, but leaves a row in each account that took part.
/// This is what one of those rows will say.
#[derive(Clone, Copy, Debug)]
pub struct Booking {
    /// The account whose history gets the row.
    pub account: AccountId,
    /// What it did to that account.
    pub kind: LedgerKind,
    /// Signed cents, the way the ledger is.
    pub amount_cents: i64,
}

/// `price_cents × qty`, saturating instead of overflowing.
///
/// Kept for the display and reporting paths — an order's running notional, a
/// market capitalisation — where a number that is merely very large is better
/// than none. **Not** for anything authoritative: settlement uses
/// [`fehu::ledger::checked_notional_cents`], which refuses a product that
/// does not fit rather than quietly returning a different one.
pub fn notional_cents(price_cents: i64, qty: u64) -> i64 {
    let product = i128::from(price_cents) * i128::from(qty);
    i64::try_from(product).unwrap_or(if product.is_negative() {
        i64::MIN
    } else {
        i64::MAX
    })
}

/// The draft behind one side of a fill: what the trader pays or receives,
/// and what the venue takes out of it.
///
/// `fee_cents` is signed the way the ledger is — negative is charged to the
/// trader, positive is a rebate paid to them — so a maker rebate and a taker
/// fee are the same arithmetic with different signs.
pub fn settlement_draft(
    trader: WalletId,
    counterparty: WalletId,
    venue: WalletId,
    side: Side,
    value_cents: i64,
    fee_cents: i64,
    ts_ms: i64,
) -> Draft {
    let (reason, signed) = match side {
        Side::Buy => (Reason::Buy, -value_cents),
        Side::Sell => (Reason::Sell, value_cents),
    };
    // The trader's side, the counterparty's mirror of it, and the venue's
    // cut — one transaction, summing to zero however the fee is signed.
    Draft::new(reason)
        .at(u64::try_from(ts_ms).unwrap_or(0))
        .posting(trader, signed)
        .posting(counterparty, -signed)
        .posting(trader, fee_cents)
        .posting(venue, -fee_cents)
}

/// Format cents for a human, by integer division only: `123456` → `1234.56`.
pub fn money(cents: i64) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    let abs = cents.unsigned_abs();
    format!("{sign}{}.{:02}", abs / 100, abs % 100)
}

/// The amount is a positive number of cents no larger than the cap.
pub fn check_transfer(amount_cents: i64) -> Result<(), MoneyError> {
    if amount_cents <= 0 {
        return Err(MoneyError::NotPositive { amount_cents });
    }
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
    /// What the faucet should pay the new account out of treasury; defaults
    /// to the server's `starting_cash_cents`. It is a transfer, not a mint:
    /// opening an account moves currency that already exists, and a treasury
    /// that cannot cover it refuses rather than printing more.
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
    /// The key that proves a request speaks for this user, shown **once**:
    /// in the response that created them, and `null` everywhere after. Send
    /// it as `Authorization: Bearer <key>`.
    pub api_key: Option<String>,
}

/// `GET /api/accounts/{id}`.
#[derive(Clone, Debug, Serialize)]
pub struct AccountDto {
    pub id: u64,
    pub user_id: u64,
    pub name: String,
    pub status: AccountStatus,
    /// The wallet in the currency ledger this account's money lives in.
    /// Operator routes address wallets by this id.
    pub wallet_id: u64,
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
    pub fn new(a: &Account, ledger: &Ledger, trader_id: Option<u64>) -> Self {
        Self {
            id: a.id.0,
            user_id: a.user_id.0,
            name: a.name.clone(),
            status: a.status(ledger),
            wallet_id: a.wallet.0,
            opened_at_ms: a.opened_at_ms,
            balance_cents: a.balance_cents(ledger),
            reserved_cents: a.reserved_cents(ledger),
            available_cents: a.available_cents(ledger),
            deposited_cents: a.deposited_cents,
            withdrawn_cents: a.withdrawn_cents,
            entries_total: a.entries_total,
            trader_id,
            valid: a.is_valid(ledger),
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
    pub fn new(a: &Account, ledger: &Ledger) -> Self {
        let issues = a.issues(ledger);
        let status = a.status(ledger);
        Self {
            account_id: a.id.0,
            status,
            valid: issues.is_empty(),
            issues,
            balance_cents: a.balance_cents(ledger),
            reserved_cents: a.reserved_cents(ledger),
            available_cents: a.available_cents(ledger),
            can_trade: status.can_debit(),
            can_deposit: status.can_credit(),
            can_withdraw: status.can_debit(),
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
    use fehu::ledger::{Reason, WalletKind};

    /// A ledger with a funded treasury, and an account holding
    /// `balance_cents` paid to it out of that treasury.
    fn world(balance_cents: i64) -> (Ledger, Account, WalletId) {
        let mut ledger = Ledger::new();
        let treasury = ledger.open(WalletKind::Treasury);
        ledger
            .mint(treasury, MAX_BALANCE_CENTS / 2, Reason::Genesis)
            .unwrap();
        let wallet = ledger.open(WalletKind::Player);
        let mut account = Account::open(AccountId(1), UserId(1), "main".into(), wallet, 10, 1_000);
        if balance_cents > 0 {
            let tx = ledger
                .post(
                    Draft::new(Reason::Faucet)
                        .debit(treasury, balance_cents)
                        .credit(wallet, balance_cents),
                )
                .unwrap();
            account.record(
                &ledger,
                LedgerKind::Deposit,
                tx.id,
                balance_cents,
                1_000,
                None,
                None,
                None,
            );
        }
        (ledger, account, treasury)
    }

    #[test]
    fn opening_writes_the_first_entry_and_holds_nothing() {
        let (ledger, account, _) = world(0);
        assert_eq!(account.balance_cents(&ledger), 0);
        let entries = account.ledger(10);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, LedgerKind::Open);
        assert_eq!(entries[0].balance_cents, 0, "opening cannot create money");
        assert!(account.is_valid(&ledger));
        assert!(ledger.check().is_empty());
    }

    #[test]
    fn funding_an_account_moves_currency_rather_than_making_it() {
        let (ledger, account, treasury) = world(250_000);
        assert_eq!(account.balance_cents(&ledger), 250_000);
        assert_eq!(account.deposited_cents, 250_000);
        assert_eq!(ledger.balance(treasury), MAX_BALANCE_CENTS / 2 - 250_000);
        assert_eq!(
            ledger.supply().outstanding_cents(),
            MAX_BALANCE_CENTS / 2,
            "the faucet is not a mint"
        );
        assert!(ledger.check().is_empty());
    }

    #[test]
    fn reservations_hold_cash_back() {
        let (mut ledger, account, _) = world(10_000);
        account.reserve(&mut ledger, 4_000);
        assert_eq!(account.reserved_cents(&ledger), 4_000);
        assert_eq!(account.available_cents(&ledger), 6_000);
        account.authorise(&ledger, 6_000).unwrap();
        assert!(account.authorise(&ledger, 6_001).is_err());
        account.release(&mut ledger, 4_000);
        assert_eq!(account.available_cents(&ledger), 10_000);
        // Releasing more than is held cannot make the reservation negative.
        account.release(&mut ledger, 1_000);
        assert_eq!(account.reserved_cents(&ledger), 0);
        assert!(ledger.check().is_empty());
    }

    #[test]
    fn a_settlement_draft_balances_whichever_way_the_fee_points() {
        let mut ledger = Ledger::new();
        let treasury = ledger.open(WalletKind::Treasury);
        ledger.mint(treasury, 1_000_000, Reason::Genesis).unwrap();
        let buyer = ledger.open(WalletKind::Player);
        let seller = ledger.open(WalletKind::Player);
        let venue = ledger.open(WalletKind::Venue);
        ledger
            .post(
                Draft::new(Reason::Faucet)
                    .debit(treasury, 500_000)
                    .credit(buyer, 500_000),
            )
            .unwrap();

        // A taker pays the fee on top of what the seller receives.
        let draft = settlement_draft(buyer, seller, venue, Side::Buy, 100_000, -250, 1);
        assert_eq!(draft.imbalance_cents(), 0);
        ledger.post(draft).unwrap();
        assert_eq!(ledger.balance(buyer), 399_750);
        assert_eq!(ledger.balance(seller), 100_000);
        assert_eq!(ledger.balance(venue), 250);

        // A maker is paid out of the venue's takings by the same arithmetic.
        let draft = settlement_draft(seller, buyer, venue, Side::Sell, 10_000, 100, 2);
        assert_eq!(draft.imbalance_cents(), 0);
        ledger.post(draft).unwrap();
        assert_eq!(ledger.balance(venue), 150);
        assert!(ledger.check().is_empty(), "{:?}", ledger.check());
    }

    #[test]
    fn status_comes_from_the_wallet_and_gates_trading() {
        let (mut ledger, account, _) = world(1_000);
        ledger.freeze(account.wallet).unwrap();
        assert_eq!(account.status(&ledger), AccountStatus::Frozen);
        assert!(account.check_tradable(&ledger).is_err());
        assert!(account.authorise(&ledger, 1).is_err());
        ledger.unfreeze(account.wallet).unwrap();
        account.authorise(&ledger, 1).unwrap();
    }

    #[test]
    fn the_ledger_is_capped_but_keeps_counting() {
        let (mut ledger, mut account, treasury) = world(0);
        for i in 1..=20 {
            let tx = ledger
                .post(
                    Draft::new(Reason::Faucet)
                        .debit(treasury, 100)
                        .credit(account.wallet, 100),
                )
                .unwrap();
            account.record(
                &ledger,
                LedgerKind::Deposit,
                tx.id,
                100,
                i,
                None,
                None,
                None,
            );
        }
        let entries = account.ledger(100);
        assert_eq!(entries.len(), 10, "capped at ledger_cap");
        assert_eq!(entries[0].id, 21, "newest first, opening entry included");
        assert_eq!(account.entries_total, 21);
        assert_eq!(account.balance_cents(&ledger), 2_000);
        assert_eq!(account.deposited_cents, 2_000);
        assert!(account.ledger_issues(&ledger).is_empty());
    }

    #[test]
    fn ledger_audit_handles_eviction_and_detects_corruption() {
        let (mut ledger, mut account, treasury) = world(100);
        assert!(account.ledger_issues(&ledger).is_empty());
        for i in 1..=20 {
            let tx = ledger
                .post(
                    Draft::new(Reason::Faucet)
                        .debit(treasury, 100)
                        .credit(account.wallet, 100),
                )
                .unwrap();
            account.record(
                &ledger,
                LedgerKind::Deposit,
                tx.id,
                100,
                i,
                None,
                None,
                None,
            );
        }
        assert!(account.ledger_issues(&ledger).is_empty());
        let valid = account.clone();
        account.ledger.back_mut().unwrap().amount_cents += 1;
        assert!(!account.ledger_issues(&ledger).is_empty());
        account = valid.clone();
        account.ledger.back_mut().unwrap().id += 1;
        assert!(!account.ledger_issues(&ledger).is_empty());
        // And a balance that has drifted from the entries is caught too.
        account = valid;
        ledger.release(account.wallet, 0);
        let tx = ledger
            .post(
                Draft::new(Reason::Faucet)
                    .debit(treasury, 1)
                    .credit(account.wallet, 1),
            )
            .unwrap();
        assert!(tx.id > 0);
        assert!(!account.ledger_issues(&ledger).is_empty());
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

    #[test]
    fn transfers_are_bounded_before_they_reach_the_ledger() {
        assert!(matches!(
            check_transfer(0).unwrap_err(),
            MoneyError::NotPositive { .. }
        ));
        assert!(matches!(
            check_transfer(-1).unwrap_err(),
            MoneyError::NotPositive { .. }
        ));
        assert!(matches!(
            check_transfer(MAX_TRANSFER_CENTS + 1).unwrap_err(),
            MoneyError::TooLarge { .. }
        ));
        check_transfer(MAX_TRANSFER_CENTS).unwrap();
    }
}
