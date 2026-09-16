//! NPC traders: the world holding inventory and quoting it.
//!
//! The point of an NPC is that it is not special. It is a funded trader with
//! a wallet, a position and reservations, its orders go through the same
//! command path a player's do, and a fill against it settles like any other
//! — which is why these tests can ask the ordinary questions: does the
//! currency add up, are the units all somewhere, and does the book empty
//! when the till does.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use fehu::Timestamp;
use fehu_economy::market::{App, Options};
use fehu_economy::{engine, router, save};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::util::ServiceExt;

/// 2023-11-14T22:13:20Z, as in the other suites.
const NOW_MS: i64 = 1_700_000_000_000;

fn test_app() -> Arc<App> {
    App::new(Options {
        history_days: 0,
        warmup_hours: 0,
        now_ms: Some(NOW_MS),
        rate_per_sec: 0.0,
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

async fn supply(app: &Arc<App>) -> Value {
    let (status, body) = get(app, None, "/api/supply").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

async fn reconciles(app: &Arc<App>) {
    let (status, body) = get(app, None, "/api/reconcile").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["valid"], true, "{body}");
}

/// List `ORE`, a good priced around a dollar.
async fn list_ore(app: &Arc<App>) {
    let (status, body) = post(
        app,
        None,
        "/api/symbols",
        json!({
            "symbol": "ORE", "kind": "good", "unit": "kg",
            "name": "Iron Ore", "start_price_cents": 10_000,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

/// Put a merchant in `symbol` with `cash` and `inventory`.
async fn merchant(app: &Arc<App>, symbol: &str, cash: i64, inventory: u64, size: u64) -> Value {
    let (status, body) = post(
        app,
        None,
        "/api/npcs",
        json!({
            "symbol": symbol, "name": "Ore Merchant",
            "cash_cents": cash, "inventory": inventory,
            "size": size, "levels": 2,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body
}

#[tokio::test]
async fn an_npc_is_funded_out_of_treasury_and_makes_no_currency() {
    let app = test_app();
    list_ore(&app).await;
    let before = supply(&app).await;
    let npc = merchant(&app, "ORE", 5_000_000, 800, 100).await;
    assert_eq!(npc["symbol"], "ORE");
    assert_eq!(npc["active"], true);

    let after = supply(&app).await;
    assert_eq!(after["minted_cents"], before["minted_cents"]);
    assert_eq!(after["outstanding_cents"], before["outstanding_cents"]);
    assert_eq!(after["balanced"], true);
    assert_eq!(
        after["treasury_cents"].as_i64().unwrap(),
        before["treasury_cents"].as_i64().unwrap() - 5_000_000,
        "the till was filled out of treasury, not out of thin air"
    );
    assert_eq!(after["npc_cents"], 5_000_000);

    // Its inventory was issued, and it is the holder of every unit.
    let (_, detail) = get(&app, None, "/api/symbols/ORE").await;
    assert_eq!(detail["info"]["asset"]["issued"], 800);
    let (_, shares) = get(&app, None, "/api/symbols/ORE/shares").await;
    assert_eq!(shares["shares_outstanding"], 800);
    assert_eq!(shares["held_shares"], 800);
    reconciles(&app).await;
}

#[tokio::test]
async fn an_npc_quotes_both_sides_and_a_player_trades_with_it() {
    let app = test_app();
    list_ore(&app).await;
    merchant(&app, "ORE", 5_000_000, 800, 100).await;
    // Nothing is on the book until the world ticks: quoting is a decision
    // the NPC takes in the engine step.
    let (_, empty) = get(&app, None, "/api/symbols/ORE/book").await;
    assert!(empty["bids"].as_array().unwrap().is_empty());
    engine::advance_to(&app, Timestamp(NOW_MS + 1_000)).await;

    let (_, book) = get(&app, None, "/api/symbols/ORE/book").await;
    let bids = book["bids"].as_array().unwrap();
    let asks = book["asks"].as_array().unwrap();
    assert_eq!(bids.len(), 2, "two levels a side: {book}");
    assert_eq!(asks.len(), 2);
    let best_bid = bids[0]["price_cents"].as_i64().unwrap();
    let best_ask = asks[0]["price_cents"].as_i64().unwrap();
    assert!(
        best_bid < best_ask,
        "a merchant does not lock its own market"
    );
    assert_eq!(bids[0]["qty"], 100);

    let player = sign_up(&app, "wanda", 1_000_000).await;
    let before = supply(&app).await;
    let (status, order) = post(
        &app,
        Some(&player.key),
        "/api/symbols/ORE/orders",
        json!({
            "trader_id": player.id, "side": "buy", "qty": 60,
            "type": "limit", "price_cents": best_ask
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{order}");
    assert_eq!(order["status"], "filled", "{order}");
    assert_eq!(order["filled"], 60);

    // Units moved from the merchant to the player; none were made.
    let (_, detail) = get(&app, None, "/api/symbols/ORE").await;
    assert_eq!(detail["info"]["asset"]["issued"], 800);
    let (_, npcs) = get(&app, None, "/api/npcs").await;
    assert_eq!(npcs["npcs"][0]["inventory"], 740);

    // And the money moved between the two of them and nowhere else.
    let after = supply(&app).await;
    assert_eq!(after["outstanding_cents"], before["outstanding_cents"]);
    assert_eq!(after["balanced"], true);
    assert_eq!(
        after["synthetic_debt_cents"], before["synthetic_debt_cents"],
        "the counterparty was funded, so nothing was owed by nobody"
    );
    assert_eq!(
        after["npc_cents"].as_i64().unwrap() - before["npc_cents"].as_i64().unwrap(),
        before["player_cents"].as_i64().unwrap() - after["player_cents"].as_i64().unwrap(),
        "what the player paid is what the merchant took"
    );
    reconciles(&app).await;
}

#[tokio::test]
async fn a_producer_makes_what_it_sells_and_buys_what_that_takes() {
    // Ore is sold from the catalogue; ingots are smelted from it. A producer
    // in INGOT with cash and no stock buys ore, runs the recipe, and quotes
    // what comes out — and keeps doing so while its stock is low.
    let app = test_app();
    list_ore(&app).await;
    let (status, body) = post(
        &app,
        None,
        "/api/symbols",
        json!({ "symbol": "INGOT", "kind": "good", "unit": "bar", "name": "Iron Ingot", "start_price_cents": 50_000 }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = post(
        &app,
        None,
        "/api/catalog",
        json!({ "symbol": "ORE", "price_cents": 100 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = post(
        &app,
        None,
        "/api/recipes",
        json!({
            "id": "smelt",
            "inputs": [{ "symbol": "ORE", "qty": 2 }],
            "outputs": [{ "symbol": "INGOT", "qty": 1 }],
            "cost_cents": 500,
            "duration_secs": 60,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, smith) = post(
        &app,
        None,
        "/api/npcs",
        json!({
            "symbol": "INGOT", "name": "Smithy", "cash_cents": 1_000_000, "inventory": 0,
            "size": 1, "levels": 1,
            "production": { "recipe": "smelt", "restock_below": 5, "runs": 3, "max_running": 1 },
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{smith}");
    assert_eq!(
        smith["production"]["recipe"], "SMELT",
        "filed as the book files it"
    );
    let smith_id = smith["trader_id"].as_u64().unwrap();
    let before = supply(&app).await;

    // First step: no stock, so it buys six ore and starts one job. Nothing
    // is minted for any of it; the ore money went to ORE's issuer.
    engine::advance_to(&app, Timestamp(NOW_MS + 1_000)).await;
    let (_, ore) = get(&app, None, "/api/symbols/ORE").await;
    assert_eq!(ore["info"]["asset"]["issued"], 6, "{ore}");
    let after = supply(&app).await;
    assert_eq!(after["outstanding_cents"], before["outstanding_cents"]);
    assert_eq!(
        after["issuer_cents"].as_i64().unwrap() - before["issuer_cents"].as_i64().unwrap(),
        600,
        "six ore at 1.00 went to ORE's issuer"
    );
    assert_eq!(
        after["venue_cents"].as_i64().unwrap() - before["venue_cents"].as_i64().unwrap(),
        1_500,
        "and 15.00 of furnace to the venue"
    );

    // One job at a time: the next step starts nothing more.
    engine::advance_to(&app, Timestamp(NOW_MS + 2_000)).await;
    let (_, ore) = get(&app, None, "/api/symbols/ORE").await;
    assert_eq!(
        ore["info"]["asset"]["issued"], 6,
        "the furnace is full: {ore}"
    );

    // Delivered: three ingots, on the book, and — still below the line —
    // another batch of ore bought in the same step.
    engine::advance_to(&app, Timestamp(NOW_MS + 61_000)).await;
    let (_, ingot) = get(&app, None, "/api/symbols/INGOT").await;
    assert_eq!(ingot["info"]["asset"]["issued"], 3, "{ingot}");
    let (_, book) = get(&app, None, "/api/symbols/INGOT/book").await;
    assert!(
        !book["asks"].as_array().unwrap().is_empty(),
        "what it made is for sale: {book}"
    );
    let (_, ore) = get(&app, None, "/api/symbols/ORE").await;
    assert_eq!(
        ore["info"]["asset"]["issued"], 12,
        "restocking again: {ore}"
    );
    reconciles(&app).await;

    // Taking the policy away makes it a merchant again: the running job
    // still delivers, but nothing starts after it.
    let (status, body) = post(
        &app,
        None,
        &format!("/api/npcs/{smith_id}/production"),
        json!({ "production": null }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["production"].is_null());
    engine::advance_to(&app, Timestamp(NOW_MS + 200_000)).await;
    let (_, ingot) = get(&app, None, "/api/symbols/INGOT").await;
    assert_eq!(ingot["info"]["asset"]["issued"], 6);
    let (_, ore) = get(&app, None, "/api/symbols/ORE").await;
    assert_eq!(ore["info"]["asset"]["issued"], 12, "no third batch: {ore}");

    // And a policy that names nonsense is refused.
    let (status, body) = post(
        &app,
        None,
        &format!("/api/npcs/{smith_id}/production"),
        json!({ "production": { "recipe": "smelt", "restock_below": 5, "runs": 0, "max_running": 1 } }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    reconciles(&app).await;
}

#[tokio::test]
async fn a_merchant_that_runs_out_has_nothing_to_say() {
    let app = test_app();
    list_ore(&app).await;
    // Enough inventory for one level, and no cash at all: it can sell but
    // never bid.
    merchant(&app, "ORE", 0, 40, 40).await;
    engine::advance_to(&app, Timestamp(NOW_MS + 1_000)).await;

    let (_, book) = get(&app, None, "/api/symbols/ORE/book").await;
    assert!(
        book["bids"].as_array().unwrap().is_empty(),
        "an empty till is an empty bid: {book}"
    );
    let asks = book["asks"].as_array().unwrap();
    assert_eq!(
        asks.len(),
        1,
        "one level's worth of stock, one level: {book}"
    );

    let player = sign_up(&app, "wanda", 5_000_000).await;
    let (status, order) = post(
        &app,
        Some(&player.key),
        "/api/symbols/ORE/orders",
        json!({
            "trader_id": player.id, "side": "buy", "qty": 40,
            "type": "limit", "price_cents": asks[0]["price_cents"]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{order}");
    assert_eq!(order["filled"], 40);

    // Sold out. The merchant now has cash and no stock, so the sides swap.
    engine::advance_to(&app, Timestamp(NOW_MS + 600_000)).await;
    let (_, book) = get(&app, None, "/api/symbols/ORE/book").await;
    assert!(
        book["asks"].as_array().unwrap().is_empty(),
        "nothing left to sell: {book}"
    );
    assert!(
        !book["bids"].as_array().unwrap().is_empty(),
        "and money to buy with: {book}"
    );
    reconciles(&app).await;
}

#[tokio::test]
async fn switching_a_merchant_off_takes_its_quotes_with_it() {
    let app = test_app();
    list_ore(&app).await;
    let npc = merchant(&app, "ORE", 5_000_000, 800, 100).await;
    let trader_id = npc["trader_id"].as_u64().unwrap();
    engine::advance_to(&app, Timestamp(NOW_MS + 1_000)).await;
    let (_, book) = get(&app, None, "/api/symbols/ORE/book").await;
    assert!(!book["bids"].as_array().unwrap().is_empty());

    let (status, off) = post(
        &app,
        None,
        &format!("/api/npcs/{trader_id}/active"),
        json!({ "active": false }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{off}");
    assert_eq!(off["active"], false);
    let (_, book) = get(&app, None, "/api/symbols/ORE/book").await;
    assert!(book["bids"].as_array().unwrap().is_empty(), "{book}");
    assert!(book["asks"].as_array().unwrap().is_empty(), "{book}");

    // It keeps everything it had; it simply stops offering it.
    let (_, npcs) = get(&app, None, "/api/npcs").await;
    assert_eq!(npcs["npcs"][0]["inventory"], 800);
    assert_eq!(npcs["npcs"][0]["cash_cents"], 5_000_000);
    engine::advance_to(&app, Timestamp(NOW_MS + 600_000)).await;
    let (_, book) = get(&app, None, "/api/symbols/ORE/book").await;
    assert!(book["bids"].as_array().unwrap().is_empty(), "still quiet");

    let (status, on) = post(
        &app,
        None,
        &format!("/api/npcs/{trader_id}/active"),
        json!({ "active": true }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{on}");
    engine::advance_to(&app, Timestamp(NOW_MS + 601_000)).await;
    let (_, book) = get(&app, None, "/api/symbols/ORE/book").await;
    assert!(!book["bids"].as_array().unwrap().is_empty(), "back: {book}");
    reconciles(&app).await;
}

#[tokio::test]
async fn a_merchant_in_a_stock_is_given_shares_nobody_held() {
    let app = test_app();
    let before = supply(&app).await;
    let (_, shares) = get(&app, None, "/api/symbols/ACME/shares").await;
    let outstanding = shares["shares_outstanding"].as_u64().unwrap();
    merchant(&app, "ACME", 10_000_000, 1_000, 100).await;

    let (_, shares) = get(&app, None, "/api/symbols/ACME/shares").await;
    assert_eq!(
        shares["shares_outstanding"], outstanding,
        "a company's float does not grow because somebody was given some"
    );
    assert_eq!(shares["held_shares"], 1_000);
    let after = supply(&app).await;
    assert_eq!(after["outstanding_cents"], before["outstanding_cents"]);
    assert_eq!(after["balanced"], true);

    // More than is unheld is refused outright.
    let (status, body) = post(
        &app,
        None,
        "/api/npcs",
        json!({
            "symbol": "ACME", "cash_cents": 0,
            "inventory": outstanding, "size": 10,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    reconciles(&app).await;
}

#[tokio::test]
async fn a_merchant_comes_back_quoting_what_it_was_quoting() {
    let dir = tempdir_lite::TempDir::new("fehu-npc");
    let path = dir.path().join("state.json");
    let options = || Options {
        history_days: 0,
        warmup_hours: 0,
        now_ms: Some(NOW_MS),
        rate_per_sec: 0.0,
        state_file: Some(path.clone()),
        ..Options::default()
    };
    let before = App::new(options());
    list_ore(&before).await;
    merchant(&before, "ORE", 5_000_000, 800, 100).await;
    engine::advance_to(&before, Timestamp(NOW_MS + 1_000)).await;
    let (_, book) = get(&before, None, "/api/symbols/ORE/book").await;
    let (_, npcs) = get(&before, None, "/api/npcs").await;

    save::write(&before, &path).await.expect("state written");
    let after = App::restore(options(), save::read(&path).unwrap());

    let (_, restored_npcs) = get(&after, None, "/api/npcs").await;
    assert_eq!(restored_npcs, npcs, "the merchant, its till and its stock");
    let (_, restored_book) = get(&after, None, "/api/symbols/ORE/book").await;
    assert_eq!(restored_book["bids"], book["bids"], "its resting quotes");
    assert_eq!(restored_book["asks"], book["asks"]);

    // And it carries on: a step that does not move the reference far enough
    // leaves the quotes exactly where they were.
    engine::advance_to(&after, Timestamp(NOW_MS + 2_000)).await;
    let (_, stepped) = get(&after, None, "/api/symbols/ORE/book").await;
    assert_eq!(stepped["bids"], book["bids"], "the band held");
    reconciles(&after).await;
}

#[tokio::test]
async fn a_world_with_no_synthetic_liquidity_owes_nobody_anything() {
    // The whole point of milestone 3: with the ladder off, every fill has a
    // funded counterparty, so the wallet that stands in for liquidity
    // nobody paid for never has to stand in for anything.
    let app = App::new(Options {
        history_days: 0,
        warmup_hours: 0,
        now_ms: Some(NOW_MS),
        rate_per_sec: 0.0,
        synthetic: false,
        ..Options::default()
    });
    let genesis = supply(&app).await;
    assert_eq!(
        genesis["synthetic_debt_cents"], 0,
        "and no float was set aside for it either"
    );

    // A seeded stock is quoted by nobody until somebody funds it.
    engine::advance_to(&app, Timestamp(NOW_MS + 5_000)).await;
    let (_, book) = get(&app, None, "/api/symbols/ACME/book").await;
    assert!(book["bids"].as_array().unwrap().is_empty(), "{book}");
    let (_, tape) = get(&app, None, "/api/symbols/ACME/trades").await;
    assert!(
        tape["trades"].as_array().unwrap().is_empty(),
        "nothing printed: {tape}"
    );
    let (_, quote) = get(&app, None, "/api/symbols/ACME").await;
    assert!(
        quote["ticks_total"].as_u64().unwrap() > 0,
        "the simulator still runs as the reference"
    );

    // A merchant makes it a market.
    merchant(&app, "ACME", 500_000_000, 20_000, 500).await;
    engine::advance_to(&app, Timestamp(NOW_MS + 6_000)).await;
    let (_, book) = get(&app, None, "/api/symbols/ACME/book").await;
    let asks = book["asks"].as_array().unwrap();
    assert!(!asks.is_empty(), "somebody is quoting now: {book}");

    let player = sign_up(&app, "wanda", 500_000_000).await;
    let (status, order) = post(
        &app,
        Some(&player.key),
        "/api/symbols/ACME/orders",
        json!({
            "trader_id": player.id, "side": "buy", "qty": 400,
            "type": "limit", "price_cents": asks[0]["price_cents"]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{order}");
    assert_eq!(order["filled"], 400, "{order}");

    let after = supply(&app).await;
    assert_eq!(
        after["synthetic_debt_cents"], 0,
        "every cent of that fill came from somebody who had it"
    );
    assert_eq!(after["balanced"], true);
    assert_eq!(after["outstanding_cents"], genesis["outstanding_cents"]);
    reconciles(&app).await;
}

/// A throwaway directory that cleans up after itself, as in `save.rs`.
#[tokio::test]
async fn a_world_can_be_seeded_with_a_merchant_behind_every_symbol() {
    // No synthetic ladder: without merchants this world's books are empty,
    // which is exactly the case seeding exists for.
    let app = App::new(Options {
        history_days: 0,
        warmup_hours: 0,
        now_ms: Some(NOW_MS),
        rate_per_sec: 0.0,
        synthetic: false,
        synthetic_float_cents: 0,
        ..Options::default()
    });
    let before = supply(&app).await;
    engine::advance_to(&app, Timestamp(NOW_MS + 1_000)).await;
    let (_, book) = get(&app, None, "/api/symbols/acme/book").await;
    assert!(
        book["bids"].as_array().unwrap().is_empty() && book["asks"].as_array().unwrap().is_empty(),
        "nobody has funded anything yet: {book}"
    );

    let made = app.seed_merchants(10_000_000).await;
    assert_eq!(made, 4, "one behind each seeded symbol");

    let after = supply(&app).await;
    assert_eq!(
        after["outstanding_cents"], before["outstanding_cents"],
        "the tills are filled out of treasury, not minted"
    );
    assert_eq!(after["npc_cents"], 40_000_000);

    engine::advance_to(&app, Timestamp(NOW_MS + 2_000)).await;
    let (_, book) = get(&app, None, "/api/symbols/acme/book").await;
    assert!(
        !book["bids"].as_array().unwrap().is_empty(),
        "and now somebody is quoting: {book}"
    );
    assert!(!book["asks"].as_array().unwrap().is_empty(), "{book}");
    assert_eq!(
        supply(&app).await["synthetic_debt_cents"],
        0,
        "with nothing owed to nobody"
    );
    reconciles(&app).await;

    // Seeding again is a second call, not a second merchant: a symbol that
    // already has one is skipped.
    assert_eq!(app.seed_merchants(10_000_000).await, 0);
    let (_, npcs) = get(&app, None, "/api/npcs").await;
    assert_eq!(npcs["npcs"].as_array().unwrap().len(), 4);
    reconciles(&app).await;
}

mod tempdir_lite {
    use std::path::{Path, PathBuf};

    pub struct TempDir(PathBuf);

    impl TempDir {
        pub fn new(prefix: &str) -> Self {
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            let path =
                std::env::temp_dir().join(format!("{prefix}-{unique}-{}", std::process::id()));
            std::fs::create_dir_all(&path).expect("a temporary directory");
            Self(path)
        }

        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
