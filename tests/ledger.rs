//! Property tests for the currency ledger: whatever is thrown at it, the
//! books balance.
//!
//! These run on the `no_std` path (`cargo test --no-default-features
//! --tests`) as well as under `std`, because the ledger is the one part of
//! the economy a game may want to run in-process on a target without one.
//!
//! The invariant every property here comes back to is the same sum the
//! module documents: everything in circulation equals what was minted less
//! what was burned, and nothing but a mint or a burn can move that number.

use fehu::ledger::{
    Draft, Flows, Ledger, LedgerError, MAX_WALLET_CENTS, Reason, Supply, WalletId, WalletKind,
    WalletStatus, checked_notional_cents,
};
use proptest::prelude::*;

/// One thing that can be done to a ledger, drawn at random.
#[derive(Clone, Copy, Debug)]
enum Op {
    Mint {
        to: usize,
        cents: i64,
    },
    Burn {
        from: usize,
        cents: i64,
    },
    Transfer {
        from: usize,
        to: usize,
        cents: i64,
    },
    Fee {
        from: usize,
        cents: i64,
    },
    Reserve {
        wallet: usize,
        cents: i64,
    },
    Release {
        wallet: usize,
        cents: i64,
    },
    Freeze {
        wallet: usize,
    },
    Unfreeze {
        wallet: usize,
    },
    Close {
        wallet: usize,
    },
    /// A deliberately unbalanced draft: it must always be refused.
    Unbalanced {
        from: usize,
        to: usize,
        cents: i64,
    },
}

fn op_strategy(wallets: usize) -> impl Strategy<Value = Op> {
    let w = 0..wallets;
    // Amounts straddle the interesting edges: zero, negative, ordinary, and
    // large enough to bump the balance cap.
    let cents = prop_oneof![
        Just(0i64),
        Just(-1i64),
        1i64..1_000_000,
        Just(MAX_WALLET_CENTS),
        Just(MAX_WALLET_CENTS / 2),
        Just(i64::MAX),
    ];
    prop_oneof![
        (w.clone(), cents.clone()).prop_map(|(to, cents)| Op::Mint { to, cents }),
        (w.clone(), cents.clone()).prop_map(|(from, cents)| Op::Burn { from, cents }),
        (w.clone(), w.clone(), cents.clone()).prop_map(|(from, to, cents)| Op::Transfer {
            from,
            to,
            cents
        }),
        (w.clone(), cents.clone()).prop_map(|(from, cents)| Op::Fee { from, cents }),
        (w.clone(), cents.clone()).prop_map(|(wallet, cents)| Op::Reserve { wallet, cents }),
        (w.clone(), cents.clone()).prop_map(|(wallet, cents)| Op::Release { wallet, cents }),
        w.clone().prop_map(|wallet| Op::Freeze { wallet }),
        w.clone().prop_map(|wallet| Op::Unfreeze { wallet }),
        w.clone().prop_map(|wallet| Op::Close { wallet }),
        (w.clone(), w, cents).prop_map(|(from, to, cents)| Op::Unbalanced { from, to, cents }),
    ]
}

/// A world with one wallet of each kind that a transaction may name, plus
/// two players so transfers have somewhere to go.
fn world() -> (Ledger, Vec<WalletId>, WalletId) {
    let mut ledger = Ledger::new();
    let issuance = ledger.issuance_wallet();
    let wallets = vec![
        ledger.open(WalletKind::Treasury),
        ledger.open(WalletKind::Player),
        ledger.open(WalletKind::Player),
        ledger.open(WalletKind::Venue),
        ledger.open(WalletKind::Issuer),
        ledger.open(WalletKind::Npc),
        ledger.open(WalletKind::Budget),
        ledger.open(WalletKind::Synthetic),
    ];
    (ledger, wallets, issuance)
}

