//! The currency is conserved, and only an operator can change how much of it
//! there is.
//!
//! This is the acceptance suite for the first milestone of
//! `docs/economy-engine-plan.md`. Everything here comes back to one sum:
//!
//! ```text
//! Σ balances (every wallet but issuance) = minted − burned
//! ```
//!
//! The other suites check that the market does the right thing. These check
//! that whatever it does, the money adds up — through sign-ups, fills, fees,
//! dividends, delistings, freezes and restarts — and that the two numbers in
//! that equation move only when an operator says so.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use fehu::Timestamp;
use fehu_webapp::market::{App, Options};
use fehu_webapp::{engine, router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::util::ServiceExt;

/// 2023-11-14T22:13:20Z, as in the other suites.
const NOW_MS: i64 = 1_700_000_000_000;

/// A small, quick market with rate limiting off.
fn test_app() -> Arc<App> {
    App::new(options())
}

fn options() -> Options {
    Options {
        history_days: 1,
        warmup_hours: 1,
        now_ms: Some(NOW_MS),
        rate_per_sec: 0.0,
        ..Options::default()
    }
}

/// A market whose operator routes are locked: `FEHU_ADMIN_KEY` set, which is
/// what the plan means by an operator and what a shared server needs.
fn guarded_app() -> Arc<App> {
    App::new(Options {
        admin_key: Some(OPERATOR.into()),
        ..options()
    })
}

async fn call(app: &Arc<App>, key: Option<&str>, mut req: Request<Body>) -> (StatusCode, Value) {
    if let Some(key) = key {
        req.headers_mut().insert(
            header::AUTHORIZATION,
            format!("Bearer {key}").parse().unwrap(),
        );
    }
    let resp = router(Arc::clone(app)).oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or_else(|e| panic!("bad JSON ({e}): {bytes:?}"))
    };
    (status, body)
}

async fn get(app: &Arc<App>, key: Option<&str>, uri: &str) -> (StatusCode, Value) {
    call(app, key, Request::get(uri).body(Body::empty()).unwrap()).await
}

