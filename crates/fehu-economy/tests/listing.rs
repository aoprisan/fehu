//! Listing and delisting symbols while the server runs.
//!
//! The symbol set used to be the build's. These tests are about what happens
//! when it is not: that a symbol listed over HTTP is a symbol like any other,
//! and that delisting one gives back everything it was holding — reserved
//! cash, promised shares, and the money the shares themselves were worth —
//! rather than dropping it on the floor.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use fehu::Timestamp;
use fehu_economy::market::{App, Options};
use fehu_economy::{engine, router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::util::ServiceExt;

/// 2023-11-14T22:13:20Z, as in the other suites.
const NOW_MS: i64 = 1_700_000_000_000;

/// A small, quick market with rate limiting off: a test sends its requests as
/// fast as the runtime will carry them, and the limiter has its own tests.
fn test_app() -> Arc<App> {
    App::new(Options {
        history_days: 1,
        warmup_hours: 1,
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

async fn sign_up(app: &Arc<App>, name: &str) -> Player {
    let (status, body) = post(app, None, "/api/traders", json!({ "name": name })).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    Player {
        id: body["id"].as_u64().unwrap(),
        key: body["api_key"].as_str().unwrap().to_owned(),
    }
}

/// The body of a listing request for a plain, cheap company.
fn listing(symbol: &str) -> Value {
    json!({
        "symbol": symbol,
        "name": "Widget Corp",
        "sector": "Industrials",
        "description": "Listed while the server was running.",
        "shares_outstanding": 1_000_000,
        "start_price_cents": 5_000,
    })
}

#[tokio::test]
async fn a_listed_symbol_is_a_symbol_like_any_other() {
    let app = test_app();
    let (status, body) = post(&app, None, "/api/symbols", listing("wdgt")).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["quote"]["symbol"], "WDGT", "a ticker is upper-cased");
    assert_eq!(body["quote"]["price_cents"], 5_000);
    assert_eq!(body["quote"]["shares_outstanding"], 1_000_000);
    assert_eq!(body["event"]["kind"], "corporate:listing");

    // It is quoted alongside the seeded four, and has its own detail page.
    let (_, body) = get(&app, None, "/api/symbols").await;
    let tickers: Vec<&str> = body["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["symbol"].as_str().unwrap())
        .collect();
    assert_eq!(tickers, ["ACME", "NBLA", "HLIO", "PXCO", "WDGT"]);
    let (status, detail) = get(&app, None, "/api/symbols/wdgt").await;
    assert_eq!(status, StatusCode::OK, "lookup is case-insensitive");
    assert_eq!(detail["info"]["name"], "Widget Corp");
    assert_eq!(detail["config"]["start_price_cents"], 5_000);
    let (_, health) = get(&app, None, "/api/health").await;
    assert_eq!(health["symbols"], 5);

    // It ticks with the rest of the market, and it can be traded.
    assert_eq!(
        engine::advance_to(&app, Timestamp(NOW_MS + 1_000)).await,
        5,
        "every listed symbol steps, the new one included"
    );
    let player = sign_up(&app, "wanda").await;
    let (status, order) = post(
        &app,
        Some(&player.key),
        "/api/symbols/WDGT/orders",
        json!({ "trader_id": player.id, "side": "buy", "qty": 10, "type": "market" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{order}");
    assert_eq!(order["status"], "filled", "{order}");
    let (_, report) = get(&app, None, "/api/reconcile").await;
    assert_eq!(report["valid"], true, "{report}");
}

#[tokio::test]
async fn a_listing_is_refused_when_it_is_not_one() {
    let app = test_app();

    let (status, body) = post(&app, None, "/api/symbols", listing("ACME")).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (status, _) = post(&app, None, "/api/symbols", listing("acme")).await;
    assert_eq!(status, StatusCode::CONFLICT, "a ticker is one ticker");

    for bad in ["", "  ", "TOOLONGATICKER", "9LIVES", "A B"] {
        let (status, body) = post(&app, None, "/api/symbols", listing(bad)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad:?}: {body}");
    }

    let mut no_shares = listing("NONE");
    no_shares["shares_outstanding"] = json!(0);
    let (status, body) = post(&app, None, "/api/symbols", no_shares).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // A price the simulator will not take is the simulator's answer, not a
    // panic on the request thread.
    let mut free = listing("FREE");
    free["start_price_cents"] = json!(0);
    let (status, body) = post(&app, None, "/api/symbols", free).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // Nothing above was listed.
    let (_, body) = get(&app, None, "/api/symbols").await;
    assert_eq!(body["symbols"].as_array().unwrap().len(), 4);
}

#[tokio::test]
async fn a_market_lists_only_so_many_symbols() {
    let app = App::new(Options {
        history_days: 0,
        warmup_hours: 0,
        now_ms: Some(NOW_MS),
        rate_per_sec: 0.0,
        max_symbols: 5,
        ..Options::default()
    });
    let (status, _) = post(&app, None, "/api/symbols", listing("AAA")).await;
    assert_eq!(status, StatusCode::CREATED, "the fifth fits");
    let (status, body) = post(&app, None, "/api/symbols", listing("BBB")).await;
    assert_eq!(status, StatusCode::CONFLICT, "the sixth does not: {body}");
    assert!(body["error"]["message"].as_str().unwrap().contains("5"));
}

#[tokio::test]
async fn the_same_ticker_listed_twice_is_the_same_company_the_second_time() {
    let app = test_app();
    post(&app, None, "/api/symbols", listing("WDGT")).await;
    let (_, first) = get(&app, None, "/api/symbols/WDGT").await;
    post(&app, None, "/api/symbols/WDGT/delist", json!({})).await;
    post(&app, None, "/api/symbols", listing("WDGT")).await;
    let (status, second) = get(&app, None, "/api/symbols/WDGT").await;
    assert_eq!(status, StatusCode::OK, "a ticker can be listed again");
    assert_eq!(
        first["info"]["seed"], second["info"]["seed"],
        "a listing with no seed takes one from its ticker, so it is the same \
         company both times"
    );
}

#[tokio::test]
async fn delisting_gives_back_everything_the_symbol_was_holding() {
    let app = test_app();
    post(&app, None, "/api/symbols", listing("WDGT")).await;
    engine::advance_to(&app, Timestamp(NOW_MS + 1_000)).await;
    let player = sign_up(&app, "wanda").await;
    let key = Some(player.key.as_str());
    let trader = format!("/api/traders/{}", player.id);

    // Shares held, a resting buy holding cash, a resting sell holding shares,
    // and a stop that never fired.
    let (status, order) = post(
        &app,
        key,
        "/api/symbols/WDGT/orders",
        json!({ "trader_id": player.id, "side": "buy", "qty": 100, "type": "market" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{order}");
    let spent = order["notional_cents"].as_i64().unwrap();
    let (status, resting) = post(
        &app,
        key,
        "/api/symbols/WDGT/orders",
        json!({ "trader_id": player.id, "side": "buy", "qty": 50, "type": "limit", "price_cents": 100 }),
    )
    .await;
    assert_eq!(resting["status"], "resting", "{resting}");
    assert_eq!(status, StatusCode::CREATED);
    let (_, sell) = post(
        &app,
        key,
        "/api/symbols/WDGT/orders",
        json!({ "trader_id": player.id, "side": "sell", "qty": 40, "type": "limit", "price_cents": 900_000 }),
    )
    .await;
    assert_eq!(sell["status"], "resting", "{sell}");
    let (status, stop) = post(
        &app,
        key,
        "/api/symbols/WDGT/stops",
        json!({ "trader_id": player.id, "side": "sell", "qty": 10, "stop_price_cents": 1 }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{stop}");

    let (_, before) = get(&app, key, &trader).await;
    assert_eq!(before["reserved_cents"], 50 * 100);
    assert_eq!(before["positions"][0]["qty"], 100);
    assert_eq!(before["positions"][0]["reserved_shares"], 40);
    let cash_before = before["cash_cents"].as_i64().unwrap();
    assert_eq!(cash_before, 10_000_000 - spent);

    // Bought out at a round 60 cents a share.
    let (status, body) = post(
        &app,
        None,
        "/api/symbols/WDGT/delist",
        json!({ "cents_per_share": 60, "note": "taken private" }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let d = &body["delisting"];
    assert_eq!(d["cents_per_share"], 60);
    assert_eq!(d["shares_bought_out"], 100);
    assert_eq!(d["accounts_paid"], 1);
    assert_eq!(d["total_cents"], 6_000);
    assert_eq!(d["orders_cancelled"], 2, "the resting buy and the sell");
    assert_eq!(d["stops_cancelled"], 1);
    assert_eq!(body["event"]["kind"], "corporate:delisting");

    // The symbol is gone, and orders in it are refused.
    let (status, _) = get(&app, None, "/api/symbols/WDGT").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = post(
        &app,
        key,
        "/api/symbols/WDGT/orders",
        json!({ "trader_id": player.id, "side": "buy", "qty": 1, "type": "market" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // The trader has their reserved cash back, the buy-out on top, no
    // position, and no stop.
    let (_, after) = get(&app, key, &trader).await;
    assert_eq!(
        after["reserved_cents"], 0,
        "the resting buy released its cash"
    );
    assert_eq!(
        after["cash_cents"],
        cash_before + 6_000,
        "the shares were paid for"
    );
    assert!(
        after["positions"].as_array().unwrap().is_empty(),
        "no position in a symbol that no longer exists: {after}"
    );
    assert!(after["open_orders"].as_array().unwrap().is_empty());
    let (_, stops) = get(&app, key, &format!("/api/traders/{}/stops", player.id)).await;
    assert!(stops.as_array().unwrap().is_empty(), "{stops}");

    // The money is recorded, and the market still adds up.
    let account = after["account_id"].as_u64().unwrap();
    let (_, ledger) = get(&app, key, &format!("/api/accounts/{account}/ledger")).await;
    let payout = ledger["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "delisting")
        .unwrap_or_else(|| panic!("the buy-out is a ledger entry of its own: {ledger}"));
    assert_eq!(payout["amount_cents"], 6_000);
    assert_eq!(payout["symbol"], "WDGT");
    assert_eq!(payout["memo"], "taken private");
    let (_, report) = get(&app, None, "/api/reconcile").await;
    assert_eq!(report["valid"], true, "{report}");

    // And the history of the company that was is still there.
    let (status, record) = get(
        &app,
        key,
        &format!("/api/orders/{}", order["order_id"].as_u64().unwrap()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{record}");
    assert_eq!(record["symbol"], "WDGT");
}

#[tokio::test]
async fn a_company_can_turn_out_to_be_worth_nothing() {
    let app = test_app();
    post(&app, None, "/api/symbols", listing("WDGT")).await;
    engine::advance_to(&app, Timestamp(NOW_MS + 1_000)).await;
    let player = sign_up(&app, "wanda").await;
    let key = Some(player.key.as_str());
    post(
        &app,
        key,
        "/api/symbols/WDGT/orders",
        json!({ "trader_id": player.id, "side": "buy", "qty": 100, "type": "market" }),
    )
    .await;
    let (_, before) = get(&app, key, &format!("/api/traders/{}", player.id)).await;
    let cash = before["cash_cents"].as_i64().unwrap();

    let (status, body) = post(
        &app,
        None,
        "/api/symbols/WDGT/delist",
        json!({ "cents_per_share": 0 }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["delisting"]["shares_bought_out"], 100);
    assert_eq!(body["delisting"]["total_cents"], 0);
    assert_eq!(
        body["delisting"]["accounts_paid"], 0,
        "nobody is credited nothing"
    );

    let (_, after) = get(&app, key, &format!("/api/traders/{}", player.id)).await;
    assert_eq!(after["cash_cents"], cash, "the shares were worth nothing");
    assert!(after["positions"].as_array().unwrap().is_empty());
    let (_, report) = get(&app, None, "/api/reconcile").await;
    assert_eq!(report["valid"], true, "{report}");

    // A negative price is not a price.
    post(&app, None, "/api/symbols", listing("ZZZZ")).await;
    let (status, _) = post(
        &app,
        None,
        "/api/symbols/ZZZZ/delist",
        json!({ "cents_per_share": -1 }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn an_order_id_is_never_reissued_after_a_delisting() {
    let app = test_app();
    post(&app, None, "/api/symbols", listing("WDGT")).await;
    engine::advance_to(&app, Timestamp(NOW_MS + 1_000)).await;
    let player = sign_up(&app, "wanda").await;
    let key = Some(player.key.as_str());

    // The new symbol's book hands out the highest ids in the market, because
    // it is the one being traded.
    let mut highest = 0;
    for _ in 0..3 {
        let (_, body) = post(
            &app,
            key,
            "/api/symbols/WDGT/orders",
            json!({ "trader_id": player.id, "side": "buy", "qty": 5, "type": "market" }),
        )
        .await;
        highest = highest.max(body["order_id"].as_u64().unwrap());
    }
    assert!(highest > 0);

    // Delisting takes that book away, and with it its counter. The next order
    // must still land above every id the log already holds.
    post(&app, None, "/api/symbols/WDGT/delist", json!({})).await;
    let (_, body) = post(
        &app,
        key,
        "/api/symbols/ACME/orders",
        json!({ "trader_id": player.id, "side": "buy", "qty": 1, "type": "market" }),
    )
    .await;
    let next = body["order_id"].as_u64().unwrap();
    assert!(
        next > highest,
        "an id from a delisted book must not come round again: {next} after {highest}"
    );
    let (_, record) = get(&app, key, &format!("/api/orders/{highest}")).await;
    assert_eq!(
        record["symbol"], "WDGT",
        "the old id still names the old order"
    );
}

#[tokio::test]
async fn only_the_game_master_lists_and_delists() {
    let app = App::new(Options {
        history_days: 0,
        warmup_hours: 0,
        now_ms: Some(NOW_MS),
        rate_per_sec: 0.0,
        admin_key: Some("s3cret".into()),
        ..Options::default()
    });
    let player = sign_up(&app, "wanda").await;

    let (status, _) = post(&app, None, "/api/symbols", listing("WDGT")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "no key");
    let (status, _) = post(&app, Some(&player.key), "/api/symbols", listing("WDGT")).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "a player is not the master"
    );
    let (status, body) = post(&app, Some("s3cret"), "/api/symbols", listing("WDGT")).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let (status, _) = post(&app, None, "/api/symbols/WDGT/delist", json!({})).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = post(
        &app,
        Some(&player.key),
        "/api/symbols/WDGT/delist",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, body) = post(&app, Some("s3cret"), "/api/symbols/WDGT/delist", json!({})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
}
