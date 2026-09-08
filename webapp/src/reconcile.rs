//! Read-only checks across accounts, traders and exchange books.
//!
//! They run over a [`Save`]: the one consistent picture of the whole market
//! there is, taken in one job on the market actor
//! ([`crate::market::Market::snapshot`]) — or read from disk, which is how a
//! save file is validated before anything is built from it.

use std::collections::BTreeMap;

use fehu::{Side, TraderId};
use serde::Serialize;

use fehu::ledger::Ledger;

use crate::account::{Account, AccountId};
use crate::save::{Save, SymbolSave};
use crate::symbol::AssetKind;
use crate::trading::Trader;

/// A consistent snapshot's reconciliation results. Ledger checks cover only
/// retained entries; synthetic liquidity has no independently held inventory.
#[derive(Debug, Serialize)]
pub struct Reconciliation {
    pub valid: bool,
    pub accounts_checked: usize,
    pub traders_checked: usize,
    pub symbols_checked: usize,
    pub resting_orders_checked: usize,
    /// Wallets in the currency ledger, the four the world always has
    /// included.
    pub wallets_checked: usize,
    /// Currency created since genesis.
    pub minted_cents: i64,
    /// Currency destroyed.
    pub burned_cents: i64,
    /// `minted − burned`: what every wallet but issuance must add up to, and
    /// does, or `issues` says so.
    pub outstanding_cents: i64,
    /// What the wallets actually hold, added up. Equal to `outstanding_cents`
    /// in a healthy world — that equality is the whole audit.
    pub circulating_cents: i128,
    /// What unfunded liquidity has put into players' hands beyond its float.
    /// Not an error: it is currency the world knows about and can point at.
    /// It goes to zero when both sides of every fill are funded.
    pub synthetic_debt_cents: i128,
    /// Jobs still in the furnace. Each one is a promise the world has taken
    /// payment for, so a job whose owner has gone is an issue rather than a
    /// row to drop.
    pub jobs_running: usize,
    /// Budget wallets rewards are paid from.
    pub budgets_checked: usize,
    pub issues: Vec<String>,
}

