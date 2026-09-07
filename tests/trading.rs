//! The exchange: synthetic liquidity, matching, price impact and the
//! invariants that tie it to the bare simulator.

use core::time::Duration;
use fehu::{
    Config, Exchange, ImpactParams, Order, OrderError, OrderId, OrderStatus, Owner, Side,
    Simulator, TimeInForce, Trade, TraderId, TradingParams,
};

const T: TraderId = TraderId(7);
const OTHER: TraderId = TraderId(8);

fn exchange(seed: u64) -> Exchange {
    Exchange::new(Config::default(), TradingParams::default(), seed).unwrap()
}

#[test]
fn without_traders_the_reference_is_the_bare_simulator() {
    let mut ex = exchange(42);
    let mut sim = Simulator::new(Config::default(), 42).unwrap();
    for _ in 0..20_000 {
        let r = ex.step();
        let t = sim.step();
        assert_eq!(r.tick, t, "price and volume must match the bare simulator");
        assert_eq!(r.impact, 0.0);
        let tape: u64 = r.trades.iter().map(|x| x.qty).sum();
        assert_eq!(tape, t.volume, "every share of volume prints");
        assert!(r.trades.iter().all(|x| x.taker.owner == Owner::Synthetic));
    }
    assert_eq!(ex.simulator().snapshot(), sim.snapshot());
}

#[test]
fn ladder_is_quoted_around_the_reference_and_widens_with_vol() {
    let mut ex = exchange(1);
    let r = ex.reference_cents();
    let book = ex.book();
    let bids = book.depth(Side::Buy, 100);
    let asks = book.depth(Side::Sell, 100);
    assert_eq!(bids.len(), 10);
    assert_eq!(asks.len(), 10);
    assert!(bids[0].price_cents < r && r < asks[0].price_cents);
    // 5 bp half spread on $100 is 5 cents.
    assert_eq!(bids[0].price_cents, r - 5);
    assert_eq!(asks[0].price_cents, r + 5);
    assert_eq!(bids[1].price_cents, bids[0].price_cents - 5);
    // Sizes grow away from the touch (in expectation; check the far end).
    let near: u64 = bids[..3].iter().map(|l| l.qty).sum();
    let far: u64 = bids[7..].iter().map(|l| l.qty).sum();
    assert!(far > near, "near {near} far {far}");
    for _ in 0..100 {
        ex.step();
    }
    let calm_spread = {
        let b = ex.book();
        b.best_ask().unwrap() - b.best_bid().unwrap()
    };
    // A vol shock widens the spread.
    ex.simulator_mut()
        .push_event(fehu::Event {
            at: fehu::Timestamp(0),
            kind: fehu::EventKind::VolShift {
                delta: 1.2,
                half_life: Duration::from_secs(3600),
            },
        })
        .unwrap();
    ex.step();
    let panic_spread = {
        let b = ex.book();
        b.best_ask().unwrap() - b.best_bid().unwrap()
    };
    assert!(
        panic_spread >= 3 * calm_spread,
        "calm {calm_spread} panic {panic_spread}"
    );
}

#[test]
fn market_buy_pays_the_spread_and_moves_the_reference() {
    let mut ex = exchange(3);
    for _ in 0..10 {
        ex.step();
    }
    let r0 = ex.reference_cents();
    let ask = ex.book().best_ask().unwrap();
    // Roughly a tenth of a day's volume in one go.
    let qty = 250_000;
    let preview = ex.preview_market(Side::Buy, qty);
    let p = ex
        .submit(Order::market(Owner::Trader(T), Side::Buy, qty))
        .unwrap();
    assert_eq!(p.filled, preview.filled);
    assert_eq!(p.notional_cents(), preview.notional_cents);
    assert!(p.filled > 0);
    assert!(
        p.avg_price_cents().unwrap() >= ask as f64,
        "paid the spread"
    );
    assert_eq!(ex.pending_flow(), i64::try_from(p.filled).unwrap());
    let report = ex.step();
    assert!(report.impact > 0.0);
    let expected = 0.7 * 0.4 * (1.0f64 / 365.25).sqrt() * (p.filled as f64 / 2_595_769.0).sqrt();
    assert!(
        (report.impact - expected).abs() < 1e-6,
        "impact {} vs {expected}",
        report.impact
    );
    assert!(
        report.tick.price_cents as f64 > r0 as f64 * (1.0 + expected * 0.5),
        "reference did not move: {r0} -> {}",
        report.tick.price_cents
    );
    assert_eq!(ex.pending_flow(), 0);
}

