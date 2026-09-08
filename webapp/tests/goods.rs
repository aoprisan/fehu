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
use axum::http::{Request, StatusCode, header};
use fehu::Timestamp;
use fehu_webapp::market::{App, Options};
use fehu_webapp::{engine, router};
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
