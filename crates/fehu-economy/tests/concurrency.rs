//! The market with several things happening at once: requests on every
//! symbol from several players while the engine steps, a delisting under a
//! request that already had the symbol in hand, and reads that must not
//! queue behind the money. These are the promises the actor design makes;
//! this is where they are held to.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use fehu::Timestamp;
use fehu_economy::market::{App, Options};
use fehu_economy::{engine, router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::util::ServiceExt;

/// 2023-11-14T22:13:20Z. Fixed so the tests are deterministic.
const NOW_MS: i64 = 1_700_000_000_000;

const TICKERS: [&str; 4] = ["ACME", "NBLA", "HLIO", "PXCO"];

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

/// A signed-up player: their trader id and their key.
async fn sign_up(app: &Arc<App>, name: &str) -> (u64, String) {
    let (status, body) = post(app, None, "/api/traders", json!({ "name": name })).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    (
        body["id"].as_u64().unwrap(),
        body["api_key"].as_str().unwrap().to_owned(),
    )
}

/// The reference price of `ticker`, in cents.
async fn reference(app: &Arc<App>, ticker: &str) -> i64 {
    get(app, None, &format!("/api/symbols/{ticker}/book"))
        .await
        .1["reference_cents"]
        .as_i64()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn order_ids_stay_unique_across_books_under_concurrent_submissions() {
    let app = test_app();
    let mut refs = Vec::new();
    for ticker in TICKERS {
        refs.push(reference(&app, ticker).await);
    }
    let mut tasks = tokio::task::JoinSet::new();
    for p in 0..8 {
        let app = Arc::clone(&app);
        let refs = refs.clone();
        tasks.spawn(async move {
            let (trader, key) = sign_up(&app, &format!("player-{p}")).await;
            let mut ids = Vec::new();
            for i in 0..24 {
                // Round-robin over the books, resting well below the market
                // so every order stays in its book and reserves its cash.
                let ticker = TICKERS[(p + i) % TICKERS.len()];
                let price = (refs[(p + i) % TICKERS.len()] / 2).max(1);
                let (status, body) = post(
                    &app,
                    Some(&key),
                    &format!("/api/symbols/{ticker}/orders"),
                    json!({ "trader_id": trader, "side": "buy", "qty": 1, "type": "limit", "price_cents": price }),
                )
                .await;
                assert_eq!(status, StatusCode::CREATED, "{body}");
                ids.push(body["order_id"].as_u64().unwrap());
            }
            ids
        });
    }
    let mut all = Vec::new();
    while let Some(ids) = tasks.join_next().await {
        all.extend(ids.unwrap());
    }
    let unique: BTreeSet<u64> = all.iter().copied().collect();
    assert_eq!(
        unique.len(),
        all.len(),
        "every trader order across every book gets an id of its own"
    );
    assert_eq!(all.len(), 8 * 24);
    let report = app.reconcile().await;
    assert!(report.valid, "{:?}", report.issues);
    assert_eq!(report.resting_orders_checked, 8 * 24);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_engine_steps_while_orders_arrive_and_the_books_still_add_up() {
    let app = test_app();
    let mut tasks = tokio::task::JoinSet::new();
    // The engine, stepping every symbol at once, over and over.
    {
        let app = Arc::clone(&app);
        tasks.spawn(async move {
            for i in 1..=40 {
                engine::advance_to(&app, Timestamp(NOW_MS + i * 1_000)).await;
                tokio::task::yield_now().await;
            }
            0
        });
    }
    // Players trading against the synthetic ladder in every book while it
    // moves: market buys, then sells of what they got, then a cancel-all.
    for p in 0..6 {
        let app = Arc::clone(&app);
        tasks.spawn(async move {
            let (trader, key) = sign_up(&app, &format!("trader-{p}")).await;
            let mut fills = 0;
            for i in 0..12 {
                let ticker = TICKERS[(p + i) % TICKERS.len()];
                let (status, body) = post(
                    &app,
                    Some(&key),
                    &format!("/api/symbols/{ticker}/orders"),
                    json!({ "trader_id": trader, "side": "buy", "qty": 10, "type": "market" }),
                )
                .await;
                assert!(
                    status == StatusCode::CREATED || status == StatusCode::UNPROCESSABLE_ENTITY,
                    "{body}"
                );
                if status == StatusCode::CREATED {
                    fills += body["filled"].as_u64().unwrap();
                }
            }
            let (status, portfolio) =
                get(&app, Some(&key), &format!("/api/traders/{trader}")).await;
            assert_eq!(status, StatusCode::OK, "{portfolio}");
            for position in portfolio["positions"].as_array().unwrap() {
                let ticker = position["symbol"].as_str().unwrap();
                let free = position["free_shares"].as_u64().unwrap();
                if free == 0 {
                    continue;
                }
                let (status, body) = post(
                    &app,
                    Some(&key),
                    &format!("/api/symbols/{ticker}/orders"),
                    json!({ "trader_id": trader, "side": "sell", "qty": free, "type": "market" }),
                )
                .await;
                assert_eq!(status, StatusCode::CREATED, "{body}");
            }
            let (status, _) = post(
                &app,
                Some(&key),
                &format!("/api/traders/{trader}/cancel_all"),
                json!({}),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            fills
        });
    }
    let mut filled = 0;
    while let Some(done) = tasks.join_next().await {
        filled += done.unwrap();
    }
    assert!(filled > 0, "somebody traded");
    let report = app.reconcile().await;
    assert!(report.valid, "{:?}", report.issues);
    let (_, health) = get(&app, None, "/api/health").await;
    assert!(health["ticks_total"].as_u64().unwrap() >= 4 * 40);
    assert!(health["fills_booked"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn a_symbol_in_hand_when_it_is_delisted_answers_as_gone() {
    let app = test_app();
    let (trader, key) = sign_up(&app, "late").await;
    // A request that looked the symbol up a moment before the delisting.
    let handle = app.symbol("PXCO").expect("PXCO is listed");
    let (status, body) = post(&app, None, "/api/symbols/PXCO/delist", json!({})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert!(app.symbol("PXCO").is_none(), "gone from the table");
    let answered = handle.ask_listed(|s| s.info.symbol).await.unwrap();
    assert_eq!(answered, None, "the handle knows the symbol is delisted");
    let (status, body) = post(
        &app,
        Some(&key),
        "/api/symbols/PXCO/orders",
        json!({ "trader_id": trader, "side": "buy", "qty": 1, "type": "market" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(app.reconcile().await.valid);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn market_data_is_served_while_the_market_actor_is_busy() {
    let app = test_app();
    // Keep the market actor occupied for a while, as a long job would.
    let busy = {
        let app = Arc::clone(&app);
        tokio::spawn(async move {
            app.market
                .call_async(|_| Box::pin(tokio::time::sleep(Duration::from_millis(400))))
                .await
                .unwrap();
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    // A quote, a book and the bars come from the symbol's own actor and do
    // not wait for the market.
    let started = std::time::Instant::now();
    for uri in [
        "/api/symbols",
        "/api/symbols/ACME/book",
        "/api/symbols/ACME/bars?interval=M1&limit=5",
        "/api/symbols/ACME/status",
    ] {
        let (status, body) = get(&app, None, uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
    }
    assert!(
        started.elapsed() < Duration::from_millis(300),
        "reads waited on the market: {:?}",
        started.elapsed()
    );
    busy.await.unwrap();
}