/// Apply `op`, ignoring whether it was allowed: the point of these tests is
/// that a refusal is as safe as a success.
fn apply(ledger: &mut Ledger, wallets: &[WalletId], op: Op) {
    match op {
        Op::Mint { to, cents } => {
            let _ = ledger.mint(wallets[to], cents, Reason::Mint);
        }
        Op::Burn { from, cents } => {
            let _ = ledger.burn(wallets[from], cents);
        }
        Op::Transfer { from, to, cents } => {
            let _ = ledger.post(
                Draft::new(Reason::Transfer)
                    .debit(wallets[from], cents)
                    .credit(wallets[to], cents),
            );
        }
        Op::Fee { from, cents } => {
            // Every fee lands in the venue wallet: nothing leaves the world
            // through one.
            let _ = ledger.post(
                Draft::new(Reason::Fee)
                    .debit(wallets[from], cents)
                    .credit(wallets[3], cents),
            );
        }
        Op::Reserve { wallet, cents } => {
            let _ = ledger.reserve(wallets[wallet], cents);
        }
        Op::Release { wallet, cents } => ledger.release(wallets[wallet], cents),
        Op::Freeze { wallet } => {
            let _ = ledger.freeze(wallets[wallet]);
        }
        Op::Unfreeze { wallet } => {
            let _ = ledger.unfreeze(wallets[wallet]);
        }
        Op::Close { wallet } => {
            let _ = ledger.close(wallets[wallet]);
        }
        Op::Unbalanced { from, to, cents } => {
            let _ = ledger.post(
                Draft::new(Reason::Transfer)
                    .debit(wallets[from], cents)
                    .credit(wallets[to], cents)
                    // The extra cent is its own posting: adding it to the
                    // credit would saturate at `i64::MAX` and quietly
                    // balance again.
                    .credit(wallets[to], 1),
            );
        }
    }
}

/// One wallet, as a refused operation must leave it: id, balance, what is
/// reserved out of that, and whether it may still move money.
type WalletState = (WalletId, i64, i64, WalletStatus);

/// Everything about a ledger that a refused operation must leave alone —
/// the flow meter included, because a refusal that was still counted would
/// be a movement an audit could see and a balance could not.
fn state(ledger: &Ledger) -> (Supply, u64, Flows, Vec<WalletState>) {
    (
        ledger.supply(),
        ledger.next_transaction_id(),
        *ledger.flows(),
        ledger
            .wallets()
            .map(|w| (w.id, w.balance_cents(), w.reserved_cents(), w.status))
            .collect(),
    )
}