/// Reconcile actual resting orders with cash/share reservations, ownership,
/// outstanding supply and the retained cash ledgers without changing state.
///
/// Since the currency ledger arrived this also answers the question the whole
/// economy rests on: **does the money add up?** Everything the wallets hold
/// must equal what has been minted less what has been burned, and no route a
/// player can reach may move either number. That check is a sum, and it is
/// [`Ledger::check`] — run here over the same consistent snapshot as
/// everything else.
pub fn reconcile(save: &Save) -> Reconciliation {
    let symbols: &[SymbolSave] = &save.symbols;
    let ledger: &Ledger = &save.market.ledger;
    let symbol = |ticker: &str| {
        symbols
            .iter()
            .find(|s| s.symbol.eq_ignore_ascii_case(ticker))
    };
    let users: BTreeMap<u64, ()> = save.market.users.iter().map(|u| (u.id.0, ())).collect();
    let accounts: BTreeMap<AccountId, &Account> =
        save.market.accounts.iter().map(|a| (a.id, a)).collect();
    let traders: BTreeMap<TraderId, &Trader> =
        save.market.traders.iter().map(|t| (t.id, t)).collect();
    let mut issues = Vec::new();
    let mut cash = BTreeMap::<AccountId, i128>::new();
    let mut sells = BTreeMap::<(TraderId, &str), u128>::new();
    let mut resting_orders_checked = 0;
    for sym in symbols {
        let ticker: &str = &sym.symbol;
        let mut held = 0i128;
        let mut bids = 0u128;
        for trader in traders.values() {
            if let Some(position) = trader.positions.get(ticker) {
                held += i128::from(position.qty);
            }
        }
        for order in sym.exchange.book().orders() {
            let Some(id) = order.owner.trader() else {
                continue;
            };
            resting_orders_checked += 1;
            let Some(trader) = traders.get(&id) else {
                issues.push(format!(
                    "{ticker} order {} references missing trader {}",
                    order.id.0, id.0
                ));
                continue;
            };
            if order.remaining == 0 || order.outstanding() > order.qty {
                issues.push(format!(
                    "{ticker} order {} has invalid remaining quantity",
                    order.id.0
                ));
            }
            match order.side {
                Side::Buy => {
                    *cash.entry(trader.account_id).or_default() +=
                        i128::from(order.price_cents) * i128::from(order.outstanding());
                    bids += u128::from(order.outstanding());
                }
                Side::Sell => {
                    *sells.entry((id, ticker)).or_default() += u128::from(order.outstanding())
                }
            }
        }
        if held < 0 {
            issues.push(format!("{ticker} holdings are negative"));
        } else if let Some(info) = sym.info.as_ref() {
            let out = u128::from(info.units_outstanding());
            let held = held as u128;
            match &info.asset {
                // A good's units exist only because a command issued them,
                // and are only ever somewhere: what is issued and not yet
                // consumed is exactly what the holders hold. Resting bids do
                // not enter into it — nothing fills them but a holder.
                AssetKind::Good { .. } if held != out => issues.push(format!(
                    "{ticker} holdings ({held}) differ from units issued less consumed ({out})"
                )),
                // A stock quoted by synthetic liquidity can be bought from
                // the shares nobody holds, so the resting bids are spoken
                // for as well.
                AssetKind::Stock { .. } if sym.exchange.params().synthetic && held + bids > out => {
                    issues.push(format!(
                        "{ticker} holdings and resting bids exceed outstanding supply"
                    ));
                }
                AssetKind::Stock { .. } if held > out => {
                    issues.push(format!("{ticker} holdings exceed outstanding supply"))
                }
                _ => {}
            }
        }
    }
    for (id, account) in &accounts {
        if *id != account.id || !users.contains_key(&account.user_id.0) {
            issues.push(format!(
                "account {} has inconsistent identity or missing user",
                id.0
            ));
        }
        for issue in account
            .issues(ledger)
            .into_iter()
            .chain(account.ledger_issues(ledger))
        {
            issues.push(format!("account {}: {issue}", id.0));
        }
        if i128::from(account.reserved_cents(ledger)) != cash.get(id).copied().unwrap_or(0) {
            issues.push(format!(
                "account {} cash reservation differs from resting buys",
                id.0
            ));
        }
    }
    for (id, trader) in &traders {
        if *id != trader.id || !users.contains_key(&trader.user_id.0) {
            issues.push(format!(
                "trader {} has inconsistent identity or missing user",
                id.0
            ));
        }
        if accounts
            .get(&trader.account_id)
            .is_none_or(|a| a.user_id != trader.user_id)
        {
            issues.push(format!(
                "trader {} account is missing or belongs to another user",
                id.0
            ));
        }
        for (&ticker, position) in &trader.positions {
            if symbol(ticker).is_none() || position.qty < 0 {
                issues.push(format!("trader {} has an invalid {ticker} position", id.0));
            }
        }
        for sym in symbols {
            let ticker: &str = &sym.symbol;
            let reserved = trader.reserved_shares.get(ticker).copied().unwrap_or(0);
            if u128::from(reserved) != sells.get(&(*id, ticker)).copied().unwrap_or(0) {
                issues.push(format!(
                    "trader {} {ticker} share reservation differs from resting sells",
                    id.0
                ));
            }
            if reserved > trader.held_shares(ticker) {
                issues.push(format!(
                    "trader {} {ticker} reserved shares exceed holdings",
                    id.0
                ));
            }
        }
        for ticker in trader.reserved_shares.keys() {
            if symbol(ticker).is_none() {
                issues.push(format!("trader {} reserves unknown symbol {ticker}", id.0));
            }
        }
    }
    // The conservation sum, and the wallets behind it.
    issues.extend(ledger.check().into_iter().map(|i| format!("ledger: {i}")));
    // Every account's wallet is its own: two accounts sharing one would let
    // a balance be spent twice over and still add up.
    let mut wallets = BTreeMap::<fehu::ledger::WalletId, u64>::new();
    for account in accounts.values() {
        *wallets.entry(account.wallet).or_default() += 1;
    }
    for (wallet, count) in wallets {
        if count > 1 {
            issues.push(format!("wallet {} is shared by {count} accounts", wallet.0));
        }
    }
    // Jobs: a running one is a promise, so what it needs to be kept has to
    // still be there. What it will deliver is its own — a good delisted
    // under a running job delivers nothing, which is a loss and not a
    // discrepancy — so only the owner is checked.
    let mut jobs_running = 0;
    for job in &save.market.jobs {
        if job.status != crate::jobs::JobStatus::Running {
            continue;
        }
        jobs_running += 1;
        let owner = TraderId(job.trader_id);
        if !traders.contains_key(&owner) {
            issues.push(format!(
                "job {} is running for missing trader {}",
                job.id, job.trader_id
            ));
        }
        if !accounts.contains_key(&AccountId(job.account_id)) {
            issues.push(format!(
                "job {} names missing account {}",
                job.id, job.account_id
            ));
        }
    }
    // Budgets and the rules that spend them: a rule paying out of a wallet
    // that is not a budget is currency arriving from somewhere nobody set
    // aside.
    let mut budgets_checked = 0;
    for budget in save.market.rewards.budgets() {
        budgets_checked += 1;
        match ledger.wallet(budget.wallet) {
            None => issues.push(format!(
                "budget {:?} names wallet {} which the ledger does not have",
                budget.name, budget.wallet.0
            )),
            Some(w) if w.kind != fehu::ledger::WalletKind::Budget => issues.push(format!(
                "budget {:?} names wallet {}, which is a {} wallet",
                budget.name,
                budget.wallet.0,
                w.kind.label()
            )),
            Some(_) => {}
        }
    }
    for rule in save.market.rewards.rules() {
        if save.market.rewards.budget(rule.budget).is_none() {
            issues.push(format!(
                "reward rule {} pays out of budget {}, which is not open",
                rule.id, rule.budget.0
            ));
        }
    }
    let supply = ledger.supply();
    Reconciliation {
        valid: issues.is_empty(),
        accounts_checked: accounts.len(),
        traders_checked: traders.len(),
        symbols_checked: symbols.len(),
        resting_orders_checked,
        wallets_checked: ledger.len(),
        minted_cents: supply.minted_cents,
        burned_cents: supply.burned_cents,
        outstanding_cents: supply.outstanding_cents(),
        circulating_cents: ledger.circulating_cents(),
        synthetic_debt_cents: ledger.synthetic_debt_cents(),
        jobs_running,
        budgets_checked,
        issues,
    }
}