/// A large start price keeps cent rounding out of the log-price comparisons;
/// one-minute ticks keep the long decay phases cheap.
fn precise() -> Config {
    Config {
        start_price_cents: 1_000_000_000,
        tick: Duration::from_secs(60),
        ..Config::default()
    }
}

#[test]
fn impact_is_exactly_a_decaying_shock_on_the_spread() {
    // Same seed with and without the trade: the simulator's RNG stream is
    // untouched by trading, and the OU spread is linear, so the treated path
    // differs from the control by J·e^{−θ(t−t0)} exactly.
    let params = TradingParams::default();
    let mut ex = Exchange::new(precise(), params, 5).unwrap();
    let mut control = Exchange::new(precise(), params, 5).unwrap();
    for _ in 0..10 {
        ex.step();
        control.step();
    }
    let buy = ex
        .submit(Order::market(Owner::Trader(T), Side::Buy, 20_000))
        .unwrap();
    assert_eq!(buy.status, OrderStatus::Filled);
    let rep = ex.step();
    control.step();
    let j = rep.impact;
    assert!(j > 0.0);
    let theta = 50.0;
    let year_secs = 365.25 * 86_400.0;
    for k in 0..5_000u64 {
        let a = ex.step().tick.price_cents as f64;
        let b = control.step().tick.price_cents as f64;
        // The jump lands at the start of its own tick, so it has already
        // decayed once by the time that tick prints.
        let expected = j * (-theta * 60.0 * ((k + 2) as f64) / year_secs).exp();
        let got = (a / b).ln();
        assert!(
            (got - expected).abs() < 1e-7,
            "tick {k}: diff {got} vs {expected}"
        );
    }
    // Selling it back costs the spread and undoes the impact.
    let sell = ex
        .submit(Order::market(Owner::Trader(T), Side::Sell, 20_000))
        .unwrap();
    assert_eq!(sell.status, OrderStatus::Filled);
    assert!(
        sell.notional_cents() < buy.notional_cents(),
        "bought for {} sold for {}",
        buy.notional_cents(),
        sell.notional_cents()
    );
    let rep = ex.step();
    control.step();
    assert!((rep.impact + j).abs() < 1e-12, "{} vs {j}", rep.impact);
    ex.advance(Duration::from_secs(60 * 86_400)).for_each(drop);
    control
        .advance(Duration::from_secs(60 * 86_400))
        .for_each(drop);
    let a = ex.simulator().snapshot();
    let b = control.simulator().snapshot();
    assert!((a.log_spread - b.log_spread).abs() < 1e-6);
    assert_eq!(a.fundamental_cents, b.fundamental_cents);
}

#[test]
fn permanent_impact_moves_the_fundamental() {
    let params = TradingParams {
        impact: ImpactParams {
            permanent_fraction: 1.0,
            ..ImpactParams::default()
        },
        ..TradingParams::default()
    };
    let mut ex = Exchange::new(precise(), params, 5).unwrap();
    let mut control = Exchange::new(precise(), params, 5).unwrap();
    ex.step();
    control.step();
    ex.submit(Order::market(Owner::Trader(T), Side::Buy, 20_000))
        .unwrap();
    let r = ex.step();
    control.step();
    assert!(r.impact > 0.0);
    ex.advance(Duration::from_secs(120 * 86_400)).for_each(drop);
    control
        .advance(Duration::from_secs(120 * 86_400))
        .for_each(drop);
    let a = ex.simulator().snapshot();
    let b = control.simulator().snapshot();
    let got = (a.fundamental_cents as f64 / b.fundamental_cents as f64).ln();
    assert!((got - r.impact).abs() < 1e-6, "{got} vs {}", r.impact);
    assert!((a.log_spread - b.log_spread).abs() < 1e-9);
}