proptest! {
    /// Whatever sequence of operations runs, the books balance: circulating
    /// currency equals minted minus burned, issuance mirrors it, nobody is
    /// overdrawn who may not be, and no reservation exceeds its balance.
    #[test]
    fn any_sequence_of_operations_leaves_the_ledger_conserved(
        ops in prop::collection::vec(op_strategy(8), 1..80)
    ) {
        let (mut ledger, wallets, issuance) = world();
        for op in ops {
            apply(&mut ledger, &wallets, op);
            let issues = ledger.check();
            prop_assert!(issues.is_empty(), "{issues:?} after {op:?}");
        }
        prop_assert_eq!(
            ledger.circulating_cents(),
            i128::from(ledger.supply().outstanding_cents())
        );
        prop_assert_eq!(
            ledger.balance(issuance),
            -ledger.supply().outstanding_cents()
        );
    }

    /// Only a mint or a burn moves the supply. Everything else — transfers,
    /// fees, reservations, freezes — leaves it exactly where it was.
    #[test]
    fn nothing_but_a_mint_or_a_burn_changes_the_supply(
        ops in prop::collection::vec(op_strategy(8), 1..80)
    ) {
        let (mut ledger, wallets, _) = world();
        for op in ops {
            let before = ledger.supply();
            apply(&mut ledger, &wallets, op);
            let after = ledger.supply();
            match op {
                Op::Mint { .. } => prop_assert!(
                    after.burned_cents == before.burned_cents
                        && after.minted_cents >= before.minted_cents,
                    "a mint may only add to what was minted"
                ),
                Op::Burn { .. } => prop_assert!(
                    after.minted_cents == before.minted_cents
                        && after.burned_cents >= before.burned_cents,
                    "a burn may only add to what was burned"
                ),
                other => prop_assert_eq!(before, after, "{:?} moved the supply", other),
            }
        }
    }

    /// The flow meter counts transactions, not attempts: every accepted one
    /// exactly once, and no refused one at all.
    ///
    /// The ledger hands transaction ids out in order and never reuses one, so
    /// "how many were accepted" is a number the meter can be checked against
    /// without keeping a second tally to compare it with.
    #[test]
    fn the_flow_meter_counts_every_accepted_transaction_and_no_refusal(
        ops in prop::collection::vec(op_strategy(8), 1..80)
    ) {
        let (mut ledger, wallets, _) = world();
        for op in ops {
            apply(&mut ledger, &wallets, op);
            prop_assert_eq!(
                ledger.flows().total().count,
                ledger.next_transaction_id() - 1,
                "meter and transaction ids disagree after {:?}", op
            );
        }
    }

    /// What the meter says was minted and burned is what the supply says.
    ///
    /// Mint, genesis and migration are the reasons that create currency and
    /// each moves exactly what it created; a burn destroys exactly what it
    /// moved. So the meter reproduces the supply from the other side — from
    /// the movements rather than the running totals — and the two agree.
    #[test]
    fn the_flow_meter_reproduces_the_supply(
        ops in prop::collection::vec(op_strategy(8), 1..80)
    ) {
        let (mut ledger, wallets, _) = world();
        for op in ops {
            apply(&mut ledger, &wallets, op);
        }
        let flows = ledger.flows();
        let created = [Reason::Genesis, Reason::Mint, Reason::Migration]
            .into_iter()
            .map(|r| flows.get(r).cents)
            .sum::<i64>();
        prop_assert_eq!(created, ledger.supply().minted_cents);
        prop_assert_eq!(flows.get(Reason::Burn).cents, ledger.supply().burned_cents);
        // And nothing that cannot touch the supply was counted as if it had.
        prop_assert_eq!(flows.get(Reason::Reward).count, 0);
        prop_assert_eq!(flows.get(Reason::Dividend).count, 0);
    }

    /// A refused operation is a no-op: not one balance, reservation, status
    /// or id may move when the ledger says no.
    #[test]
    fn a_refused_operation_changes_nothing_at_all(
        setup in prop::collection::vec(op_strategy(8), 0..30),
        op in op_strategy(8),
    ) {
        let (mut ledger, wallets, _) = world();
        for op in setup {
            apply(&mut ledger, &wallets, op);
        }
        let before = state(&ledger);
        let refused = match op {
            Op::Mint { to, cents } => ledger.mint(wallets[to], cents, Reason::Mint).err().is_some(),
            Op::Burn { from, cents } => ledger.burn(wallets[from], cents).err().is_some(),
            Op::Transfer { from, to, cents } => ledger
                .post(
                    Draft::new(Reason::Transfer)
                        .debit(wallets[from], cents)
                        .credit(wallets[to], cents),
                )
                .is_err(),
            Op::Reserve { wallet, cents } => ledger.reserve(wallets[wallet], cents).is_err(),
            Op::Freeze { wallet } => ledger.freeze(wallets[wallet]).is_err(),
            Op::Close { wallet } => ledger.close(wallets[wallet]).is_err(),
            Op::Unbalanced { from, to, cents } => ledger
                .post(
                    Draft::new(Reason::Transfer)
                        .debit(wallets[from], cents)
                        .credit(wallets[to], cents)
                        .credit(wallets[to], 1),
                )
                .is_err(),
            _ => return Ok(()),
        };
        if refused {
            prop_assert_eq!(before, state(&ledger));
        }
    }

    /// An unbalanced draft is refused whatever it names and whoever is
    /// solvent. There is no way to post one.
    #[test]
    fn an_unbalanced_draft_is_always_refused(
        from in 0usize..8,
        to in 0usize..8,
        cents in 1i64..1_000_000,
        extra in 1i64..1_000,
    ) {
        let (mut ledger, wallets, _) = world();
        ledger.mint(wallets[0], MAX_WALLET_CENTS / 2, Reason::Genesis).unwrap();
        let err = ledger
            .post(
                Draft::new(Reason::Transfer)
                    .debit(wallets[from], cents)
                    .credit(wallets[to], cents + extra),
            )
            .unwrap_err();
        prop_assert!(matches!(err, LedgerError::Unbalanced { .. }), "{err}");
    }

    /// Reservations bound spending exactly: a wallet can commit and spend its
    /// available balance and not one cent more, and the two never overlap.
    #[test]
    fn a_reservation_is_exactly_what_cannot_be_spent(
        funded in 1i64..1_000_000,
        reserved in 0i64..1_000_000,
    ) {
        let (mut ledger, wallets, _) = world();
        let (treasury, player) = (wallets[0], wallets[1]);
        ledger.mint(treasury, funded, Reason::Genesis).unwrap();
        ledger
            .post(Draft::new(Reason::Faucet).debit(treasury, funded).credit(player, funded))
            .unwrap();

        let held = reserved.min(funded);
        ledger.reserve(player, held).unwrap();
        prop_assert_eq!(ledger.available(player), funded - held);
        if reserved > funded {
            prop_assert!(ledger.reserve(player, reserved).is_err());
        }

        let available = ledger.available(player);
        prop_assert!(
            ledger
                .post(
                    Draft::new(Reason::Buy)
                        .debit(player, available + 1)
                        .credit(treasury, available + 1)
                )
                .is_err(),
            "a cent past available is a cent too far"
        );
        if available > 0 {
            ledger
                .post(
                    Draft::new(Reason::Buy)
                        .debit(player, available)
                        .credit(treasury, available),
                )
                .unwrap();
        }
        prop_assert_eq!(ledger.available(player), 0);
        prop_assert!(ledger.check().is_empty());
    }

    /// Settling a fill with the fee in the same transaction conserves
    /// currency for any price and quantity that fit: what the buyer pays is
    /// what the seller and the venue receive between them.
    #[test]
    fn a_fill_and_its_fee_conserve_currency(
        price_cents in 1i64..1_000_000,
        qty in 1u64..10_000,
        fee_bps in 0i64..1_000,
    ) {
        let (mut ledger, wallets, _) = world();
        let (treasury, buyer, seller, venue) = (wallets[0], wallets[1], wallets[2], wallets[3]);
        let value = checked_notional_cents(price_cents, qty).unwrap();
        let fee = i64::try_from(i128::from(value) * i128::from(fee_bps) / 10_000).unwrap();

        ledger.mint(treasury, MAX_WALLET_CENTS / 2, Reason::Genesis).unwrap();
        ledger
            .post(
                Draft::new(Reason::Faucet)
                    .debit(treasury, value + fee)
                    .credit(buyer, value + fee),
            )
            .unwrap();
        let before = ledger.supply();

        ledger
            .post(
                Draft::new(Reason::Buy)
                    .debit(buyer, value + fee)
                    .credit(seller, value)
                    .credit(venue, fee),
            )
            .unwrap();

        prop_assert_eq!(ledger.balance(buyer), 0);
        prop_assert_eq!(ledger.balance(seller), value);
        prop_assert_eq!(ledger.balance(venue), fee);
        prop_assert_eq!(ledger.supply(), before, "a fee is collected, not destroyed");
        prop_assert!(ledger.check().is_empty());
    }

    /// A closed wallet is empty by construction, so closing can never strand
    /// currency where nothing can reach it again.
    #[test]
    fn closing_can_never_strand_currency(
        ops in prop::collection::vec(op_strategy(8), 1..60)
    ) {
        let (mut ledger, wallets, _) = world();
        for op in ops {
            apply(&mut ledger, &wallets, op);
        }
        for wallet in ledger.wallets() {
            if wallet.status == WalletStatus::Closed {
                prop_assert_eq!(wallet.balance_cents(), 0, "wallet {:?}", wallet.id);
                prop_assert_eq!(wallet.reserved_cents(), 0, "wallet {:?}", wallet.id);
            }
        }
    }

    /// The notional is exact or it is refused; it never silently becomes a
    /// different number.
    #[test]
    fn a_notional_is_exact_or_refused(price_cents in any::<i64>(), qty in any::<u64>()) {
        let product = i128::from(price_cents) * i128::from(qty);
        match checked_notional_cents(price_cents, qty) {
            Ok(value) => prop_assert_eq!(i128::from(value), product),
            Err(e) => {
                prop_assert_eq!(e, LedgerError::Overflow);
                prop_assert!(i64::try_from(product).is_err());
            }
        }
    }
}
