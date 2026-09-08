//! Goods: assets that are issued and consumed rather than floated.
//!
//! A good is the same thing as a stock everywhere it matters — the same
//! book, the same positions, the same reservations — and deliberately not
//! the same in three places. It has no synthetic liquidity, because a print
//! against liquidity nobody funded would be a unit nobody issued; it has no
//! dividend and no buyout, because it has no shareholders; and its supply is
//! a count that moves, so the audit asks a stricter question of it than of a
//! company: not "is this within the float" but "is every issued unit
//! somewhere".

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use fehu::Timestamp;
use fehu_webapp::market::{App, Options};
use fehu_webapp::{engine, router, save};
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

/// A player with cash, and the key they trade with.
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

/// What the world says about its own money: minted, burned and where it
/// sits.
async fn supply(app: &Arc<App>) -> Value {
    let (status, body) = get(app, None, "/api/supply").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

/// Assert the books add up and every issued unit is somewhere.
async fn reconciles(app: &Arc<App>) {
    let (status, body) = get(app, None, "/api/reconcile").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["valid"], true, "{body}");
}

/// Put `symbol` in the catalogue at `price_cents`, with `available` units to
/// make.
async fn stock_catalog(
    app: &Arc<App>,
    symbol: &str,
    price_cents: i64,
    available: Option<u64>,
) -> Value {
    let mut body = json!({ "symbol": symbol, "price_cents": price_cents });
    if let Some(available) = available {
        body["available"] = json!(available);
    }
    let (status, body) = post(app, None, "/api/catalog", body).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

/// The body that lists one good.
fn good(symbol: &str, unit: &str) -> Value {
    json!({
        "symbol": symbol,
        "kind": "good",
        "name": "Iron Ore",
        "sector": "Materials",
        "description": "Dug up, smelted, gone.",
        "unit": unit,
        "start_price_cents": 250,
    })
}

/// List `symbol` as a good and return the listing response.
async fn list_good(app: &Arc<App>, symbol: &str, unit: &str) -> Value {
    let (status, body) = post(app, None, "/api/symbols", good(symbol, unit)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body
}

#[tokio::test]
async fn a_good_is_listed_holding_nothing_and_quoting_nothing() {
    let app = test_app();
    let body = list_good(&app, "ore", "kg").await;
    assert_eq!(body["quote"]["symbol"], "ORE");
    assert_eq!(body["quote"]["asset_kind"], "good");
    assert_eq!(body["quote"]["unit"], "kg");
    assert_eq!(
        body["quote"]["shares_outstanding"], 0,
        "a good is listed with nothing in existence"
    );
    assert_eq!(body["quote"]["market_cap_cents"], 0);
    assert_eq!(
        body["quote"]["bid_cents"],
        Value::Null,
        "no ladder is quoted"
    );
    assert_eq!(body["quote"]["ask_cents"], Value::Null);
    assert_eq!(body["event"]["kind"], "corporate:listing");

    // The simulator still runs underneath it as a reference price, but
    // nothing prints against it.
    engine::advance_to(&app, Timestamp(NOW_MS + 60_000)).await;
    let (_, detail) = get(&app, None, "/api/symbols/ore").await;
    assert_eq!(detail["info"]["asset"]["kind"], "good");
    assert_eq!(detail["info"]["asset"]["issued"], 0);
    assert_eq!(detail["info"]["asset"]["consumed"], 0);
    assert_eq!(detail["info"]["asset"]["unit"], "kg");
    assert!(detail["ticks_total"].as_u64().unwrap() > 0, "it ticks");
    assert_eq!(detail["quote"]["day_volume"], 0, "but nothing trades");

    let (_, book) = get(&app, None, "/api/symbols/ore/book").await;
    assert!(book["bids"].as_array().unwrap().is_empty());
    assert!(book["asks"].as_array().unwrap().is_empty());
    let (_, tape) = get(&app, None, "/api/symbols/ore/trades").await;
    assert!(tape["trades"].as_array().unwrap().is_empty());

    let (_, shares) = get(&app, None, "/api/symbols/ore/shares").await;
    assert_eq!(shares["asset_kind"], "good");
    assert_eq!(shares["unit"], "kg");
    assert_eq!(shares["shares_outstanding"], 0);
    assert_eq!(shares["held_shares"], 0);
}

#[tokio::test]
async fn a_good_has_neither_shareholders_nor_a_buyout() {
    let app = test_app();
    list_good(&app, "ore", "kg").await;

    let (status, body) = post(
        &app,
        None,
        "/api/symbols/ore/dividend",
        json!({ "cents_per_share": 10 }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("is a good"),
        "{body}"
    );

    let (status, body) = post(
        &app,
        None,
        "/api/symbols/ore/delist",
        json!({ "cents_per_share": 10 }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("is a good"),
        "{body}"
    );

    // Refused, and so still listed and still worth nothing.
    let (status, _) = get(&app, None, "/api/symbols/ore").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn a_listing_is_a_stock_unless_it_says_otherwise() {
    let app = test_app();
    let (status, body) = post(
        &app,
        None,
        "/api/symbols",
        json!({
            "symbol": "wdgt",
            "shares_outstanding": 1_000,
            "start_price_cents": 5_000,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["quote"]["asset_kind"], "stock");
    assert_eq!(body["quote"]["unit"], Value::Null);
    assert_eq!(body["quote"]["shares_outstanding"], 1_000);

    // A stock still needs shares, and an unknown kind is not guessed at.
    let (status, body) = post(
        &app,
        None,
        "/api/symbols",
        json!({ "symbol": "noth", "shares_outstanding": 0, "start_price_cents": 100 }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = post(
        &app,
        None,
        "/api/symbols",
        json!({ "symbol": "land", "kind": "parcel", "start_price_cents": 100 }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("parcel"),
        "{body}"
    );
}

#[tokio::test]
async fn the_audit_holds_over_a_world_with_goods_in_it() {
    let app = test_app();
    list_good(&app, "ore", "kg").await;
    list_good(&app, "ingot", "bar").await;
    engine::advance_to(&app, Timestamp(NOW_MS + 30_000)).await;
    let (status, body) = get(&app, None, "/api/reconcile").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["valid"], true, "{body}");
    assert_eq!(body["symbols_checked"], 6);
}

#[tokio::test]
async fn buying_a_good_makes_units_and_moves_currency_without_making_any() {
    let app = test_app();
    list_good(&app, "ore", "kg").await;
    let item = stock_catalog(&app, "ORE", 250, Some(1_000)).await;
    assert_eq!(item["price_cents"], 250);
    assert_eq!(item["available"], 1_000);
    assert_eq!(item["issued"], 0);

    let player = sign_up(&app, "wanda", 1_000_000).await;
    let before = supply(&app).await;

    let (status, receipt) = post(
        &app,
        Some(&player.key),
        &format!("/api/traders/{}/purchases", player.id),
        json!({ "symbol": "ore", "qty": 100 }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{receipt}");
    assert_eq!(receipt["qty"], 100);
    assert_eq!(receipt["unit_price_cents"], 250);
    assert_eq!(receipt["total_cents"], 25_000);
    assert_eq!(receipt["position_qty"], 100);
    assert_eq!(receipt["units_outstanding"], 100);
    assert_eq!(receipt["available"], 900);

    // Units were made. Currency was not: it moved from the player to the
    // good's issuer, and the world holds exactly as much as it did.
    let after = supply(&app).await;
    assert_eq!(after["minted_cents"], before["minted_cents"]);
    assert_eq!(after["burned_cents"], before["burned_cents"]);
    assert_eq!(after["outstanding_cents"], before["outstanding_cents"]);
    assert_eq!(after["balanced"], true);
    assert_eq!(
        after["player_cents"].as_i64().unwrap(),
        before["player_cents"].as_i64().unwrap() - 25_000
    );
    assert_eq!(
        after["issuer_cents"].as_i64().unwrap(),
        before["issuer_cents"].as_i64().unwrap() + 25_000
    );

    // The player's own view: a position in a good, and a row saying what it
    // cost.
    let (_, trader) = get(
        &app,
        Some(&player.key),
        &format!("/api/traders/{}", player.id),
    )
    .await;
    let ore = trader["positions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["symbol"] == "ORE")
        .expect("a position in ORE");
    assert_eq!(ore["qty"], 100);
    assert_eq!(ore["free_shares"], 100);

    let (_, quote) = get(&app, None, "/api/symbols/ore").await;
    assert_eq!(quote["quote"]["shares_outstanding"], 100);
    reconciles(&app).await;
}

#[tokio::test]
async fn consuming_a_good_destroys_units_and_moves_no_currency() {
    let app = test_app();
    list_good(&app, "ore", "kg").await;
    stock_catalog(&app, "ORE", 250, None).await;
    let player = sign_up(&app, "wanda", 1_000_000).await;
    post(
        &app,
        Some(&player.key),
        &format!("/api/traders/{}/purchases", player.id),
        json!({ "symbol": "ORE", "qty": 60 }),
    )
    .await;
    let before = supply(&app).await;

    let (status, receipt) = post(
        &app,
        Some(&player.key),
        &format!("/api/traders/{}/consume", player.id),
        json!({ "symbol": "ORE", "qty": 25 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    assert_eq!(receipt["qty"], 25);
    assert_eq!(receipt["position_qty"], 35);
    assert_eq!(receipt["units_outstanding"], 35);

    let after = supply(&app).await;
    assert_eq!(after["outstanding_cents"], before["outstanding_cents"]);
    assert_eq!(after["player_cents"], before["player_cents"]);
    assert_eq!(after["balanced"], true);

    let (_, detail) = get(&app, None, "/api/symbols/ore").await;
    assert_eq!(detail["info"]["asset"]["issued"], 60);
    assert_eq!(detail["info"]["asset"]["consumed"], 25);
    reconciles(&app).await;

    // More than is held is refused, and nothing is destroyed by the asking.
    let (status, body) = post(
        &app,
        Some(&player.key),
        &format!("/api/traders/{}/consume", player.id),
        json!({ "symbol": "ORE", "qty": 36 }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "insufficient_inventory");
    let (_, detail) = get(&app, None, "/api/symbols/ore").await;
    assert_eq!(detail["info"]["asset"]["consumed"], 25);
    reconciles(&app).await;
}

#[tokio::test]
async fn units_promised_to_a_resting_sell_cannot_be_eaten() {
    let app = test_app();
    list_good(&app, "ore", "kg").await;
    stock_catalog(&app, "ORE", 250, None).await;
    let player = sign_up(&app, "wanda", 1_000_000).await;
    post(
        &app,
        Some(&player.key),
        &format!("/api/traders/{}/purchases", player.id),
        json!({ "symbol": "ORE", "qty": 40 }),
    )
    .await;
    let (status, order) = post(
        &app,
        Some(&player.key),
        "/api/symbols/ORE/orders",
        json!({
            "trader_id": player.id, "side": "sell", "qty": 30,
            "type": "limit", "price_cents": 400
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{order}");
    assert_eq!(order["status"], "resting", "nothing else is quoting ORE");

    let (status, body) = post(
        &app,
        Some(&player.key),
        &format!("/api/traders/{}/consume", player.id),
        json!({ "symbol": "ORE", "qty": 11 }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    // The ten that are not promised away still can be.
    let (status, receipt) = post(
        &app,
        Some(&player.key),
        &format!("/api/traders/{}/consume", player.id),
        json!({ "symbol": "ORE", "qty": 10 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    assert_eq!(receipt["position_qty"], 30);
    reconciles(&app).await;
}

#[tokio::test]
async fn a_good_changes_hands_between_two_players() {
    let app = test_app();
    list_good(&app, "ore", "kg").await;
    stock_catalog(&app, "ORE", 250, None).await;
    let seller = sign_up(&app, "wanda", 1_000_000).await;
    let buyer = sign_up(&app, "vic", 1_000_000).await;
    post(
        &app,
        Some(&seller.key),
        &format!("/api/traders/{}/purchases", seller.id),
        json!({ "symbol": "ORE", "qty": 100 }),
    )
    .await;
    let before = supply(&app).await;

    let (status, ask) = post(
        &app,
        Some(&seller.key),
        "/api/symbols/ORE/orders",
        json!({
            "trader_id": seller.id, "side": "sell", "qty": 100,
            "type": "limit", "price_cents": 400
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{ask}");
    assert_eq!(ask["status"], "resting");

    // A partial fill: the buyer takes 40 of the 100 on offer.
    let (status, bid) = post(
        &app,
        Some(&buyer.key),
        "/api/symbols/ORE/orders",
        json!({
            "trader_id": buyer.id, "side": "buy", "qty": 40,
            "type": "limit", "price_cents": 400
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{bid}");
    assert_eq!(bid["status"], "filled", "{bid}");
    assert_eq!(bid["filled"], 40);

    // The units moved between players; none were made or destroyed.
    let (_, detail) = get(&app, None, "/api/symbols/ore").await;
    assert_eq!(detail["info"]["asset"]["issued"], 100);
    assert_eq!(detail["info"]["asset"]["consumed"], 0);
    let (_, shares) = get(&app, None, "/api/symbols/ore/shares").await;
    assert_eq!(shares["shares_outstanding"], 100);
    assert_eq!(shares["held_shares"], 100);

    // And so did the money, between the two of them and nowhere else.
    let after = supply(&app).await;
    assert_eq!(after["outstanding_cents"], before["outstanding_cents"]);
    assert_eq!(after["player_cents"], before["player_cents"]);
    assert_eq!(after["issuer_cents"], before["issuer_cents"]);
    assert_eq!(after["balanced"], true);
    assert_eq!(
        after["synthetic_debt_cents"], before["synthetic_debt_cents"],
        "no fill against liquidity nobody funded"
    );
    reconciles(&app).await;
}

#[tokio::test]
async fn the_catalogue_is_a_line_per_good_and_it_runs_out() {
    let app = test_app();
    list_good(&app, "ore", "kg").await;

    // A stock is not made to order.
    let (status, body) = post(
        &app,
        None,
        "/api/catalog",
        json!({ "symbol": "ACME", "price_cents": 100 }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "not_a_good");

    // Nor is a free one.
    let (status, body) = post(
        &app,
        None,
        "/api/catalog",
        json!({ "symbol": "ORE", "price_cents": 0 }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    stock_catalog(&app, "ORE", 100, Some(30)).await;
    let (status, listed) = get(&app, None, "/api/catalog").await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert_eq!(listed["items"].as_array().unwrap().len(), 1);
    assert_eq!(listed["items"][0]["symbol"], "ORE");

    let player = sign_up(&app, "wanda", 1_000_000).await;
    let (status, body) = post(
        &app,
        Some(&player.key),
        &format!("/api/traders/{}/purchases", player.id),
        json!({ "symbol": "ORE", "qty": 31 }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "catalog_exhausted");
    let (status, receipt) = post(
        &app,
        Some(&player.key),
        &format!("/api/traders/{}/purchases", player.id),
        json!({ "symbol": "ORE", "qty": 30 }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{receipt}");
    assert_eq!(receipt["available"], 0);

    // Taking the line away stops the making, and leaves what was made.
    let (status, removed) = call(
        &app,
        None,
        Request::builder()
            .method(Method::DELETE)
            .uri("/api/catalog/ore")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{removed}");
    assert_eq!(removed["issued"], 30);
    let (status, body) = post(
        &app,
        Some(&player.key),
        &format!("/api/traders/{}/purchases", player.id),
        json!({ "symbol": "ORE", "qty": 1 }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"]["code"], "not_in_catalog");
    let (_, detail) = get(&app, None, "/api/symbols/ore").await;
    assert_eq!(detail["info"]["asset"]["issued"], 30);
    reconciles(&app).await;
}

#[tokio::test]
async fn a_purchase_is_not_paid_for_twice_by_a_retry() {
    let app = test_app();
    list_good(&app, "ore", "kg").await;
    stock_catalog(&app, "ORE", 250, None).await;
    let player = sign_up(&app, "wanda", 1_000_000).await;
    let uri = format!("/api/traders/{}/purchases", player.id);
    let send = async |app: &Arc<App>| {
        call(
            app,
            Some(&player.key),
            Request::post(&uri)
                .header(header::CONTENT_TYPE, "application/json")
                .header("idempotency-key", "buy-once")
                .body(Body::from(
                    json!({ "symbol": "ORE", "qty": 20 }).to_string(),
                ))
                .unwrap(),
        )
        .await
    };
    let (status, first) = send(&app).await;
    assert_eq!(status, StatusCode::CREATED, "{first}");
    let (status, again) = send(&app).await;
    assert_eq!(status, StatusCode::CREATED, "{again}");
    assert_eq!(first["tx_id"], again["tx_id"], "the same purchase");

    let (_, detail) = get(&app, None, "/api/symbols/ore").await;
    assert_eq!(
        detail["info"]["asset"]["issued"], 20,
        "one delivery, one set of units"
    );
    reconciles(&app).await;
}

#[tokio::test]
async fn a_world_with_goods_in_it_comes_back_whole() {
    let dir = tempdir_lite::TempDir::new("fehu-goods");
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
    let (status, body) = post(&before, None, "/api/symbols", good("ore", "kg")).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    stock_catalog(&before, "ORE", 250, Some(400)).await;
    let player = sign_up(&before, "wanda", 1_000_000).await;
    post(
        &before,
        Some(&player.key),
        &format!("/api/traders/{}/purchases", player.id),
        json!({ "symbol": "ORE", "qty": 90 }),
    )
    .await;
    post(
        &before,
        Some(&player.key),
        &format!("/api/traders/{}/consume", player.id),
        json!({ "symbol": "ORE", "qty": 15 }),
    )
    .await;
    let (_, catalog) = get(&before, None, "/api/catalog").await;
    let (_, detail) = get(&before, None, "/api/symbols/ore").await;
    let (_, portfolio) = get(
        &before,
        Some(&player.key),
        &format!("/api/traders/{}", player.id),
    )
    .await;

    save::write(&before, &path).await.expect("state written");
    let after = App::restore(options(), save::read(&path).unwrap());

    let (_, restored_catalog) = get(&after, None, "/api/catalog").await;
    assert_eq!(restored_catalog, catalog, "what the world will still make");
    let (_, restored_detail) = get(&after, None, "/api/symbols/ore").await;
    assert_eq!(
        restored_detail["info"], detail["info"],
        "issued and consumed both survive"
    );
    let (_, restored_portfolio) = get(
        &after,
        Some(&player.key),
        &format!("/api/traders/{}", player.id),
    )
    .await;
    assert_eq!(restored_portfolio, portfolio, "the inventory");
    reconciles(&after).await;

    // And the good is still a good: no ladder appears under it on restart.
    engine::advance_to(&after, Timestamp(NOW_MS + 30_000)).await;
    let (_, book) = get(&after, None, "/api/symbols/ore/book").await;
    assert!(book["bids"].as_array().unwrap().is_empty());
    assert!(book["asks"].as_array().unwrap().is_empty());
    reconciles(&after).await;
}

/// A throwaway directory that cleans up after itself, as in `save.rs`.
/// `tempfile` is not a dependency of this workspace.
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