#[test]
fn resting_orders_fill_when_the_market_comes_to_them() {
    let mut ex = exchange(9);
    ex.step();
    let r = ex.reference_cents();
    // A bid a little below the touch and an ask a little above.
    let bid = ex
        .submit(Order::limit(Owner::Trader(T), Side::Buy, r - 30, 500))
        .unwrap();
    let ask = ex
        .submit(Order::limit(Owner::Trader(T), Side::Sell, r + 30, 500))
        .unwrap();
    assert_eq!(bid.status, OrderStatus::Resting);
    assert_eq!(ask.status, OrderStatus::Resting);
    assert_eq!(ex.book().orders_of(Owner::Trader(T)).count(), 2);
    let mut fills: Vec<Trade> = Vec::new();
    for _ in 0..20_000 {
        let rep = ex.step();
        fills.extend(
            rep.trades
                .iter()
                .filter(|t| t.maker.owner == Owner::Trader(T)),
        );
        if ex.book().orders_of(Owner::Trader(T)).count() == 0 {
            break;
        }
    }
    let bought: u64 = fills
        .iter()
        .filter(|t| t.maker.order == bid.id)
        .map(|t| t.qty)
        .sum();
    let sold: u64 = fills
        .iter()
        .filter(|t| t.maker.order == ask.id)
        .map(|t| t.qty)
        .sum();
    assert_eq!(
        bought, 500,
        "bid should fill once the price falls through it"
    );
    assert_eq!(sold, 500, "ask should fill once the price rises through it");
    assert!(
        fills
            .iter()
            .all(|t| t.price_cents == r - 30 || t.price_cents == r + 30)
    );
    // Passive fills count as flow too: the exchange moved shares to the trader.
    let net: i64 = fills.iter().map(Trade::trader_flow).sum();
    assert_eq!(net, 0);
}

#[test]
fn traders_trade_with_each_other_without_moving_the_reference() {
    let mut ex = exchange(11);
    ex.step();
    let r = ex.reference_cents();
    // An order inside the spread rests; the other trader hits it.
    let rest = ex
        .submit(Order::limit(Owner::Trader(T), Side::Sell, r, 100))
        .unwrap();
    assert_eq!(rest.status, OrderStatus::Resting);
    let hit = ex
        .submit(Order::limit(Owner::Trader(OTHER), Side::Buy, r, 100))
        .unwrap();
    assert_eq!(hit.status, OrderStatus::Filled);
    assert_eq!(hit.trades[0].maker.order, rest.id);
    assert_eq!(hit.trades[0].price_cents, r);
    assert_eq!(hit.trades[0].signed_qty_for(T), -100);
    assert_eq!(hit.trades[0].signed_qty_for(OTHER), 100);
    assert_eq!(ex.pending_flow(), 0);
    let rep = ex.step();
    assert_eq!(rep.impact, 0.0);
    // The interim trade is part of the next tick's volume.
    let tape: u64 = rep.trades.iter().map(|t| t.qty).sum();
    assert_eq!(rep.tick.volume, tape + 100);
}

#[test]
fn cancel_and_ownership() {
    let mut ex = exchange(2);
    let r = ex.reference_cents();
    let o = ex
        .submit(Order::limit(Owner::Trader(T), Side::Buy, r - 100, 10))
        .unwrap();
    assert_eq!(ex.cancel(o.id, OTHER), Err(fehu::CancelError::NotOwner));
    assert_eq!(ex.cancel(o.id, T).unwrap().remaining, 10);
    assert_eq!(ex.cancel(o.id, T), Err(fehu::CancelError::Unknown));
    assert_eq!(
        ex.cancel(OrderId(1), T),
        Err(fehu::CancelError::NotOwner),
        "synthetic quotes are not cancellable"
    );
    assert_eq!(
        ex.submit(Order::market(Owner::Synthetic, Side::Buy, 1)),
        Err(fehu::OrderError::SyntheticOwner)
    );
    let ioc = ex
        .submit(Order::limit(Owner::Trader(T), Side::Buy, r - 100, 10).with_tif(TimeInForce::Ioc))
        .unwrap();
    assert_eq!(ioc.status, OrderStatus::Cancelled);
    assert!(ex.book().orders_of(Owner::Trader(T)).next().is_none());
}

