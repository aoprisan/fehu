//! Milestone 3's acceptance: every way a fill can happen, in a world where
//! both sides of one are funded.
//!
//! The claim being tested is a conservation law with two halves. **Currency**
//! is conserved: minted less burned is what the wallets hold, before and
//! after every order type the venue accepts, and nothing but an operator's
//! mint or burn moves either number. **Units** are conserved: a stock's
//! float does not change because it traded, and a good's issued-less-consumed
//! is exactly what its holders hold.
//!
//! The world here runs with `FEHU_SYNTHETIC=0`, so there is no ladder to sell
//! what nobody owns and no printed flow to pay for. Every counterparty is a
//! player or a merchant, and `synthetic_debt_cents` must be zero at the end
//! of all of it — which is the number milestone 3 exists to retire.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use fehu::Timestamp;
use fehu_economy::market::{App, Options};
use fehu_economy::{engine, router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::util::ServiceExt;

/// 2023-11-14T22:13:20Z, as in the other suites.
const NOW_MS: i64 = 1_700_000_000_000;

/// A world where nothing is quoted by nobody.
fn funded_world() -> Arc<App> {
    App::new(Options {
        history_days: 0,
        warmup_hours: 0,
        now_ms: Some(NOW_MS),
        rate_per_sec: 0.0,
        synthetic: false,
        // Automatic halts off: this suite drives the price hard to reach a
        // stop, and a halt would hold the trigger rather than fire it. What
        // a halt does has its own tests.
        price_limit_pct: 0.0,
        ..Options::default()
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

async fn get(app: &Arc<App>, key: Option<&str>, uri: &str) -> Value {
    let (status, body) = call(app, key, Request::get(uri).body(Body::empty()).unwrap()).await;
    assert!(status.is_success(), "GET {uri} → {status}: {body}");
    body
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

async fn patch(app: &Arc<App>, key: Option<&str>, uri: &str, body: Value) -> (StatusCode, Value) {
    call(
        app,
        key,
        Request::builder()
            .method(Method::PATCH)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await
}

struct Player {
    id: u64,
    key: String,
}

async fn sign_up(app: &Arc<App>, name: &str, cash_cents: i64) -> Player {
    let (status, body) = post(
        app,
        None,
        "/api/traders",
        json!({ "name": name, "cash_cents": cash_cents }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    Player {
        id: body["id"].as_u64().unwrap(),
        key: body["api_key"].as_str().unwrap().to_owned(),
    }
}

/// What the world says about its own money.
async fn supply(app: &Arc<App>) -> Value {
    get(app, None, "/api/supply").await
}

/// The books add up, every issued unit is somewhere, and nothing was owed by
/// nobody.
async fn conserved(app: &Arc<App>, since: &Value, what: &str) {
    let report = get(app, None, "/api/reconcile").await;
    assert_eq!(report["valid"], true, "after {what}: {report}");
    let now = supply(app).await;
    assert_eq!(now["balanced"], true, "after {what}: {now}");
    assert_eq!(
        now["minted_cents"], since["minted_cents"],
        "after {what}, currency was created"
    );
    assert_eq!(
        now["burned_cents"], since["burned_cents"],
        "after {what}, currency was destroyed"
    );
    assert_eq!(
        now["outstanding_cents"], since["outstanding_cents"],
        "after {what}, the supply moved"
    );
    assert_eq!(
        now["synthetic_debt_cents"], 0,
        "after {what}, something settled against nobody"
    );
}

/// Every unit of `symbol` that exists is held by somebody.
async fn units_all_somewhere(app: &Arc<App>, symbol: &str) {
    let shares = get(app, None, &format!("/api/symbols/{symbol}/shares")).await;
    assert_eq!(
        shares["held_shares"], shares["shares_outstanding"],
        "{symbol}: units in existence that nobody holds"
    );
}

#[tokio::test]
async fn every_kind_of_fill_conserves_the_currency_and_the_units() {
    let app = funded_world();

    // A good, a merchant to make a market in it, and two players.
    let (status, body) = post(
        &app,
        None,
        "/api/symbols",
        json!({
            "symbol": "ORE", "kind": "good", "unit": "kg",
            "name": "Iron Ore", "start_price_cents": 10_000,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, npc) = post(
        &app,
        None,
        "/api/npcs",
        json!({
            "symbol": "ORE", "name": "Ore Merchant",
            "cash_cents": 50_000_000, "inventory": 5_000,
            "size": 200, "levels": 4,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{npc}");
    let alice = sign_up(&app, "alice", 200_000_000).await;
    let bob = sign_up(&app, "bob", 200_000_000).await;
    engine::advance_to(&app, Timestamp(NOW_MS + 1_000)).await;

    let opening = supply(&app).await;
    conserved(&app, &opening, "the world was built").await;
    units_all_somewhere(&app, "ORE").await;

    let quotes = |app: &Arc<App>| {
        let app = Arc::clone(app);
        async move {
            let book = get(&app, None, "/api/symbols/ORE/book").await;
            let bid = book["bids"][0]["price_cents"].as_i64().expect("a bid");
            let ask = book["asks"][0]["price_cents"].as_i64().expect("an ask");
            (bid, ask)
        }
    };

    // 1. A player takes part of what a merchant is offering: a partial fill
    //    against a funded counterparty.
    let (_, ask) = quotes(&app).await;
    let (status, order) = post(
        &app,
        Some(&alice.key),
        "/api/symbols/ORE/orders",
        json!({ "trader_id": alice.id, "side": "buy", "qty": 150, "type": "limit", "price_cents": ask }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{order}");
    assert_eq!(order["filled"], 150);
    conserved(&app, &opening, "a player bought from a merchant").await;
    units_all_somewhere(&app, "ORE").await;

    // 2. Immediate-or-cancel: it takes what is there and the rest is
    //    dropped rather than left resting.
    engine::advance_to(&app, Timestamp(NOW_MS + 2_000)).await;
    let (_, ask) = quotes(&app).await;
    let (status, ioc) = post(
        &app,
        Some(&bob.key),
        "/api/symbols/ORE/orders",
        json!({
            "trader_id": bob.id, "side": "buy", "qty": 900,
            "type": "limit", "price_cents": ask, "tif": "ioc"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{ioc}");
    assert!(ioc["filled"].as_u64().unwrap() > 0, "{ioc}");
    assert!(ioc["filled"].as_u64().unwrap() < 900, "{ioc}");
    assert_eq!(ioc["status"], "cancelled", "the remainder did not rest");
    conserved(&app, &opening, "an immediate-or-cancel").await;
    units_all_somewhere(&app, "ORE").await;

    // 3. Fill-or-kill: more than the book holds, so nothing happens at all.
    engine::advance_to(&app, Timestamp(NOW_MS + 3_000)).await;
    let (_, ask) = quotes(&app).await;
    let held_before = get(&app, None, "/api/symbols/ORE/shares").await;
    let (status, fok) = post(
        &app,
        Some(&bob.key),
        "/api/symbols/ORE/orders",
        json!({
            "trader_id": bob.id, "side": "buy", "qty": 5_000,
            "type": "limit", "price_cents": ask, "tif": "fok"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{fok}");
    assert_eq!(fok["filled"], 0, "all or nothing, and it was nothing");
    let held_after = get(&app, None, "/api/symbols/ORE/shares").await;
    assert_eq!(held_after["held_shares"], held_before["held_shares"]);
    conserved(&app, &opening, "a fill-or-kill that killed").await;

    // 4. A resting order, amended, then filled at its new price. Alice
    //    undercuts the merchant so that the fill is genuinely player to
    //    player rather than a sweep of the cheaper quotes beside it.
    engine::advance_to(&app, Timestamp(NOW_MS + 4_000)).await;
    let (bid, ask) = quotes(&app).await;
    assert!(bid + 2 < ask, "there is room inside the spread");
    let (status, resting) = post(
        &app,
        Some(&alice.key),
        "/api/symbols/ORE/orders",
        json!({
            "trader_id": alice.id, "side": "sell", "qty": 100,
            "type": "limit", "price_cents": bid + 1, "post_only": true
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{resting}");
    assert_eq!(resting["status"], "resting", "{resting}");
    let order_id = resting["order_id"].as_u64().unwrap();
    let (status, amended) = patch(
        &app,
        Some(&alice.key),
        &format!("/api/symbols/ORE/orders/{order_id}"),
        json!({ "trader_id": alice.id, "qty": 60, "price_cents": bid + 2 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{amended}");
    assert_eq!(amended["qty"], 60, "{amended}");
    conserved(&app, &opening, "an amendment").await;
    units_all_somewhere(&app, "ORE").await;

    let (status, taken) = post(
        &app,
        Some(&bob.key),
        "/api/symbols/ORE/orders",
        json!({
            "trader_id": bob.id, "side": "buy", "qty": 60,
            "type": "limit", "price_cents": bid + 2
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{taken}");
    assert_eq!(taken["filled"], 60, "player to player: {taken}");
    conserved(&app, &opening, "a player-to-player fill").await;
    units_all_somewhere(&app, "ORE").await;

    // 5. A stop that fires becomes an order like any other.
    let (bid, _) = quotes(&app).await;
    let (status, stop) = post(
        &app,
        Some(&bob.key),
        "/api/symbols/ORE/stops",
        json!({
            "trader_id": bob.id, "side": "sell", "qty": 50,
            "stop_price_cents": bid / 2
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{stop}");
    // A sell stop sits below the market and fires when the price reaches
    // it, which the merchant's own quoting will not do on its own — so move
    // the reference there.
    post(
        &app,
        None,
        "/api/symbols/ORE/events",
        json!({ "type": "jump", "pct": -0.7 }),
    )
    .await;
    engine::advance_to(&app, Timestamp(NOW_MS + 30_000)).await;
    let stops = get(&app, Some(&bob.key), &format!("/api/traders/{}", bob.id)).await;
    assert!(
        stops["stops"].as_array().unwrap().is_empty(),
        "the stop fired: {stops}"
    );
    conserved(&app, &opening, "a stop that fired").await;
    units_all_somewhere(&app, "ORE").await;

    // 6. Consuming: the one thing that does change the count of units, and
    //    the only thing that may.
    let inventory = get(
        &app,
        Some(&alice.key),
        &format!("/api/traders/{}", alice.id),
    )
    .await;
    let free = inventory["positions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["symbol"] == "ORE")
        .map_or(0, |p| p["free_shares"].as_u64().unwrap());
    assert!(free > 0, "alice has ore to eat: {inventory}");
    let before = get(&app, None, "/api/symbols/ORE").await;
    let (status, receipt) = post(
        &app,
        Some(&alice.key),
        &format!("/api/traders/{}/consume", alice.id),
        json!({ "symbol": "ORE", "qty": free }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    assert_eq!(
        receipt["units_outstanding"].as_u64().unwrap(),
        before["info"]["asset"]["issued"].as_u64().unwrap()
            - before["info"]["asset"]["consumed"].as_u64().unwrap()
            - free
    );
    conserved(&app, &opening, "units were eaten").await;
    units_all_somewhere(&app, "ORE").await;
}

#[tokio::test]
async fn corporate_actions_still_conserve_the_currency_without_a_ladder() {
    let app = funded_world();
    let (status, npc) = post(
        &app,
        None,
        "/api/npcs",
        json!({
            "symbol": "ACME", "cash_cents": 200_000_000,
            "inventory": 50_000, "size": 400, "levels": 3,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{npc}");
    let player = sign_up(&app, "wanda", 50_000_000).await;
    engine::advance_to(&app, Timestamp(NOW_MS + 1_000)).await;
    let book = get(&app, None, "/api/symbols/ACME/book").await;
    let ask = book["asks"][0]["price_cents"].as_i64().expect("an ask");
    let (status, order) = post(
        &app,
        Some(&player.key),
        "/api/symbols/ACME/orders",
        json!({ "trader_id": player.id, "side": "buy", "qty": 400, "type": "limit", "price_cents": ask }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{order}");
    assert_eq!(order["filled"], 400);

    let opening = supply(&app).await;
    conserved(&app, &opening, "a funded fill in a stock").await;

    // A dividend is money from the issuer's wallet, not from nowhere.
    let (status, dividend) = post(
        &app,
        None,
        "/api/symbols/ACME/dividend",
        json!({ "cents_per_share": 25 }),
    )
    .await;
    assert!(status.is_success(), "{status}: {dividend}");
    assert!(dividend["dividend"]["total_cents"].as_i64().unwrap() > 0);
    conserved(&app, &opening, "a dividend").await;

    // And a delisting buys every holder out of a float that never grew.
    let shares = get(&app, None, "/api/symbols/ACME/shares").await;
    let outstanding = shares["shares_outstanding"].as_u64().unwrap();
    let held = shares["held_shares"].as_u64().unwrap();
    let (status, delisting) = post(
        &app,
        None,
        "/api/symbols/ACME/delist",
        json!({ "cents_per_share": 100 }),
    )
    .await;
    assert!(status.is_success(), "{status}: {delisting}");
    assert_eq!(
        delisting["delisting"]["shares_bought_out"], held,
        "the merchant and the player, and nobody else"
    );
    assert!(held <= outstanding, "the float never grew");
    conserved(&app, &opening, "a delisting").await;

    // The merchant's symbol is gone; it stops quoting rather than erroring.
    engine::advance_to(&app, Timestamp(NOW_MS + 60_000)).await;
    conserved(&app, &opening, "the world carried on without it").await;
}