#[cfg(test)]
mod tests {
    use crate::market::Market;
    use crate::symbol::AssetKind;
    use crate::{App, Options};
    use fehu::{Order, Owner, Side};

    fn quiet() -> Options {
        Options {
            history_days: 0,
            warmup_hours: 0,
            ..Options::default()
        }
    }

    #[tokio::test]
    async fn shared_account_reservations_are_summed_across_traders_and_symbols() {
        let app = App::new(quiet());
        let (first, second, account) = app
            .market
            .call(|m| {
                let first = m
                    .sign_up(None, None, 10000, 0, "sha256:test".into())
                    .unwrap();
                let account = m.traders[&first].account_id;
                let user = m.traders[&first].user_id;
                let second = m.create_trader(user, account, None, 0);
                (first, second, account)
            })
            .await
            .unwrap();
        // Straight into the books, behind the market's back: the point is
        // to see whether reconciliation notices.
        let listings = app.listings();
        for (index, id) in [(0, first), (1, second)] {
            listings.all()[index]
                .change(move |s| {
                    s.exchange
                        .submit(Order::limit(Owner::Trader(id), Side::Buy, 1, 10))
                        .unwrap();
                })
                .await
                .unwrap();
        }
        app.market
            .call(move |m| {
                let Market {
                    accounts, ledger, ..
                } = m;
                accounts.get(&account).unwrap().reserve(ledger, 20);
            })
            .await
            .unwrap();
        let report = app.reconcile().await;
        assert!(report.valid, "{report:?}");
        app.market
            .call(move |m| {
                let Market {
                    accounts, ledger, ..
                } = m;
                accounts.get(&account).unwrap().release(ledger, 10);
            })
            .await
            .unwrap();
        assert!(!app.reconcile().await.valid);
    }

    #[tokio::test]
    async fn supply_and_missing_owners_are_reported_without_panicking() {
        let app = App::new(quiet());
        let (trader, account) = app
            .market
            .call(|m| {
                let trader = m
                    .sign_up(None, None, 10000, 0, "sha256:test".into())
                    .unwrap();
                (trader, m.traders[&trader].account_id)
            })
            .await
            .unwrap();
        app.listings().all()[0]
            .change(move |s| {
                s.exchange
                    .submit(Order::limit(Owner::Trader(trader), Side::Buy, 1, 10))
                    .unwrap();
                s.info.asset = AssetKind::stock(1);
            })
            .await
            .unwrap();
        app.market
            .call(move |m| {
                let Market {
                    accounts, ledger, ..
                } = m;
                accounts.get(&account).unwrap().reserve(ledger, 10);
            })
            .await
            .unwrap();
        assert!(
            app.reconcile()
                .await
                .issues
                .iter()
                .any(|s| s.contains("outstanding supply"))
        );
        app.market
            .call(move |m| m.traders.remove(&trader))
            .await
            .unwrap();
        assert!(
            app.reconcile()
                .await
                .issues
                .iter()
                .any(|s| s.contains("missing trader"))
        );
    }
}