#[test]
fn market_orders_stop_at_the_collar() {
    let mut ex = exchange(4);
    ex.step();
    let r = ex.reference_cents();
    // A trader ask far above the collar must not be hit by a market buy.
    ex.submit(Order::limit(
        Owner::Trader(OTHER),
        Side::Sell,
        r * 2,
        1_000_000,
    ))
    .unwrap();
    let p = ex
        .submit(Order::market(Owner::Trader(T), Side::Buy, 10_000_000))
        .unwrap();
    assert_eq!(p.status, OrderStatus::Cancelled);
    assert!(p.remaining > 0);
    assert!(p.trades.iter().all(|t| t.price_cents <= r * 105 / 100 + 1));
    assert!(p.trades.iter().all(|t| t.maker.owner == Owner::Synthetic));
}

#[test]
fn flow_direction_follows_the_return() {
    let mut ex = exchange(6);
    let mut agree = 0u64;
    let mut total = 0u64;
    let mut prev = ex.reference_cents();
    for _ in 0..20_000 {
        let rep = ex.step();
        let up = rep.tick.price_cents > prev;
        let down = rep.tick.price_cents < prev;
        prev = rep.tick.price_cents;
        for t in &rep.trades {
            if up || down {
                total += t.qty;
                if (t.taker_side == Side::Buy) == up {
                    agree += t.qty;
                }
            }
        }
    }
    let share = agree as f64 / total as f64;
    assert!(share > 0.7, "only {share:.2} of volume followed the return");
}

#[test]
fn same_inputs_same_outputs() {
    let run = || {
        let mut ex = exchange(77);
        let mut acc = Vec::new();
        for i in 0..2_000 {
            if i % 97 == 0 {
                let r = ex.reference_cents();
                let side = if i % 2 == 0 { Side::Buy } else { Side::Sell };
                ex.submit(Order::limit(Owner::Trader(T), side, r, 300))
                    .unwrap();
                ex.submit(Order::market(Owner::Trader(OTHER), side.opposite(), 5_000))
                    .unwrap();
            }
            let rep = ex.step();
            acc.push((rep.tick, rep.trades.len(), rep.impact.to_bits()));
        }
        acc
    };
    assert_eq!(run(), run());
}

#[cfg(feature = "serde")]
#[test]
fn exchange_round_trips_through_serde() {
    let mut ex = exchange(21);
    for _ in 0..500 {
        ex.step();
    }
    let r = ex.reference_cents();
    ex.submit(Order::limit(Owner::Trader(T), Side::Buy, r - 50, 1_000))
        .unwrap();
    ex.submit(Order::market(Owner::Trader(T), Side::Buy, 3_000))
        .unwrap();
    let json = serde_json::to_string(&ex).unwrap();
    let mut back: Exchange = serde_json::from_str(&json).unwrap();
    let bytes = postcard::to_allocvec(&ex).unwrap();
    let mut back2: Exchange = postcard::from_bytes(&bytes).unwrap();
    assert_eq!(back.book(), ex.book());
    assert_eq!(back.pending_flow(), ex.pending_flow());
    for _ in 0..2_000 {
        let a = ex.step();
        let b = back.step();
        let c = back2.step();
        assert_eq!(a, b);
        assert_eq!(a, c);
    }
    let mut bad: serde_json::Value = serde_json::from_str(&json).unwrap();
    bad["version"] = serde_json::json!(99);
    assert!(serde_json::from_value::<Exchange>(bad).is_err());
}

