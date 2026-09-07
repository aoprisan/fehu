//! Read-only checks across accounts, traders and exchange books.

use std::collections::BTreeMap;

use fehu::{Side, TraderId};
use serde::Serialize;

use crate::account::AccountId;
use crate::market::Market;

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

impl Market {
    /// Reconcile actual resting orders with cash/share reservations, ownership,
    /// outstanding supply and the retained cash ledgers without changing state.
    pub fn reconcile(&self) -> Reconciliation {
        let mut issues = Vec::new();
        let mut cash = BTreeMap::<AccountId, i128>::new();
        let mut sells = BTreeMap::<(TraderId, &str), u128>::new();
        let mut resting_orders_checked = 0;
        for symbol in &self.symbols {
            let ticker = symbol.info.symbol;
            let mut held = 0i128;
            let mut bids = 0u128;
            for trader in self.traders.values() {
                if let Some(position) = trader.positions.get(ticker) {
                    held += i128::from(position.qty);
                }
            }
            for order in symbol.exchange.book().orders() {
                let Some(id) = order.owner.trader() else {
                    continue;
                };
                resting_orders_checked += 1;
                let Some(trader) = self.traders.get(&id) else {
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
            if held < 0 || held as u128 + bids > u128::from(symbol.info.shares_outstanding) {
                issues.push(format!(
                    "{ticker} holdings and resting bids exceed outstanding supply or are negative"
                ));
            }
        }
        for (id, account) in &self.accounts {
            if *id != account.id || !self.users.contains_key(&account.user_id) {
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
        for (id, trader) in &self.traders {
            if *id != trader.id || !self.users.contains_key(&trader.user_id) {
                issues.push(format!(
                    "trader {} has inconsistent identity or missing user",
                    id.0
                ));
            }
            if self
                .accounts
                .get(&trader.account_id)
                .is_none_or(|a| a.user_id != trader.user_id)
            {
                issues.push(format!(
                    "trader {} account is missing or belongs to another user",
                    id.0
                ));
            }
            for (&ticker, position) in &trader.positions {
                if self.symbol(ticker).is_none() || position.qty < 0 {
                    issues.push(format!("trader {} has an invalid {ticker} position", id.0));
                }
            }
            for symbol in &self.symbols {
                let ticker = symbol.info.symbol;
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
                if self.symbol(ticker).is_none() {
                    issues.push(format!("trader {} reserves unknown symbol {ticker}", id.0));
                }
            }
        }
        Reconciliation {
            valid: issues.is_empty(),
            accounts_checked: self.accounts.len(),
            traders_checked: self.traders.len(),
            symbols_checked: self.symbols.len(),
            resting_orders_checked,
            issues,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{App, Options};
    use fehu::{Order, Owner};

    #[test]
    fn shared_account_reservations_are_summed_across_traders_and_symbols() {
        let app = App::new(Options {
            history_days: 0,
            warmup_hours: 0,
            ..Options::default()
        });
        let mut market = app.market();
        let first = market.sign_up(None, None, 10000, 0).unwrap();
        let account = market.traders[&first].account_id;
        let user = market.traders[&first].user_id;
        let second = market.create_trader(user, account, None, 0);
        for (index, id) in [(0, first), (1, second)] {
            market.symbols[index]
                .exchange
                .submit(Order::limit(Owner::Trader(id), Side::Buy, 1, 10))
                .unwrap();
        }
        market.accounts.get_mut(&account).unwrap().reserve(20);
        assert!(market.reconcile().valid, "{:?}", market.reconcile());
        market.accounts.get_mut(&account).unwrap().release(10);
        assert!(!market.reconcile().valid);
    }

    #[test]
    fn supply_and_missing_owners_are_reported_without_panicking() {
        let app = App::new(Options {
            history_days: 0,
            warmup_hours: 0,
            ..Options::default()
        });
        let mut market = app.market();
        let trader = market.sign_up(None, None, 10000, 0).unwrap();
        let account = market.traders[&trader].account_id;
        market.symbols[0]
            .exchange
            .submit(Order::limit(Owner::Trader(trader), Side::Buy, 1, 10))
            .unwrap();
        market.accounts.get_mut(&account).unwrap().reserve(10);
        market.symbols[0].info.shares_outstanding = 1;
        assert!(
            market
                .reconcile()
                .issues
                .iter()
                .any(|s| s.contains("outstanding supply"))
        );
        market.traders.remove(&trader);
        assert!(
            market
                .reconcile()
                .issues
                .iter()
                .any(|s| s.contains("missing trader"))
        );
    }
}