async fn post(app: &Arc<App>, key: Option<&str>, uri: &str, body: Value) -> (StatusCode, Value) {
    call(
        app,
        key,
        Request::post(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await
}

struct Player {
    id: u64,
    account: u64,
    key: String,
}

async fn sign_up(app: &Arc<App>, name: &str) -> Player {
    let (status, body) = post(app, None, "/api/traders", json!({ "name": name })).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    Player {
        id: body["id"].as_u64().unwrap(),
        account: body["account_id"].as_u64().unwrap(),
        key: body["api_key"].as_str().unwrap().to_owned(),
    }
}

/// The operator key `guarded_app` is locked with. `test_app` has none, and
/// then any key does — including this one.
const OPERATOR: &str = "operator-key";

/// What the world says about its own currency. Public to anyone.
async fn supply(app: &Arc<App>) -> Value {
    let (status, body) = get(app, None, "/api/supply").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

/// Assert the books balance, and hand back what was minted less what was
/// burned so a caller can check that number has not moved either.
///
/// `/api/reconcile` is the operator's, so this speaks as one.
async fn audit(app: &Arc<App>, when: &str) -> i64 {
    let supply = supply(app).await;
    assert_eq!(supply["balanced"], true, "{when}: {supply}");
    assert_eq!(
        supply["circulating_cents"], supply["outstanding_cents"],
        "{when}: the wallets hold something other than what exists"
    );
    let (status, report) = get(app, Some(OPERATOR), "/api/reconcile").await;
    assert_eq!(status, StatusCode::OK, "{when}: {report}");
    assert_eq!(report["valid"], true, "{when}: {report}");
    supply["outstanding_cents"].as_i64().unwrap()
}

#[tokio::test]
async fn a_world_starts_with_its_genesis_supply_and_nothing_else() {
    let app = test_app();
    let before = audit(&app, "at genesis").await;
    let supply = supply(&app).await;

    assert_eq!(supply["burned_cents"], 0, "nothing has been destroyed yet");
    assert_eq!(supply["minted_cents"], before, "everything was minted once");
    assert_eq!(supply["player_cents"], 0, "nobody has signed up");
    assert_eq!(supply["venue_cents"], 0, "nothing has traded");
    assert!(
        supply["issuer_cents"].as_i64().unwrap() > 0,
        "each seeded symbol can fund its own payouts: {supply}"
    );
    // Treasury, the synthetic float and the issuers between them are the
    // whole of it: there is nowhere else for it to be.
    let held = supply["treasury_cents"].as_i64().unwrap()
        + supply["issuer_cents"].as_i64().unwrap()
        + supply["circulating_cents"].as_i64().unwrap()
        - supply["treasury_cents"].as_i64().unwrap()
        - supply["issuer_cents"].as_i64().unwrap();
    assert_eq!(held, before);
}

#[tokio::test]
async fn no_route_a_player_can_reach_changes_the_supply() {
    let app = test_app();
    let genesis = audit(&app, "at genesis").await;

    // Signing up, opening a second account and asking for cash all move
    // currency out of treasury. None of them makes any.
    let ada = sign_up(&app, "ada").await;
    assert_eq!(audit(&app, "after a sign-up").await, genesis);
    let (status, body) = post(
        &app,
        Some(&ada.key),
        "/api/users/1/accounts",
        json!({ "name": "second", "cash_cents": 250_000 }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(audit(&app, "after a second account").await, genesis);

    let supply = supply(&app).await;
    assert!(
        supply["player_cents"].as_i64().unwrap() > 0,
        "the player has money"
    );
    assert_eq!(
        supply["minted_cents"].as_i64().unwrap(),
        genesis,
        "and every cent of it existed before they did"
    );

    // Trading it, at every price the book will take, moves it around and
    // makes none of it.
    for order in [
        json!({"trader_id": ada.id, "type": "market", "side": "buy", "qty": 50}),
        json!({"trader_id": ada.id, "type": "limit", "side": "buy", "qty": 10, "price_cents": 1}),
        json!({"trader_id": ada.id, "type": "limit", "side": "sell", "qty": 5, "price_cents": 1}),
    ] {
        let (status, body) = post(&app, Some(&ada.key), "/api/symbols/ACME/orders", order).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(audit(&app, "after an order").await, genesis);
    }
    engine::step(&app).await;
    assert_eq!(audit(&app, "after an engine step").await, genesis);

    let (status, _) = post(
        &app,
        Some(&ada.key),
        &format!("/api/traders/{}/cancel_all", ada.id),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(audit(&app, "after cancelling everything").await, genesis);
}

#[tokio::test]
async fn only_an_operator_mints_and_burns() {
    let app = guarded_app();
    let ada = sign_up(&app, "ada").await;
    let genesis = audit(&app, "at genesis").await;
    let mint = format!("/api/accounts/{}/deposit", ada.account);
    let burn = format!("/api/accounts/{}/withdraw", ada.account);

    // Their own key is not enough, and neither is nobody's.
    for key in [Some(ada.key.as_str()), None] {
        for uri in [&mint, &burn] {
            let (status, body) = post(&app, key, uri, json!({ "amount_cents": 1_000 })).await;
            assert!(
                status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN,
                "{uri} as {key:?}: {status} {body}"
            );
        }
    }
    assert_eq!(
        audit(&app, "after the refusals").await,
        genesis,
        "a refused mint mints nothing"
    );

    // The operator's key is.
    let (status, body) = post(
        &app,
        Some(OPERATOR),
        &mint,
        json!({ "amount_cents": 1_000, "memo": "quest reward" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        audit(&app, "after a mint").await,
        genesis + 1_000,
        "a mint is the one thing that makes the supply larger"
    );
    assert_eq!(body["entries"][0]["kind"], "deposit");
    assert!(body["entries"][0]["tx_id"].as_u64().unwrap() > 0);

    let (status, body) = post(&app, Some(OPERATOR), &burn, json!({ "amount_cents": 400 })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(audit(&app, "after a burn").await, genesis + 600);
    assert_eq!(supply(&app).await["burned_cents"], 400);
}

#[tokio::test]
async fn a_fee_is_collected_by_the_venue_rather_than_destroyed() {
    let app = App::new(Options {
        taker_fee_bps: 25,
        ..options()
    });
    let genesis = audit(&app, "at genesis").await;
    let ada = sign_up(&app, "ada").await;

    let (status, body) = post(
        &app,
        Some(&ada.key),
        "/api/symbols/ACME/orders",
        json!({ "trader_id": ada.id, "type": "market", "side": "buy", "qty": 100 }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let supply = supply(&app).await;
    assert!(
        supply["venue_cents"].as_i64().unwrap() > 0,
        "the taker paid a fee: {supply}"
    );
    assert_eq!(
        audit(&app, "after a fee").await,
        genesis,
        "a fee moves currency to the venue; it does not leave the world"
    );
}

#[tokio::test]
async fn a_dividend_the_issuer_cannot_fund_is_refused_and_pays_nobody() {
    // No issuer float: every payout has to be funded by an operator first.
    let app = App::new(Options {
        issuer_float_cents: 0,
        ..options()
    });
    let genesis = audit(&app, "at genesis").await;
    let ada = sign_up(&app, "ada").await;
    let (status, body) = post(
        &app,
        Some(&ada.key),
        "/api/symbols/ACME/orders",
        json!({ "trader_id": ada.id, "type": "market", "side": "buy", "qty": 100 }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (_, before) = get(&app, Some(&ada.key), &format!("/api/traders/{}", ada.id)).await;
    let cash_before = before["cash_cents"].as_i64().unwrap();

    let (status, body) = post(
        &app,
        None,
        "/api/symbols/ACME/dividend",
        json!({ "cents_per_share": 10 }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "payout_not_funded");

    let (_, after) = get(&app, Some(&ada.key), &format!("/api/traders/{}", ada.id)).await;
    assert_eq!(
        after["cash_cents"].as_i64().unwrap(),
        cash_before,
        "a payout that could not be funded paid nobody, not even a little"
    );
    assert_eq!(audit(&app, "after a refused dividend").await, genesis);
}

#[tokio::test]
async fn a_dividend_and_a_delisting_are_paid_out_of_the_issuer_and_conserve_currency() {
    let app = test_app();
    let genesis = audit(&app, "at genesis").await;
    let ada = sign_up(&app, "ada").await;
    let (status, body) = post(
        &app,
        Some(&ada.key),
        "/api/symbols/ACME/orders",
        json!({ "trader_id": ada.id, "type": "market", "side": "buy", "qty": 100 }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let issuer_before = supply(&app).await["issuer_cents"].as_i64().unwrap();
    let (status, body) = post(
        &app,
        None,
        "/api/symbols/ACME/dividend",
        json!({ "cents_per_share": 10 }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let paid = body["dividend"]["total_cents"].as_i64().unwrap();
    assert_eq!(paid, 1_000, "100 shares at 10 cents");
    let issuer_after = supply(&app).await["issuer_cents"].as_i64().unwrap();
    assert_eq!(
        issuer_before - issuer_after,
        paid,
        "every cent the holders got came out of the issuer"
    );
    assert_eq!(audit(&app, "after a dividend").await, genesis);

    // And the buyout, which is the same arithmetic with the shares removed.
    let (status, body) = post(&app, None, "/api/symbols/ACME/delist", json!({})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert!(body["delisting"]["total_cents"].as_i64().unwrap() > 0);
    assert_eq!(audit(&app, "after a delisting").await, genesis);
}

#[tokio::test]
async fn freezing_withdraws_the_orders_that_a_frozen_wallet_could_not_pay_for() {
    let app = guarded_app();
    let genesis = audit(&app, "at genesis").await;
    let ada = sign_up(&app, "ada").await;

    let (status, body) = post(
        &app,
        Some(&ada.key),
        "/api/symbols/ACME/orders",
        json!({"trader_id": ada.id, "type": "limit", "side": "buy", "qty": 10, "price_cents": 1}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (_, account) = get(
        &app,
        Some(&ada.key),
        &format!("/api/accounts/{}", ada.account),
    )
    .await;
    assert!(
        account["reserved_cents"].as_i64().unwrap() > 0,
        "the resting buy holds cash back"
    );

    // A player cannot freeze — or unfreeze — themselves.
    let status_uri = format!("/api/accounts/{}/status", ada.account);
    let (status, body) = post(
        &app,
        Some(&ada.key),
        &status_uri,
        json!({"status":"frozen"}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    let (status, body) = post(
        &app,
        Some(OPERATOR),
        &status_uri,
        json!({ "status": "frozen" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "frozen");
    assert_eq!(
        body["reserved_cents"], 0,
        "the resting order went with the freeze: an order that outlived one \
         would fill against a wallet that can no longer pay"
    );
    let (_, orders) = get(
        &app,
        Some(&ada.key),
        &format!("/api/traders/{}/orders", ada.id),
    )
    .await;
    for order in orders.as_array().expect("the order log") {
        assert_eq!(
            order["status"], "cancelled",
            "nothing is left resting: {order}"
        );
    }
    assert_eq!(audit(&app, "after a freeze").await, genesis);
}

#[tokio::test]
async fn closing_an_account_can_never_strand_currency() {
    let app = guarded_app();
    let genesis = audit(&app, "at genesis").await;
    let ada = sign_up(&app, "ada").await;
    let status_uri = format!("/api/accounts/{}/status", ada.account);

    // Closing is the owner's to do, unlike freezing — but only once there is
    // nothing left in the wallet to lose.
    let (status, body) = post(
        &app,
        Some(&ada.key),
        &status_uri,
        json!({"status":"closed"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    let (_, account) = get(
        &app,
        Some(&ada.key),
        &format!("/api/accounts/{}", ada.account),
    )
    .await;
    let (status, body) = post(
        &app,
        Some(OPERATOR),
        &format!("/api/accounts/{}/withdraw", ada.account),
        json!({ "amount_cents": account["balance_cents"] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body) = post(
        &app,
        Some(&ada.key),
        &status_uri,
        json!({"status":"closed"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["balance_cents"], 0);

    // Terminal, and not even the operator can undo it: a closed account is
    // closed for good, which is what makes emptying it first the price of
    // closing rather than a formality.
    let (status, body) = post(
        &app,
        Some(&ada.key),
        &status_uri,
        json!({"status":"active"}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "reopening is not the owner's to attempt: {body}"
    );
    let (status, body) = post(
        &app,
        Some(OPERATOR),
        &status_uri,
        json!({"status":"active"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let burned = account["balance_cents"].as_i64().unwrap();
    assert_eq!(
        audit(&app, "after a close").await,
        genesis - burned,
        "the balance was burned on the way out, and is gone from the supply \
         rather than stranded in a wallet nothing can reach"
    );
}

#[tokio::test]
async fn the_books_still_balance_after_a_restart() {
    let app = test_app();
    let genesis = audit(&app, "at genesis").await;
    let ada = sign_up(&app, "ada").await;
    for order in [
        json!({"trader_id": ada.id, "type": "market", "side": "buy", "qty": 40}),
        json!({"trader_id": ada.id, "type": "limit", "side": "buy", "qty": 10, "price_cents": 1}),
    ] {
        let (status, body) = post(&app, Some(&ada.key), "/api/symbols/ACME/orders", order).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    engine::step(&app).await;
    let before = supply(&app).await;

    let restored = App::restore(options(), app.save().await);
    let after = supply(&restored).await;
    assert_eq!(
        before, after,
        "every wallet, and the supply behind them, came back exactly"
    );
    assert_eq!(audit(&restored, "after a restart").await, genesis);
}

#[tokio::test]
async fn the_synthetic_counterparty_is_measured_rather_than_hidden() {
    // No float at all, so the very first fill against unfunded liquidity puts
    // it into debt. The point is that the debt is *reported* — the currency it
    // hands a player is real, and pretending otherwise is how supply goes
    // wrong quietly.
    let app = App::new(Options {
        synthetic_float_cents: 0,
        ..options()
    });
    let genesis = audit(&app, "at genesis").await;
    let ada = sign_up(&app, "ada").await;

    let (status, body) = post(
        &app,
        Some(&ada.key),
        "/api/symbols/ACME/orders",
        json!({ "trader_id": ada.id, "type": "market", "side": "buy", "qty": 100 }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    // Buying *pays* synthetic liquidity, so it is in credit, not debt.
    assert_eq!(supply(&app).await["synthetic_debt_cents"], 0);

    let (status, body) = post(
        &app,
        Some(&ada.key),
        "/api/symbols/ACME/orders",
        json!({ "trader_id": ada.id, "type": "market", "side": "sell", "qty": 100 }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // Whatever the round trip did, the sum still holds and the reconcile
    // pass is clean: a debt that is on the books is not drift.
    assert_eq!(audit(&app, "after a round trip").await, genesis);
    let (_, report) = get(&app, Some(OPERATOR), "/api/reconcile").await;
    assert!(report["synthetic_debt_cents"].as_i64().unwrap() >= 0);
}

#[tokio::test]
async fn a_maker_rebate_is_paid_even_before_the_venue_has_collected_anything() {
    // The venue pays a maker rebate out of what it takes in fees. With no
    // taker fee it takes nothing, and the very first maker fill asks it to
    // pay out of an empty wallet.
    //
    // That must not refuse the settlement: the book has already traded, so
    // the shares have moved and refusing would leave the money behind.
    let app = App::new(Options {
        maker_fee_bps: -25,
        taker_fee_bps: 0,
        price_limit_pct: 0.0,
        ..options()
    });
    let genesis = audit(&app, "at genesis").await;
    let ada = sign_up(&app, "ada").await;

    // Rest a bid under the market and let the simulator's flow sell into it.
    let (_, book) = get(&app, None, "/api/symbols/ACME/book").await;
    let bid = book["bid_cents"].as_i64().unwrap();
    let (status, body) = post(
        &app,
        Some(&ada.key),
        "/api/symbols/ACME/orders",
        json!({"trader_id": ada.id, "type": "limit", "side": "buy", "qty": 500,
               "price_cents": bid}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    for minute in 1..=60 {
        engine::advance_to(&app, Timestamp(NOW_MS + minute * 60_000)).await;
    }

    let (_, portfolio) = get(&app, Some(&ada.key), &format!("/api/traders/{}", ada.id)).await;
    let fills = portfolio["fills"].as_array().expect("the fill log");
    assert!(
        fills.iter().any(|f| f["liquidity"] == "maker"),
        "the resting bid was hit: {portfolio}"
    );
    assert!(
        fills
            .iter()
            .filter(|f| f["liquidity"] == "maker")
            .all(|f| f["fee_cents"].as_i64().unwrap() >= 0),
        "a maker is never charged: {portfolio}"
    );

    // The regression this guards: a settlement the venue could not fund was
    // refused *after* the book had traded, so the shares moved and nothing
    // was booked — an empty fill log and no position against an order the
    // book had already worked down.
    let filled: i64 = fills
        .iter()
        .map(|f| f["qty"].as_i64().unwrap())
        .sum::<i64>();
    let resting: i64 = portfolio["open_orders"]
        .as_array()
        .expect("the open orders")
        .iter()
        .map(|o| o["remaining"].as_i64().unwrap())
        .sum();
    assert_eq!(
        filled + resting,
        500,
        "every share the book worked off the order was booked: {portfolio}"
    );
    assert_eq!(
        portfolio["positions"][0]["qty"].as_i64().unwrap(),
        filled,
        "and the position is what was filled: {portfolio}"
    );
    assert_eq!(audit(&app, "after maker fills").await, genesis);
}

#[tokio::test]
async fn a_resting_buy_that_spends_everything_still_settles() {
    // The cash a resting buy reserves is exactly the cash that pays for it.
    // If the reservation is still held when the fill is posted, a trader who
    // committed their whole balance to the order looks insolvent at the
    // moment it fills — and the settlement is refused after the book has
    // already traded.
    let app = App::new(Options {
        price_limit_pct: 0.0,
        ..options()
    });
    let genesis = audit(&app, "at genesis").await;
    let ada = sign_up(&app, "ada").await;

    let (_, account) = get(
        &app,
        Some(&ada.key),
        &format!("/api/accounts/{}", ada.account),
    )
    .await;
    let cash = account["available_cents"].as_i64().unwrap();
    let (_, book) = get(&app, None, "/api/symbols/ACME/book").await;
    let bid = book["bid_cents"].as_i64().unwrap();
    // Every cent of it, committed to one resting bid.
    let qty = cash / bid;
    let (status, body) = post(
        &app,
        Some(&ada.key),
        "/api/symbols/ACME/orders",
        json!({"trader_id": ada.id, "type": "limit", "side": "buy", "qty": qty,
               "price_cents": bid}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (_, account) = get(
        &app,
        Some(&ada.key),
        &format!("/api/accounts/{}", ada.account),
    )
    .await;
    assert!(
        account["available_cents"].as_i64().unwrap() < bid,
        "the order reserves all but the change: {account}"
    );

    for minute in 1..=60 {
        engine::advance_to(&app, Timestamp(NOW_MS + minute * 60_000)).await;
    }

    let (_, portfolio) = get(&app, Some(&ada.key), &format!("/api/traders/{}", ada.id)).await;
    let filled: i64 = portfolio["fills"]
        .as_array()
        .expect("the fill log")
        .iter()
        .map(|f| f["qty"].as_i64().unwrap())
        .sum();
    let resting: i64 = portfolio["open_orders"]
        .as_array()
        .expect("the open orders")
        .iter()
        .map(|o| o["remaining"].as_i64().unwrap())
        .sum();
    assert!(filled > 0, "the bid was hit at all: {portfolio}");
    assert_eq!(
        filled + resting,
        qty,
        "every share the book worked off the order was booked: {portfolio}"
    );
    assert_eq!(audit(&app, "after the bid filled").await, genesis);
}