#[test]
fn a_tick_and_lot_are_enforced_on_both_sides_of_the_book() {
    let params = TradingParams {
        rules: fehu::MarketRules {
            tick_cents: 25,
            lot: 10,
        },
        ..TradingParams::default()
    };
    let mut ex = Exchange::new(Config::default(), params, 9).unwrap();

    // Every synthetic quote is on the grid and in whole lots.
    for level in ex.book().orders() {
        assert_eq!(
            level.price_cents % 25,
            0,
            "synthetic quote off the tick: {level:?}"
        );
        assert_eq!(
            level.qty % 10,
            0,
            "synthetic quote in an odd lot: {level:?}"
        );
    }
    let bid = ex.book().best_bid().unwrap();
    let ask = ex.book().best_ask().unwrap();
    assert!(bid < ask, "the tick must not collapse the spread");

    // A trader's order has to be on the grid too, and in whole lots.
    let off_tick = Order::limit(Owner::Trader(T), Side::Buy, bid - 1, 10);
    assert_eq!(ex.submit(off_tick), Err(OrderError::OffTick));
    let odd_lot = Order::limit(Owner::Trader(T), Side::Buy, bid - 25, 15);
    assert_eq!(ex.submit(odd_lot), Err(OrderError::OddLot));
    let good = Order::limit(Owner::Trader(T), Side::Buy, bid - 25, 20);
    assert_eq!(ex.submit(good).unwrap().status, OrderStatus::Resting);

    // Market orders are converted to a collar limit, which lands on the grid.
    let placement = ex
        .submit(Order::market(Owner::Trader(OTHER), Side::Buy, 10))
        .unwrap();
    assert!(placement.filled > 0, "{placement:?}");
    for trade in &placement.trades {
        assert_eq!(trade.price_cents % 25, 0, "print off the tick: {trade:?}");
    }

    // And the ladder stays on the grid as the price moves.
    for report in ex.advance(Duration::from_secs(120)) {
        for trade in &report.trades {
            assert_eq!(
                trade.price_cents % 25,
                0,
                "synthetic print off the tick: {trade:?}"
            );
            assert_eq!(
                trade.qty % 10,
                0,
                "synthetic print in an odd lot: {trade:?}"
            );
        }
    }
    for level in ex.book().orders().filter(|o| o.owner == Owner::Synthetic) {
        assert_eq!(level.price_cents % 25, 0);
        assert_eq!(level.qty % 10, 0);
    }
}

#[test]
fn an_iceberg_shows_a_slice_at_a_time_and_gives_up_its_place_for_the_next() {
    let mut ex = exchange(11);
    let bid = ex.book().best_bid().unwrap();
    let ask = ex.book().best_ask().unwrap();
    // Inside the spread, where the synthetic ladder has nothing: what shows
    // at this price is only what these two orders show.
    let price = bid + 1;
    assert!(price < ask, "the spread has room: {bid}..{ask}");

    // Somebody ordinary is already queued at that price, behind nothing.
    let ahead = ex
        .submit(Order::limit(Owner::Trader(OTHER), Side::Buy, price, 20))
        .unwrap();
    assert_eq!(ahead.status, OrderStatus::Resting);
    // 100 shares, shown 20 at a time.
    let iceberg = ex
        .submit_iceberg(Order::limit(Owner::Trader(T), Side::Buy, price, 100), 20)
        .unwrap();
    assert_eq!(iceberg.status, OrderStatus::Resting);
    assert_eq!(iceberg.remaining, 100, "all of it is still open");

    let resting = ex.book().get(iceberg.id).unwrap();
    assert_eq!(resting.remaining, 20, "only a slice is on show");
    assert_eq!(resting.hidden, 80);
    assert_eq!(resting.outstanding(), 100);
    let shown = |ex: &Exchange| {
        ex.book()
            .depth(Side::Buy, 50)
            .into_iter()
            .find(|l| l.price_cents == price)
            .map_or(0, |l| l.qty)
    };
    assert_eq!(shown(&ex), 40, "the book shows 20 + 20, not 120");

    // A sell of 30 takes the order ahead and half the slice: the iceberg is
    // not out of what it is showing, so it stays where it is.
    let hit = ex
        .submit(
            Order::limit(Owner::Trader(OTHER), Side::Sell, price, 30).with_tif(TimeInForce::Ioc),
        )
        .unwrap();
    assert_eq!(hit.filled, 30);
    let resting = ex.book().get(iceberg.id).unwrap();
    assert_eq!(resting.remaining, 10, "half the slice is left");
    assert_eq!(resting.hidden, 80, "and nothing new has been shown");

    // Ten more empties the slice, and the next one is posted at the back.
    let hit = ex
        .submit(
            Order::limit(Owner::Trader(OTHER), Side::Sell, price, 10).with_tif(TimeInForce::Ioc),
        )
        .unwrap();
    assert_eq!(hit.filled, 10);
    let resting = ex.book().get(iceberg.id).unwrap();
    assert_eq!(resting.remaining, 20, "refreshed to a full slice");
    assert_eq!(resting.hidden, 60);
    assert_eq!(resting.outstanding(), 80);
    assert_eq!(shown(&ex), 20);

    // A big sell walks through slice after slice until the whole thing is
    // gone, and the price level with it.
    let sweep = ex
        .submit(
            Order::limit(Owner::Trader(OTHER), Side::Sell, price, 80).with_tif(TimeInForce::Ioc),
        )
        .unwrap();
    assert_eq!(sweep.filled, 80, "the hidden size is still real liquidity");
    assert!(ex.book().get(iceberg.id).is_none(), "it is finished");
    assert_eq!(shown(&ex), 0);
    ex.book().validate_state().unwrap();
}

