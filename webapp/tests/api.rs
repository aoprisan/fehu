//! End-to-end tests over the router, without a TCP listener.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use fehu::Timestamp;
use fehu_webapp::{App, Options, engine, router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::util::ServiceExt;

/// 2023-11-14T22:13:20Z. Fixed so the tests are deterministic.
const NOW_MS: i64 = 1_700_000_000_000;

fn test_app() -> Arc<App> {
    App::new(Options {
        history_days: 5,
        warmup_hours: 1,
        now_ms: Some(NOW_MS),
        ..Options::default()
    })
}

async fn call(app: &Arc<App>, req: Request<Body>) -> (StatusCode, Value) {
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

async fn get(app: &Arc<App>, uri: &str) -> (StatusCode, Value) {
    call(app, Request::get(uri).body(Body::empty()).unwrap()).await
}

async fn post(app: &Arc<App>, uri: &str, body: Value) -> (StatusCode, Value) {
    call(
        app,
        Request::post(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await
}

#[tokio::test]
async fn health_and_symbols() {
    let app = test_app();
    let (status, body) = get(&app, "/api/health").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");
    assert_eq!(body["symbols"], 4);

    let (status, body) = get(&app, "/api/symbols").await;
    assert_eq!(status, StatusCode::OK);
    let symbols = body["symbols"].as_array().unwrap();
    let tickers: Vec<&str> = symbols
        .iter()
        .map(|s| s["symbol"].as_str().unwrap())
        .collect();
    assert_eq!(tickers, ["ACME", "NBLA", "HLIO", "PXCO"]);
    for s in symbols {
        assert!(s["price_cents"].as_i64().unwrap() >= 1);
        assert!(s["prev_close_cents"].as_i64().is_some());
        assert!(s["change_pct"].as_f64().is_some());
    }

    let (status, body) = get(&app, "/api/symbols/nbla").await;
    assert_eq!(status, StatusCode::OK, "lookup is case-insensitive");
    assert_eq!(body["info"]["symbol"], "NBLA");
    assert_eq!(body["config"]["start_price_cents"], 31_255);

    let (status, body) = get(&app, "/api/symbols/NOPE").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "unknown_symbol");
}

#[tokio::test]
async fn bars_have_history_and_are_well_formed() {
    let app = test_app();
    for (iv, min_bars) in [("M1", 60), ("M5", 12), ("H1", 1), ("D1", 6)] {
        let (status, body) = get(
            &app,
            &format!("/api/symbols/ACME/bars?interval={iv}&limit=5000"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["interval"], iv);
        let bars = body["bars"].as_array().unwrap();
        assert!(bars.len() >= min_bars, "{iv}: {} bars", bars.len());
        let mut prev_ts = i64::MIN;
        for b in bars {
            let (o, h, l, c) = (
                b["open"].as_i64().unwrap(),
                b["high"].as_i64().unwrap(),
                b["low"].as_i64().unwrap(),
                b["close"].as_i64().unwrap(),
            );
            assert!(l <= o.min(c) && h >= o.max(c), "{iv}: bad OHLC {b}");
            let ts = b["open_ts"].as_i64().unwrap();
            assert!(ts > prev_ts, "{iv}: bars must be strictly increasing");
            prev_ts = ts;
        }
        // The last bar is the in-progress one, containing "now".
        assert!(prev_ts <= NOW_MS && NOW_MS < prev_ts + body["interval_ms"].as_i64().unwrap());
    }
    // Coarse pre-history (5 days) plus the fine day we ticked through.
    let (_, body) = get(&app, "/api/symbols/ACME/bars?interval=1d").await;
    let bars = body["bars"].as_array().unwrap();
    assert_eq!(bars.len(), 6);
    assert_eq!(bars[0]["ticks"], 0, "coarse bars carry no tick count");
    assert!(bars[5]["ticks"].as_u64().unwrap() > 1000);

    let (status, _) = get(&app, "/api/symbols/ACME/bars?interval=W1").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (_, body) = get(&app, "/api/symbols/ACME/bars?limit=3").await;
    assert_eq!(body["bars"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn game_event_moves_the_price() {
    let app = test_app();
    let before = get(&app, "/api/symbols/ACME").await.1["quote"]["price_cents"]
        .as_i64()
        .unwrap();

    let (status, body) = post(
        &app,
        "/api/game/events",
        json!({ "kind": "scandal", "symbol": "ACME", "note": "leaked memo", "source": "test" }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["id"], 1);
    assert_eq!(body["kind"], "game:scandal");
    assert_eq!(body["symbols"], json!(["ACME"]));
    assert_eq!(body["source"], "test");
    assert_eq!(body["note"], "leaked memo");
    // "now" is the sim clock, which tracks wall time since the app was built.
    let at = body["at_ms"].as_i64().unwrap();
    assert!((NOW_MS..NOW_MS + 5_000).contains(&at), "at_ms = {at}");
    assert_eq!(body["effects"].as_array().unwrap().len(), 4);
    assert_eq!(body["effects"][0], json!({ "type": "jump", "pct": -0.1 }));

    // Queued, not yet applied: nothing has ticked since.
    let snap = get(&app, "/api/symbols/ACME").await.1;
    assert_eq!(snap["snapshot"]["pending_events"], 4);

    // Tick ten seconds forward: the jump lands on the first tick.
    let ticks = engine::advance_to(&app, Timestamp(NOW_MS + 10_000));
    assert_eq!(ticks, 40, "4 symbols × 10 one-second ticks");
    let after = get(&app, "/api/symbols/ACME").await.1;
    assert_eq!(after["snapshot"]["pending_events"], 0);
    let price = after["quote"]["price_cents"].as_i64().unwrap();
    assert!(
        (price as f64) < before as f64 * 0.95,
        "expected a ~10% drop, got {before} -> {price}"
    );
    assert!(after["snapshot"]["vol_effect"].as_f64().unwrap() > 0.3);

    // Logged, newest first, and filterable by symbol.
    let (_, log) = get(&app, "/api/events").await;
    assert_eq!(log["events"][0]["id"], 1);
    let (_, log) = get(&app, "/api/symbols/NBLA/events").await;
    assert!(log["events"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn market_wide_event_hits_every_symbol() {
    let app = test_app();
    let (status, body) = post(
        &app,
        "/api/game/events",
        json!({ "kind": "market_crash", "magnitude": 2.0, "delay_secs": 30 }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["symbols"], json!(["ACME", "NBLA", "HLIO", "PXCO"]));
    assert_eq!(body["magnitude"], 2.0);
    let at = body["at_ms"].as_i64().unwrap();
    assert!(
        (NOW_MS + 30_000..NOW_MS + 35_000).contains(&at),
        "at_ms = {at}"
    );
    assert_eq!(body["effects"][0]["pct"], -0.16);
    for sym in ["ACME", "NBLA", "HLIO", "PXCO"] {
        let (_, s) = get(&app, &format!("/api/symbols/{sym}")).await;
        assert_eq!(s["snapshot"]["pending_events"], 3, "{sym}");
    }
}

#[tokio::test]
async fn raw_simulator_events() {
    let app = test_app();
    let (status, body) = post(
        &app,
        "/api/symbols/HLIO/events",
        json!({ "type": "drift_for_total_move", "total": 0.05, "half_life_secs": 3600 }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["kind"], "sim:drift_for_total_move");
    assert_eq!(body["source"], "api");
    assert_eq!(body["summary"][0], "drift for +5.0% over ~1h");

    let (status, body) = post(
        &app,
        "/api/symbols/HLIO/events",
        json!({ "type": "fundamental_target", "target_cents": 5000 }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["id"], 2);

    let (status, body) = get(&app, "/api/events?limit=1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["events"].as_array().unwrap().len(), 1);
    assert_eq!(body["events"][0]["id"], 2);
}

#[tokio::test]
async fn rejects_bad_events() {
    let app = test_app();
    let cases = [
        (
            "/api/symbols/ACME/events",
            json!({ "type": "jump", "pct": -1.5 }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_event",
        ),
        (
            "/api/symbols/ACME/events",
            json!({ "type": "vol_shift", "delta": 0.2, "half_life_secs": 0 }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_event",
        ),
        (
            "/api/symbols/ACME/events",
            json!({ "type": "teleport" }),
            StatusCode::BAD_REQUEST,
            "bad_json",
        ),
        (
            "/api/symbols/ACME/events",
            json!({ "type": "jump", "pct": 0.1, "at_ms": 1, "delay_secs": 1 }),
            StatusCode::BAD_REQUEST,
            "bad_request",
        ),
        (
            "/api/symbols/NOPE/events",
            json!({ "type": "jump", "pct": 0.1 }),
            StatusCode::NOT_FOUND,
            "unknown_symbol",
        ),
        (
            "/api/game/events",
            json!({ "kind": "hype" }),
            StatusCode::BAD_REQUEST,
            "bad_request",
        ),
        (
            "/api/game/events",
            json!({ "kind": "hype", "symbol": "ACME", "magnitude": 50 }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_event",
        ),
        (
            "/api/game/events",
            json!({ "kind": "alien_invasion", "symbol": "ACME" }),
            StatusCode::BAD_REQUEST,
            "bad_json",
        ),
    ];
    for (uri, body, want_status, want_code) in cases {
        let (status, resp) = post(&app, uri, body.clone()).await;
        assert_eq!(status, want_status, "{uri} {body}: {resp}");
        assert_eq!(resp["error"]["code"], want_code, "{uri} {body}: {resp}");
    }
    // Nothing leaked into the log or the queues.
    assert!(
        get(&app, "/api/events").await.1["events"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        get(&app, "/api/symbols/ACME").await.1["snapshot"]["pending_events"],
        0
    );
}

#[tokio::test]
async fn catalog_lists_every_kind() {
    let app = test_app();
    let (status, body) = get(&app, "/api/game/catalog").await;
    assert_eq!(status, StatusCode::OK);
    let entries = body.as_array().unwrap();
    assert_eq!(entries.len(), 12);
    let market: Vec<&str> = entries
        .iter()
        .filter(|e| e["scope"] == "market")
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        market,
        ["market_crash", "market_rally", "rate_hike", "rate_cut"]
    );
    for e in entries {
        assert!(!e["effects"].as_array().unwrap().is_empty(), "{e}");
    }
}

#[tokio::test]
async fn ui_is_served() {
    let app = test_app();
    let resp = router(app)
        .oneshot(Request::get("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        resp.headers()[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    let html = resp.into_body().collect().await.unwrap().to_bytes();
    assert!(std::str::from_utf8(&html).unwrap().contains("fehu market"));
}

#[tokio::test]
async fn engine_step_is_idempotent_within_a_second() {
    let app = test_app();
    // The sim clock is already at "now" after warm-up; stepping to the same
    // instant emits nothing.
    assert_eq!(engine::advance_to(&app, Timestamp(NOW_MS)), 0);
    assert_eq!(engine::advance_to(&app, Timestamp(NOW_MS + 999)), 0);
    assert_eq!(engine::advance_to(&app, Timestamp(NOW_MS + 1000)), 4);
}

// ---------------------------------------------------------------------------
// Trading

async fn delete(app: &Arc<App>, uri: &str) -> (StatusCode, Value) {
    call(app, Request::delete(uri).body(Body::empty()).unwrap()).await
}

async fn new_trader(app: &Arc<App>, name: &str) -> u64 {
    let (status, body) = post(app, "/api/traders", json!({ "name": name })).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["id"].as_u64().unwrap()
}

#[tokio::test]
async fn book_and_tape_are_live() {
    let app = test_app();
    let (status, body) = get(&app, "/api/symbols/ACME/book?depth=3").await;
    assert_eq!(status, StatusCode::OK);
    let bids = body["bids"].as_array().unwrap();
    let asks = body["asks"].as_array().unwrap();
    assert_eq!(bids.len(), 3);
    assert_eq!(asks.len(), 3);
    let r = body["reference_cents"].as_i64().unwrap();
    assert!(bids[0]["price_cents"].as_i64().unwrap() < r);
    assert!(asks[0]["price_cents"].as_i64().unwrap() > r);
    assert!(bids[0]["price_cents"].as_i64().unwrap() > bids[1]["price_cents"].as_i64().unwrap());
    assert_eq!(body["bid_cents"], bids[0]["price_cents"]);
    assert_eq!(body["pending_flow"], 0);

    // Warm-up runs on the bare simulator, so the tape starts empty and fills
    // from the first live tick.
    let (_, body) = get(&app, "/api/symbols/ACME/trades?limit=5").await;
    assert!(body["trades"].as_array().unwrap().is_empty());
    engine::advance_to(&app, Timestamp(NOW_MS + 5_000));
    let (status, body) = get(&app, "/api/symbols/ACME/trades?limit=5").await;
    assert_eq!(status, StatusCode::OK);
    let trades = body["trades"].as_array().unwrap();
    assert_eq!(trades.len(), 5, "five ticks printed a tape");
    assert!(trades[0]["ts_ms"].as_i64().unwrap() >= trades[4]["ts_ms"].as_i64().unwrap());
    for t in trades {
        assert!(t["taker_trader"].is_null());
        assert!(t["qty"].as_u64().unwrap() >= 1);
    }
    let (_, q) = get(&app, "/api/symbols").await;
    assert!(q["symbols"][0]["bid_cents"].as_i64().is_some());
    assert!(q["symbols"][0]["ask_cents"].as_i64().is_some());
}

#[tokio::test]
async fn market_order_fills_and_moves_the_price() {
    let app = test_app();
    // A rich trader, so 20 000 shares (~1.7 M) are affordable.
    let (status, body) = post(
        &app,
        "/api/traders",
        json!({ "name": "alice", "cash_cents": 1_000_000_000 }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["id"], 1);
    assert_eq!(body["name"], "alice");
    assert_eq!(body["cash_cents"], 1_000_000_000);
    assert_eq!(body["equity_cents"], 1_000_000_000);

    let before = get(&app, "/api/symbols/ACME/book").await.1;
    let ask = before["ask_cents"].as_i64().unwrap();
    let r0 = before["reference_cents"].as_i64().unwrap();

    let (status, body) = post(
        &app,
        "/api/symbols/ACME/orders",
        json!({ "trader_id": 1, "side": "buy", "qty": 20000, "type": "market" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["status"], "filled");
    assert_eq!(body["filled"], 20000);
    assert!(body["avg_price_cents"].as_f64().unwrap() >= ask as f64);
    let notional = body["notional_cents"].as_i64().unwrap();
    assert!(!body["trades"].as_array().unwrap().is_empty());
    assert_eq!(body["trades"][0]["taker_trader"], 1);
    assert!(body["trades"][0]["maker_trader"].is_null());

    let (_, p) = get(&app, "/api/traders/1").await;
    assert_eq!(p["cash_cents"], 1_000_000_000 - notional);
    assert_eq!(p["positions"][0]["symbol"], "ACME");
    assert_eq!(p["positions"][0]["qty"], 20000);
    assert_eq!(
        p["fills"].as_array().unwrap().len(),
        body["trades"].as_array().unwrap().len()
    );
    assert_eq!(p["fills"][0]["liquidity"], "taker");
    assert!(p["open_orders"].as_array().unwrap().is_empty());
    let (_, b) = get(&app, "/api/symbols/ACME/book").await;
    assert_eq!(b["pending_flow"], 20000);

    // The impact lands on the next tick and is a small positive move.
    engine::advance_to(&app, Timestamp(NOW_MS + 1000));
    let (_, b) = get(&app, "/api/symbols/ACME/book").await;
    assert_eq!(b["pending_flow"], 0);
    let r1 = b["reference_cents"].as_i64().unwrap();
    // 0.7 · 0.22/√365.25 · √(20000/6.49M) ≈ 0.045 %: about 4 cents on $84,
    // against a 1 s noise std of ~0.3 cents.
    assert!(r1 > r0 + 2, "{r0} -> {r1}");
    let (_, p) = get(&app, "/api/traders/1").await;
    assert_eq!(p["positions"][0]["mark_cents"], r1);
    let (_, h) = get(&app, "/api/health").await;
    assert_eq!(h["traders"], 1);
}

#[tokio::test]
async fn limit_orders_rest_reserve_and_cancel() {
    let app = test_app();
    let id = new_trader(&app, "bob").await;
    let book = get(&app, "/api/symbols/PXCO/book").await.1;
    let bid = book["bid_cents"].as_i64().unwrap();
    let price = bid - 20;
    let (status, body) = post(
        &app,
        "/api/symbols/PXCO/orders",
        json!({ "trader_id": id, "side": "buy", "qty": 100, "type": "limit", "price_cents": price }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["status"], "resting");
    assert_eq!(body["filled"], 0);
    let order_id = body["order_id"].as_u64().unwrap();

    let (_, p) = get(&app, &format!("/api/traders/{id}")).await;
    assert_eq!(p["reserved_cents"], 100 * price);
    assert_eq!(p["free_cash_cents"], 10_000_000 - 100 * price);
    assert_eq!(p["open_orders"][0]["order_id"], order_id);
    assert_eq!(p["open_orders"][0]["symbol"], "PXCO");
    let (_, orders) = get(&app, &format!("/api/symbols/PXCO/orders?trader_id={id}")).await;
    assert_eq!(orders.as_array().unwrap().len(), 1);
    let (status, o) = get(&app, &format!("/api/symbols/PXCO/orders/{order_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(o["remaining"], 100);
    // Visible in the book at its price.
    let (_, b) = get(&app, "/api/symbols/PXCO/book?depth=50").await;
    assert!(
        b["bids"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l["price_cents"] == price && l["qty"] == 100)
    );

    // Someone else cannot cancel it.
    let other = new_trader(&app, "mallory").await;
    let (status, _) = delete(
        &app,
        &format!("/api/symbols/PXCO/orders/{order_id}?trader_id={other}"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, body) = delete(
        &app,
        &format!("/api/symbols/PXCO/orders/{order_id}?trader_id={id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["remaining"], 100);
    let (_, p) = get(&app, &format!("/api/traders/{id}")).await;
    assert_eq!(p["reserved_cents"], 0);
    let (status, _) = get(&app, &format!("/api/symbols/PXCO/orders/{order_id}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // cancel_all sweeps every symbol.
    for sym in ["ACME", "HLIO"] {
        let b = get(&app, &format!("/api/symbols/{sym}/book")).await.1;
        let bid = b["bid_cents"].as_i64().unwrap();
        post(
            &app,
            &format!("/api/symbols/{sym}/orders"),
            json!({ "trader_id": id, "side": "buy", "qty": 10, "type": "limit", "price_cents": bid - 5 }),
        )
        .await;
    }
    let (status, body) = post(&app, &format!("/api/traders/{id}/cancel_all"), json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 2);
    let (_, p) = get(&app, &format!("/api/traders/{id}")).await;
    assert!(p["open_orders"].as_array().unwrap().is_empty());
    assert_eq!(p["reserved_cents"], 0);
}

#[tokio::test]
async fn resting_bid_fills_when_the_market_trades_through_it() {
    let app = test_app();
    let id = new_trader(&app, "carol").await;
    let book = get(&app, "/api/symbols/NBLA/book").await.1;
    let bid = book["bid_cents"].as_i64().unwrap();
    let ask = book["ask_cents"].as_i64().unwrap();
    // Inside the spread: the next synthetic sell print hits it.
    let price = (bid + ask) / 2;
    let (status, body) = post(
        &app,
        "/api/symbols/NBLA/orders",
        json!({ "trader_id": id, "side": "buy", "qty": 50, "type": "limit", "price_cents": price }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["status"], "resting");
    let mut filled = 0;
    for k in 1..=600 {
        engine::advance_to(&app, Timestamp(NOW_MS + k * 1000));
        let (_, p) = get(&app, &format!("/api/traders/{id}")).await;
        filled = p["positions"]
            .as_array()
            .unwrap()
            .first()
            .and_then(|q| q["qty"].as_i64())
            .unwrap_or(0);
        if filled == 50 {
            assert_eq!(p["reserved_cents"], 0);
            assert!(p["open_orders"].as_array().unwrap().is_empty());
            assert_eq!(p["fills"][0]["liquidity"], "maker");
            assert_eq!(p["fills"][0]["price_cents"], price);
            assert_eq!(p["fills"][0]["counterparty"], "synthetic");
            assert_eq!(p["cash_cents"], 10_000_000 - 50 * price);
            break;
        }
    }
    assert_eq!(filled, 50, "bid inside the spread never filled");
}

#[tokio::test]
async fn traders_can_trade_with_each_other() {
    let app = test_app();
    let a = new_trader(&app, "a").await;
    let b = new_trader(&app, "b").await;
    // a buys some stock, then offers it inside the spread; b lifts it.
    let (status, body) = post(
        &app,
        "/api/symbols/HLIO/orders",
        json!({ "trader_id": a, "side": "buy", "qty": 1000, "type": "market" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let book = get(&app, "/api/symbols/HLIO/book").await.1;
    let mid = (book["bid_cents"].as_i64().unwrap() + book["ask_cents"].as_i64().unwrap()) / 2;
    let (status, body) = post(
        &app,
        "/api/symbols/HLIO/orders",
        json!({ "trader_id": a, "side": "sell", "qty": 1000, "type": "limit", "price_cents": mid }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["status"], "resting");
    let (_, pa) = get(&app, &format!("/api/traders/{a}")).await;
    assert_eq!(pa["positions"][0]["reserved_shares"], 1000);
    let (status, body) = post(
        &app,
        "/api/symbols/HLIO/orders",
        json!({ "trader_id": b, "side": "buy", "qty": 600, "type": "limit", "price_cents": mid, "tif": "ioc" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["status"], "filled");
    assert_eq!(body["trades"][0]["maker_trader"], a);
    assert_eq!(body["trades"][0]["price_cents"], mid);
    let (_, pa) = get(&app, &format!("/api/traders/{a}")).await;
    assert_eq!(pa["positions"][0]["qty"], 400);
    assert_eq!(pa["positions"][0]["reserved_shares"], 400);
    assert_eq!(pa["open_orders"][0]["remaining"], 400);
    assert_eq!(pa["fills"][0]["counterparty"], "trader");
    let (_, pb) = get(&app, &format!("/api/traders/{b}")).await;
    assert_eq!(pb["positions"][0]["qty"], 600);
    // Trader-to-trader flow does not move the reference.
    let (_, book) = get(&app, "/api/symbols/HLIO/book").await;
    assert_eq!(book["pending_flow"], 1000, "only a's market buy is pending");
}

#[tokio::test]
async fn orders_are_checked_and_rejected() {
    let app = test_app();
    let id = new_trader(&app, "dan").await;
    let cases = [
        (
            json!({ "trader_id": id, "side": "buy", "qty": 10_000_000, "type": "market" }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "order_refused",
        ),
        (
            json!({ "trader_id": id, "side": "sell", "qty": 1, "type": "market" }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "order_refused",
        ),
        (
            json!({ "trader_id": id, "side": "buy", "qty": 0, "type": "market" }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_order",
        ),
        (
            json!({ "trader_id": id, "side": "buy", "qty": 1, "type": "limit", "price_cents": 0 }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_order",
        ),
        (
            json!({ "trader_id": 99, "side": "buy", "qty": 1, "type": "market" }),
            StatusCode::NOT_FOUND,
            "unknown_trader",
        ),
        (
            json!({ "trader_id": id, "side": "up", "qty": 1, "type": "market" }),
            StatusCode::BAD_REQUEST,
            "bad_json",
        ),
    ];
    for (body, want_status, want_code) in cases {
        let (status, resp) = post(&app, "/api/symbols/ACME/orders", body.clone()).await;
        assert_eq!(status, want_status, "{body}: {resp}");
        assert_eq!(resp["error"]["code"], want_code, "{body}: {resp}");
    }
    let (status, _) = post(
        &app,
        "/api/symbols/NOPE/orders",
        json!({ "trader_id": id, "side": "buy", "qty": 1, "type": "market" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = get(&app, "/api/traders/99").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, p) = get(&app, &format!("/api/traders/{id}")).await;
    assert_eq!(p["cash_cents"], 10_000_000);
    assert!(p["positions"].as_array().unwrap().is_empty());
    let (_, list) = get(&app, "/api/traders").await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["equity_cents"], 10_000_000);
}

// ---------------------------------------------------------------------------
// Users, accounts and money

#[tokio::test]
async fn users_open_accounts_and_add_money() {
    let app = test_app();
    let (status, user) = post(
        &app,
        "/api/users",
        json!({ "name": "ada", "email": "ada@example.com" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{user}");
    assert_eq!(user["id"], 1);
    assert_eq!(user["name"], "ada");
    assert_eq!(user["email"], "ada@example.com");
    assert_eq!(user["balance_cents"], 0);
    assert!(user["accounts"].as_array().unwrap().is_empty());

    // An account with an opening balance, then two deposits.
    let (status, account) = post(
        &app,
        "/api/users/1/accounts",
        json!({ "name": "main", "cash_cents": 100_000 }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{account}");
    assert_eq!(account["balance_cents"], 100_000);
    assert_eq!(account["available_cents"], 100_000);
    assert_eq!(account["deposited_cents"], 100_000);
    assert_eq!(account["status"], "active");
    assert_eq!(account["valid"], true);
    assert!(account["trader_id"].is_null(), "no trader on it yet");
    let id = account["id"].as_u64().unwrap();

    let (status, body) = post(
        &app,
        &format!("/api/accounts/{id}/deposit"),
        json!({ "amount_cents": 25_000, "memo": "week 1" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["account"]["balance_cents"], 125_000);
    assert_eq!(body["entries"][0]["amount_cents"], 25_000);
    assert_eq!(body["entries"][0]["balance_cents"], 125_000);
    assert_eq!(body["entries"][0]["memo"], "week 1");

    let (status, body) = post(
        &app,
        &format!("/api/accounts/{id}/withdraw"),
        json!({ "amount_cents": 5_000 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["account"]["balance_cents"], 120_000);
    assert_eq!(body["account"]["withdrawn_cents"], 5_000);
    assert_eq!(body["entries"][0]["amount_cents"], -5_000);

    // The ledger has all of it, newest first.
    let (status, ledger) = get(&app, &format!("/api/accounts/{id}/ledger")).await;
    assert_eq!(status, StatusCode::OK);
    let kinds: Vec<&str> = ledger["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["withdrawal", "deposit", "open"]);

    let (_, user) = get(&app, "/api/users/1").await;
    assert_eq!(user["accounts"], json!([id]));
    assert_eq!(user["balance_cents"], 120_000);
    let (_, accounts) = get(&app, "/api/users/1/accounts").await;
    assert_eq!(accounts.as_array().unwrap().len(), 1);
    let (_, all) = get(&app, "/api/accounts").await;
    assert_eq!(all.as_array().unwrap().len(), 1);
    let (_, health) = get(&app, "/api/health").await;
    assert_eq!(health["users"], 1);
    assert_eq!(health["accounts"], 1);
    assert_eq!(health["cash_cents"], 120_000);
}

#[tokio::test]
async fn money_movements_are_validated() {
    let app = test_app();
    post(&app, "/api/users", json!({ "name": "bo" })).await;
    let (_, account) = post(
        &app,
        "/api/users/1/accounts",
        json!({ "cash_cents": 1_000 }),
    )
    .await;
    let id = account["id"].as_u64().unwrap();
    let cases = [
        (
            "deposit",
            json!({ "amount_cents": 0 }),
            StatusCode::BAD_REQUEST,
            "invalid_amount",
        ),
        (
            "deposit",
            json!({ "amount_cents": -100 }),
            StatusCode::BAD_REQUEST,
            "invalid_amount",
        ),
        (
            "deposit",
            json!({ "amount_cents": 1_000_000_000_000_000_i64 + 1 }),
            StatusCode::BAD_REQUEST,
            "invalid_amount",
        ),
        (
            "deposit",
            json!({ "amount_cents": 12.5 }),
            StatusCode::BAD_REQUEST,
            "bad_json",
        ),
        (
            "withdraw",
            json!({ "amount_cents": 1_001 }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "insufficient_funds",
        ),
    ];
    for (path, body, want_status, want_code) in cases {
        let (status, resp) = post(&app, &format!("/api/accounts/{id}/{path}"), body.clone()).await;
        assert_eq!(status, want_status, "{path} {body}: {resp}");
        assert_eq!(resp["error"]["code"], want_code, "{path} {body}: {resp}");
    }
    // Unknown ids, and an email that is not one.
    for uri in ["/api/accounts/99", "/api/accounts/99/ledger"] {
        let (status, body) = get(&app, uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"]["code"], "unknown_account");
    }
    let (status, body) = get(&app, "/api/users/99").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "unknown_user");
    let (status, body) = post(&app, "/api/users", json!({ "email": "nope" })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // The balance never moved.
    let (_, account) = get(&app, &format!("/api/accounts/{id}")).await;
    assert_eq!(account["balance_cents"], 1_000);
    assert_eq!(account["valid"], true);
}

#[tokio::test]
async fn orders_settle_through_the_account() {
    let app = test_app();
    let (status, trader) = post(
        &app,
        "/api/traders",
        json!({ "name": "cleo", "email": "cleo@example.com", "cash_cents": 5_000_000 }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{trader}");
    assert_eq!(trader["user_id"], 1);
    assert_eq!(trader["account_id"], 1);
    assert_eq!(trader["account_status"], "active");
    let id = trader["id"].as_u64().unwrap();

    // A market buy debits the account and writes one ledger entry per print.
    let (status, order) = post(
        &app,
        "/api/symbols/ACME/orders",
        json!({ "trader_id": id, "side": "buy", "qty": 100, "type": "market" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{order}");
    let notional = order["notional_cents"].as_i64().unwrap();
    let prints = order["trades"].as_array().unwrap().len();
    let (_, ledger) = get(&app, "/api/accounts/1/ledger").await;
    assert_eq!(ledger["account"]["balance_cents"], 5_000_000 - notional);
    assert_eq!(ledger["account"]["trader_id"], id);
    let entries = ledger["entries"].as_array().unwrap();
    assert_eq!(entries.len(), prints + 1, "one per print, plus `open`");
    assert_eq!(entries[0]["kind"], "buy");
    assert_eq!(entries[0]["symbol"], "ACME");
    assert!(entries[0]["amount_cents"].as_i64().unwrap() < 0);
    assert_eq!(
        entries
            .iter()
            .map(|e| e["amount_cents"].as_i64().unwrap())
            .sum::<i64>(),
        5_000_000 - notional,
        "the ledger adds up to the balance"
    );

    // Adding money through the trader lands on its account.
    let (status, p) = post(
        &app,
        &format!("/api/traders/{id}/deposit"),
        json!({ "amount_cents": 300_000 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{p}");
    assert_eq!(p["cash_cents"], 5_300_000 - notional);
    assert_eq!(p["free_cash_cents"], 5_300_000 - notional);

    // A resting buy reserves cash on the account, not just in the portfolio.
    let (_, book) = get(&app, "/api/symbols/PXCO/book").await;
    let price = book["bid_cents"].as_i64().unwrap() - 20;
    post(
        &app,
        "/api/symbols/PXCO/orders",
        json!({ "trader_id": id, "side": "buy", "qty": 100, "type": "limit", "price_cents": price }),
    )
    .await;
    let (_, account) = get(&app, "/api/accounts/1").await;
    assert_eq!(account["reserved_cents"], 100 * price);
    assert_eq!(
        account["available_cents"].as_i64().unwrap(),
        account["balance_cents"].as_i64().unwrap() - 100 * price
    );
    // Reserved cash cannot be withdrawn, and the account cannot be closed.
    let (status, body) = post(
        &app,
        "/api/accounts/1/withdraw",
        json!({ "amount_cents": account["balance_cents"] }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "insufficient_funds");
    let (status, body) = post(
        &app,
        "/api/accounts/1/status",
        json!({ "status": "closed" }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "cash_reserved");

    post(&app, &format!("/api/traders/{id}/cancel_all"), json!({})).await;
    let (_, check) = get(&app, "/api/accounts/1/validate").await;
    assert_eq!(check["valid"], true);
    assert_eq!(check["reserved_cents"], 0);
    assert_eq!(check["issues"], json!([]));
    assert_eq!(check["can_trade"], true);
}

#[tokio::test]
async fn a_frozen_account_cannot_trade() {
    let app = test_app();
    let id = new_trader(&app, "dee").await;
    let (status, body) = post(
        &app,
        "/api/accounts/1/status",
        json!({ "status": "frozen" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "frozen");

    let (status, body) = post(
        &app,
        "/api/symbols/ACME/orders",
        json!({ "trader_id": id, "side": "buy", "qty": 1, "type": "market" }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "account_not_active");
    // Deposits still land; withdrawals do not.
    let (status, _) = post(
        &app,
        "/api/accounts/1/deposit",
        json!({ "amount_cents": 1_000 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = post(
        &app,
        "/api/accounts/1/withdraw",
        json!({ "amount_cents": 1_000 }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    // Unfrozen, it trades again.
    post(
        &app,
        "/api/accounts/1/status",
        json!({ "status": "active" }),
    )
    .await;
    let (status, _) = post(
        &app,
        "/api/symbols/ACME/orders",
        json!({ "trader_id": id, "side": "buy", "qty": 1, "type": "market" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // A closed account is terminal.
    post(
        &app,
        "/api/accounts/1/status",
        json!({ "status": "closed" }),
    )
    .await;
    let (status, body) = post(
        &app,
        "/api/accounts/1/status",
        json!({ "status": "active" }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (status, _) = post(
        &app,
        "/api/accounts/1/deposit",
        json!({ "amount_cents": 1 }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn one_user_can_run_several_traders() {
    let app = test_app();
    post(&app, "/api/users", json!({ "name": "eve" })).await;
    let (status, first) = post(
        &app,
        "/api/traders",
        json!({ "name": "alpha", "user_id": 1, "cash_cents": 10_000 }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{first}");
    let (status, second) = post(
        &app,
        "/api/traders",
        json!({ "name": "beta", "user_id": 1, "cash_cents": 20_000 }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{second}");
    assert_eq!(first["user_id"], 1);
    assert_eq!(second["user_id"], 1);
    assert_ne!(first["account_id"], second["account_id"]);

    let (_, user) = get(&app, "/api/users/1").await;
    assert_eq!(user["traders"].as_array().unwrap().len(), 2);
    assert_eq!(user["balance_cents"], 30_000);

    // A trader may also join an account that already exists…
    let (status, third) = post(
        &app,
        "/api/traders",
        json!({ "name": "gamma", "user_id": 1, "account_id": first["account_id"] }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{third}");
    assert_eq!(third["account_id"], first["account_id"]);
    assert_eq!(third["cash_cents"], 10_000, "the same money, shared");

    // …but not one that belongs to somebody else.
    post(&app, "/api/users", json!({ "name": "mallory" })).await;
    let (status, body) = post(
        &app,
        "/api/traders",
        json!({ "user_id": 2, "account_id": first["account_id"] }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = post(&app, "/api/traders", json!({ "user_id": 99 })).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"]["code"], "unknown_user");
    let (status, body) = post(&app, "/api/traders", json!({ "account_id": 1 })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn a_trader_can_only_sell_shares_it_holds() {
    let app = test_app();
    let id = new_trader(&app, "sam").await;

    // Nothing owned yet: there is nothing to sell.
    let (status, body) = post(
        &app,
        "/api/symbols/ACME/orders",
        json!({ "trader_id": id, "side": "sell", "qty": 1, "type": "market" }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "order_refused");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("insufficient shares"),
        "{body}"
    );

    // Buy 200, and 201 is still one too many.
    let (status, body) = post(
        &app,
        "/api/symbols/ACME/orders",
        json!({ "trader_id": id, "side": "buy", "qty": 200, "type": "market" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["filled"], 200);
    let (_, p) = get(&app, &format!("/api/traders/{id}")).await;
    assert_eq!(p["positions"][0]["qty"], 200);
    assert_eq!(p["positions"][0]["free_shares"], 200);
    assert_eq!(p["positions"][0]["reserved_shares"], 0);
    let (status, body) = post(
        &app,
        "/api/symbols/ACME/orders",
        json!({ "trader_id": id, "side": "sell", "qty": 201, "type": "market" }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // A resting sell of 150 promises those shares away: only 50 stay free.
    let ask = get(&app, "/api/symbols/ACME/book").await.1["ask_cents"]
        .as_i64()
        .unwrap();
    let (status, body) = post(
        &app,
        "/api/symbols/ACME/orders",
        json!({ "trader_id": id, "side": "sell", "qty": 150, "type": "limit", "price_cents": ask * 2 }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["status"], "resting");
    let order_id = body["order_id"].as_u64().unwrap();
    let (_, p) = get(&app, &format!("/api/traders/{id}")).await;
    assert_eq!(p["positions"][0]["reserved_shares"], 150);
    assert_eq!(p["positions"][0]["free_shares"], 50);

    for (qty, expected) in [
        (51, StatusCode::UNPROCESSABLE_ENTITY),
        (50, StatusCode::CREATED),
    ] {
        let (status, body) = post(
            &app,
            "/api/symbols/ACME/orders",
            json!({ "trader_id": id, "side": "sell", "qty": qty, "type": "market" }),
        )
        .await;
        assert_eq!(status, expected, "selling {qty} of 50 free: {body}");
    }
    let (_, p) = get(&app, &format!("/api/traders/{id}")).await;
    assert_eq!(p["positions"][0]["qty"], 150, "sold the 50 that were free");
    assert_eq!(p["positions"][0]["free_shares"], 0);

    // Cancelling the resting sell frees them again, and then everything can go.
    let (status, _) = delete(
        &app,
        &format!("/api/symbols/ACME/orders/{order_id}?trader_id={id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, p) = get(&app, &format!("/api/traders/{id}")).await;
    assert_eq!(p["positions"][0]["free_shares"], 150);
    let (status, body) = post(
        &app,
        "/api/symbols/ACME/orders",
        json!({ "trader_id": id, "side": "sell", "qty": 150, "type": "market" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (_, p) = get(&app, &format!("/api/traders/{id}")).await;
    assert_eq!(p["positions"][0]["qty"], 0);
    let (status, _) = post(
        &app,
        "/api/symbols/ACME/orders",
        json!({ "trader_id": id, "side": "sell", "qty": 1, "type": "market" }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "flat again");
}

#[tokio::test]
async fn every_symbol_has_a_finite_number_of_shares() {
    let app = test_app();
    let (status, shares) = get(&app, "/api/symbols/ACME/shares").await;
    assert_eq!(status, StatusCode::OK);
    let outstanding = shares["shares_outstanding"].as_u64().unwrap();
    assert_eq!(outstanding, 240_000_000);
    assert_eq!(shares["held_shares"], 0);
    assert_eq!(shares["bid_shares"], 0);
    assert_eq!(shares["available_shares"], outstanding);
    assert!(shares["holders"].as_array().unwrap().is_empty());

    // The quote carries the count and the market cap it implies.
    let (_, symbols) = get(&app, "/api/symbols").await;
    let acme = &symbols["symbols"][0];
    assert_eq!(acme["shares_outstanding"], outstanding);
    assert_eq!(
        acme["market_cap_cents"].as_i64().unwrap(),
        acme["price_cents"].as_i64().unwrap() * outstanding as i64
    );

    // Buying takes shares out of circulation; a resting bid spoken for them.
    let id = new_trader(&app, "tara").await;
    post(
        &app,
        "/api/symbols/ACME/orders",
        json!({ "trader_id": id, "side": "buy", "qty": 500, "type": "market" }),
    )
    .await;
    let bid = get(&app, "/api/symbols/ACME/book").await.1["bid_cents"]
        .as_i64()
        .unwrap();
    post(
        &app,
        "/api/symbols/ACME/orders",
        json!({ "trader_id": id, "side": "buy", "qty": 40, "type": "limit", "price_cents": bid / 2 }),
    )
    .await;
    let (_, shares) = get(&app, "/api/symbols/ACME/shares").await;
    assert_eq!(shares["held_shares"], 500);
    assert_eq!(shares["bid_shares"], 40);
    assert_eq!(shares["available_shares"], outstanding - 540);
    assert_eq!(shares["holders"][0]["trader_id"], id);
    assert_eq!(shares["holders"][0]["qty"], 500);
    assert_eq!(shares["holders"][0]["free_shares"], 500);
    let (_, detail) = get(&app, "/api/symbols/ACME").await;
    assert_eq!(detail["info"]["shares_outstanding"], outstanding);
    assert_eq!(detail["shares"]["held_shares"], 500);

    // Nobody can buy shares that do not exist — checked before the money is.
    let (status, body) = post(
        &app,
        "/api/symbols/ACME/orders",
        json!({ "trader_id": id, "side": "buy", "qty": outstanding, "type": "market" }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "order_refused");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not enough shares left"),
        "{body}"
    );

    // Other symbols have their own counts and are unaffected.
    let (_, nbla) = get(&app, "/api/symbols/NBLA/shares").await;
    assert_eq!(nbla["shares_outstanding"], 85_000_000);
    assert_eq!(nbla["available_shares"], 85_000_000);
}

#[tokio::test]
async fn a_users_shares_are_the_sum_of_their_traders() {
    let app = test_app();
    post(&app, "/api/users", json!({ "name": "nina" })).await;
    let mut ids = Vec::new();
    for name in ["alpha", "beta"] {
        let (status, body) = post(
            &app,
            "/api/traders",
            json!({ "name": name, "user_id": 1, "cash_cents": 5_000_000 }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        ids.push(body["id"].as_u64().unwrap());
    }
    let (alpha, beta) = (ids[0], ids[1]);

    for (trader, qty) in [(alpha, 100), (beta, 40)] {
        let (status, body) = post(
            &app,
            "/api/symbols/ACME/orders",
            json!({ "trader_id": trader, "side": "buy", "qty": qty, "type": "market" }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    let (status, body) = post(
        &app,
        "/api/symbols/HLIO/orders",
        json!({ "trader_id": beta, "side": "buy", "qty": 300, "type": "market" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let (status, holdings) = get(&app, "/api/users/1/holdings").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(holdings["user_id"], 1);
    assert_eq!(holdings["shares_owned"], 440);
    assert_eq!(holdings["free_shares"], 440);
    assert_eq!(holdings["reserved_shares"], 0);
    let acme = &holdings["holdings"][0];
    assert_eq!(acme["symbol"], "ACME");
    assert_eq!(acme["qty"], 140, "both traders' ACME, added up");
    assert_eq!(acme["traders"], json!([alpha, beta]));
    assert!(acme["avg_cost_cents"].as_f64().unwrap() > 0.0);
    assert_eq!(
        acme["market_value_cents"].as_i64().unwrap(),
        acme["mark_cents"].as_i64().unwrap() * 140
    );
    assert_eq!(holdings["holdings"][1]["symbol"], "HLIO");
    assert_eq!(holdings["holdings"][1]["qty"], 300);
    assert_eq!(
        holdings["market_value_cents"].as_i64().unwrap(),
        acme["market_value_cents"].as_i64().unwrap()
            + holdings["holdings"][1]["market_value_cents"]
                .as_i64()
                .unwrap()
    );

    // The user's own row agrees with the holdings.
    let (_, user) = get(&app, "/api/users/1").await;
    assert_eq!(user["shares_owned"], 440);
    assert_eq!(user["holdings_value_cents"], holdings["market_value_cents"]);

    // Shares belong to the trader that bought them: alpha cannot sell beta's.
    let (status, body) = post(
        &app,
        "/api/symbols/ACME/orders",
        json!({ "trader_id": alpha, "side": "sell", "qty": 140, "type": "market" }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("have 100 free"),
        "{body}"
    );
    // And beta cannot sell HLIO it does not hold in that symbol.
    let (status, body) = post(
        &app,
        "/api/symbols/HLIO/orders",
        json!({ "trader_id": alpha, "side": "sell", "qty": 1, "type": "market" }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // A resting sell shows up as reserved for the whole user.
    let ask = get(&app, "/api/symbols/ACME/book").await.1["ask_cents"]
        .as_i64()
        .unwrap();
    post(
        &app,
        "/api/symbols/ACME/orders",
        json!({ "trader_id": alpha, "side": "sell", "qty": 60, "type": "limit", "price_cents": ask * 2 }),
    )
    .await;
    let (_, holdings) = get(&app, "/api/users/1/holdings").await;
    assert_eq!(holdings["holdings"][0]["reserved_shares"], 60);
    assert_eq!(holdings["holdings"][0]["free_shares"], 80);
    assert_eq!(holdings["shares_owned"], 440, "reserving sells nothing");
    assert_eq!(holdings["free_shares"], 380);

    let (status, body) = get(&app, "/api/users/99/holdings").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"]["code"], "unknown_user");
}
