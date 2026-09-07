//! Read-only checks across accounts, traders and exchange books.
//!
//! They run over a [`Save`]: the one consistent picture of the whole market
//! there is, taken in one job on the market actor
//! ([`crate::market::Market::snapshot`]) — or read from disk, which is how a
//! save file is validated before anything is built from it.

use std::collections::BTreeMap;

use fehu::{Side, TraderId};
use serde::Serialize;

use crate::account::{Account, AccountId};
use crate::save::{Save, SymbolSave};
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
    pub issues: Vec<String>,
}

/// Reconcile actual resting orders with cash/share reservations, ownership,
/// outstanding supply and the retained cash ledgers without changing state.
pub fn reconcile(save: &Save) -> Reconciliation {
    let symbols: &[SymbolSave] = &save.symbols;
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
        let outstanding = sym.info.as_ref().map(|i| u128::from(i.shares_outstanding));
        if held < 0 || outstanding.is_some_and(|out| held as u128 + bids > out) {
            issues.push(format!(
                "{ticker} holdings and resting bids exceed outstanding supply or are negative"
            ));
        }
    }
    for (id, account) in &accounts {
        if *id != account.id || !users.contains_key(&account.user_id.0) {
            issues.push(format!(
                "account {} has inconsistent identity or missing user",
                id.0
            ));
        }
        for issue in account.issues().into_iter().chain(account.ledger_issues()) {
            issues.push(format!("account {}: {issue}", id.0));
        }
        if i128::from(account.reserved_cents()) != cash.get(id).copied().unwrap_or(0) {
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
    Reconciliation {
        valid: issues.is_empty(),
        accounts_checked: accounts.len(),
        traders_checked: traders.len(),
        symbols_checked: symbols.len(),
        resting_orders_checked,
        issues,
    }
}

#[cfg(test)]
mod tests {
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
                let first = m.sign_up(None, None, 10000, 0).unwrap();
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
            .call(move |m| m.accounts.get_mut(&account).unwrap().reserve(20))
            .await
            .unwrap();
        let report = app.reconcile().await;
        assert!(report.valid, "{report:?}");
        app.market
            .call(move |m| m.accounts.get_mut(&account).unwrap().release(10))
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
                let trader = m.sign_up(None, None, 10000, 0).unwrap();
                (trader, m.traders[&trader].account_id)
            })
            .await
            .unwrap();
        app.listings().all()[0]
            .change(move |s| {
                s.exchange
                    .submit(Order::limit(Owner::Trader(trader), Side::Buy, 1, 10))
                    .unwrap();
                s.info.shares_outstanding = 1;
            })
            .await
            .unwrap();
        app.market
            .call(move |m| m.accounts.get_mut(&account).unwrap().reserve(10))
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