#[test]
fn an_iceberg_keeps_its_size_to_itself() {
    let mut ex = exchange(12);
    let bid = ex.book().best_bid().unwrap();
    let ask = ex.book().best_ask().unwrap();
    let price = bid + 1;
    assert!(price < ask, "the spread has room: {bid}..{ask}");
    let iceberg = ex
        .submit_iceberg(Order::limit(Owner::Trader(T), Side::Buy, price, 500), 10)
        .unwrap();

    // Neither the preview nor a fill-or-kill can see past the slice: the
    // hidden size is not liquidity anybody is entitled to count on.
    let preview = ex.book().preview(Side::Sell, 500, Some(price));
    assert_eq!(preview.filled, 10, "only the slice is visible: {preview:?}");
    let fok = ex
        .submit(
            Order::limit(Owner::Trader(OTHER), Side::Sell, price, 100).with_tif(TimeInForce::Fok),
        )
        .unwrap();
    assert_eq!(fok.status, OrderStatus::Cancelled);
    assert_eq!(fok.filled, 0, "a fill-or-kill measures what it can see");
    assert_eq!(ex.book().get(iceberg.id).unwrap().outstanding(), 500);

    // Cancelling gives back everything, shown and hidden alike.
    let cancelled = ex.cancel(iceberg.id, T).unwrap();
    assert_eq!(cancelled.outstanding(), 500);
    assert_eq!(cancelled.remaining, 10);
    assert_eq!(cancelled.hidden, 490);
    ex.book().validate_state().unwrap();
}

#[test]
fn an_iceberg_has_to_be_an_order_that_can_rest() {
    let mut ex = exchange(13);
    let price = ex.book().best_bid().unwrap() - 10;
    let order = Order::limit(Owner::Trader(T), Side::Buy, price, 100);
    for bad in [0, 101] {
        assert_eq!(
            ex.submit_iceberg(order, bad),
            Err(OrderError::BadDisplay),
            "display of {bad}"
        );
    }
    assert_eq!(
        ex.submit_iceberg(order.with_tif(TimeInForce::Ioc), 10),
        Err(OrderError::BadDisplay),
        "an order that cannot rest has nothing to hide"
    );
    assert_eq!(
        ex.submit_iceberg(Order::market(Owner::Trader(T), Side::Buy, 100), 10),
        Err(OrderError::BadDisplay)
    );
    assert_eq!(
        ex.submit_iceberg(Order::limit(Owner::Synthetic, Side::Buy, price, 100), 10),
        Err(OrderError::SyntheticOwner)
    );
    // The slice is in lots, like everything else.
    let mut lots = Exchange::new(
        Config::default(),
        TradingParams {
            rules: fehu::MarketRules {
                tick_cents: 1,
                lot: 10,
            },
            ..TradingParams::default()
        },
        13,
    )
    .unwrap();
    let price = lots.book().best_bid().unwrap() - 10;
    let order = Order::limit(Owner::Trader(T), Side::Buy, price, 100);
    assert_eq!(
        lots.submit_iceberg(order, 15),
        Err(OrderError::BadDisplay),
        "an odd-lot slice"
    );
    assert!(lots.submit_iceberg(order, 20).is_ok());
}

#[test]
fn the_default_rules_constrain_nothing() {
    let rules = fehu::MarketRules::default();
    assert!(rules.allows_price(8_431));
    assert!(rules.allows_qty(7));
    assert_eq!(rules.floor_price(8_431), 8_431);
    assert_eq!(rules.ceil_price(8_431), 8_431);
    assert_eq!(rules.floor_qty(7), 7);

    let rules = fehu::MarketRules {
        tick_cents: 25,
        lot: 10,
    };
    assert!(!rules.allows_price(8_431));
    assert!(rules.allows_price(8_425));
    assert_eq!(rules.floor_price(8_431), 8_425);
    assert_eq!(rules.ceil_price(8_431), 8_450);
    assert_eq!(rules.ceil_price(8_425), 8_425, "already on the grid");
    assert_eq!(rules.floor_price(3), 25, "a price is never rounded to zero");
    assert_eq!(rules.floor_qty(17), 10);
    assert_eq!(rules.floor_qty(7), 0, "the caller decides what that means");

    // Out of range is a configuration error, not a silent clamp.
    for bad in [
        fehu::MarketRules {
            tick_cents: 0,
            lot: 1,
        },
        fehu::MarketRules {
            tick_cents: 1,
            lot: 0,
        },
    ] {
        let params = TradingParams {
            rules: bad,
            ..TradingParams::default()
        };
        assert!(
            Exchange::new(Config::default(), params, 1).is_err(),
            "{bad:?}"
        );
    }
}

#[test]
fn advancing_without_matching_preserves_resting_orders_until_resync() {
    let mut ex = exchange(42);
    let bid = ex.book().best_bid().unwrap() - 1;
    let placement = ex
        .submit(Order::limit(Owner::Trader(T), Side::Buy, bid, 10))
        .unwrap();
    assert_eq!(placement.status, OrderStatus::Resting);
    let book = ex.book().clone();
    let at = ex.clock();
    ex.simulator_mut()
        .push_event(fehu::Event {
            at,
            kind: fehu::EventKind::Jump(-0.5),
        })
        .unwrap();
    let reports: Vec<_> = ex
        .advance_without_matching(Duration::from_secs(10))
        .collect();
    assert!(!reports.is_empty());
    assert!(
        reports
            .iter()
            .all(|r| r.trades.is_empty() && r.tick.volume == 0)
    );
    assert!(reports.last().unwrap().tick.price_cents < bid);
    assert_eq!(ex.book(), &book);
    assert!(ex.advance_without_matching(Duration::ZERO).next().is_none());
    let trades = ex.resync();
    assert!(trades.iter().any(|t| t.maker.order == placement.id));
    assert!(ex.book().get(placement.id).is_none());
    assert_eq!(ex.book().validate_state(), Ok(()));
}

#[test]
fn market_funding_counts_hidden_slices_without_exposing_them() {
    for side in [Side::Buy, Side::Sell] {
        let mut ex = exchange(42);
        let price = match side {
            Side::Buy => ex.book().best_bid().unwrap() + 1,
            Side::Sell => ex.book().best_ask().unwrap() - 1,
        };
        ex.submit_iceberg(
            Order::limit(Owner::Trader(T), side.opposite(), price, 1_000_000),
            1_000,
        )
        .unwrap();
        let visible = ex.preview_market(side, 1_000_000);
        let cost = ex.market_cost_cents(side, 1_000_000);
        assert!(visible.notional_cents < cost);
        let placement = ex
            .submit(Order::market(Owner::Trader(OTHER), side, 1_000_000))
            .unwrap();
        assert_eq!(placement.notional_cents(), cost);
        assert_eq!(placement.filled, 1_000_000);
    }
}
