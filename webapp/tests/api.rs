//! End-to-end tests over the router, without a TCP listener.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use fehu::Timestamp;
use fehu_webapp::market::{App, Options};
use fehu_webapp::{engine, router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::time::Duration;
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

/// Whoever a request is sent as: the bare app is an anonymous caller, a
/// [`Player`] carries the API key they signed up with.
trait Caller {
    fn app(&self) -> &Arc<App>;
    fn api_key(&self) -> Option<&str> {
        None
    }
}

impl Caller for Arc<App> {
    fn app(&self) -> &Arc<App> {
        self
    }
}

/// A signed-up player: their user, their trader, and their key.
struct Player {
    app: Arc<App>,
    user: u64,
    trader: u64,
    key: String,
}

impl Caller for Player {
    fn app(&self) -> &Arc<App> {
        &self.app
    }

    fn api_key(&self) -> Option<&str> {
        Some(&self.key)
    }
}

/// Sign a player up through `POST /api/traders`, which creates their user,
/// funds an account and hands over the key exactly once.
async fn sign_up(app: &Arc<App>, name: &str) -> Player {
    let (status, body) = post(app, "/api/traders", json!({ "name": name })).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let key = body["api_key"]
        .as_str()
        .unwrap_or_else(|| panic!("sign-up must hand over an API key: {body}"))
        .to_owned();
    Player {
        app: Arc::clone(app),
        user: body["user_id"].as_u64().unwrap(),
        trader: body["id"].as_u64().unwrap(),
        key,
    }
}

/// The player a `POST /api/traders` response describes, key included.
fn player_from(app: &Arc<App>, body: &Value) -> Player {
    Player {
        app: Arc::clone(app),
        user: body["user_id"].as_u64().unwrap(),
        trader: body["id"].as_u64().unwrap(),
        key: body["api_key"]
            .as_str()
            .unwrap_or_else(|| panic!("sign-up must hand over an API key: {body}"))
            .to_owned(),
    }
}

/// Register a user through `POST /api/users`, keeping the key the response
/// carried. The player has no trader of their own yet.
async fn register(app: &Arc<App>, body: Value) -> (Player, Value) {
    let (status, user) = post(app, "/api/users", body).await;
    assert_eq!(status, StatusCode::CREATED, "{user}");
    let key = user["api_key"]
        .as_str()
        .unwrap_or_else(|| panic!("creating a user must hand over an API key: {user}"))
        .to_owned();
    let player = Player {
        app: Arc::clone(app),
        user: user["id"].as_u64().unwrap(),
        trader: 0,
        key,
    };
    (player, user)
}

async fn call(caller: &impl Caller, mut req: Request<Body>) -> (StatusCode, Value) {
    if let Some(key) = caller.api_key() {
        req.headers_mut().insert(
            header::AUTHORIZATION,
            format!("Bearer {key}").parse().unwrap(),
        );
    }
    let resp = router(Arc::clone(caller.app())).oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or_else(|e| panic!("bad JSON ({e}): {bytes:?}"))
    };
    (status, body)
}

async fn get(caller: &impl Caller, uri: &str) -> (StatusCode, Value) {
    call(caller, Request::get(uri).body(Body::empty()).unwrap()).await
}

async fn post(caller: &impl Caller, uri: &str, body: Value) -> (StatusCode, Value) {
    call(
        caller,
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

async fn delete(caller: &impl Caller, uri: &str) -> (StatusCode, Value) {
    call(caller, Request::delete(uri).body(Body::empty()).unwrap()).await
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
    let alice = player_from(&app, &body);
    assert_eq!(body["id"], 1);
    assert_eq!(body["name"], "alice");
    assert_eq!(body["cash_cents"], 1_000_000_000);
    assert_eq!(body["equity_cents"], 1_000_000_000);

    let before = get(&alice, "/api/symbols/ACME/book").await.1;
    let ask = before["ask_cents"].as_i64().unwrap();
    let r0 = before["reference_cents"].as_i64().unwrap();

    let (status, body) = post(
        &alice,
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

    let (_, p) = get(&alice, "/api/traders/1").await;
    assert_eq!(p["cash_cents"], 1_000_000_000 - notional);
    assert_eq!(p["positions"][0]["symbol"], "ACME");
    assert_eq!(p["positions"][0]["qty"], 20000);
    assert_eq!(
        p["fills"].as_array().unwrap().len(),
        body["trades"].as_array().unwrap().len()
    );
    assert_eq!(p["fills"][0]["liquidity"], "taker");
    assert!(p["open_orders"].as_array().unwrap().is_empty());
    let (_, b) = get(&alice, "/api/symbols/ACME/book").await;
    assert_eq!(b["pending_flow"], 20000);

    // The impact lands on the next tick and is a small positive move.
    engine::advance_to(&app, Timestamp(NOW_MS + 1000));
    let (_, b) = get(&alice, "/api/symbols/ACME/book").await;
    assert_eq!(b["pending_flow"], 0);
    let r1 = b["reference_cents"].as_i64().unwrap();
    // 0.7 · 0.22/√365.25 · √(20000/6.49M) ≈ 0.045 %: about 4 cents on $84,
    // against a 1 s noise std of ~0.3 cents.
    assert!(r1 > r0 + 2, "{r0} -> {r1}");
    let (_, p) = get(&alice, "/api/traders/1").await;
    assert_eq!(p["positions"][0]["mark_cents"], r1);
    let (_, h) = get(&alice, "/api/health").await;
    assert_eq!(h["traders"], 1);
}

#[tokio::test]
async fn limit_orders_rest_reserve_and_cancel() {
    let app = test_app();
    let bob = sign_up(&app, "bob").await;
    let id = bob.trader;
    let book = get(&bob, "/api/symbols/PXCO/book").await.1;
    let bid = book["bid_cents"].as_i64().unwrap();
    let price = bid - 20;
    let (status, body) = post(&bob,
        "/api/symbols/PXCO/orders",
        json!({ "trader_id": id, "side": "buy", "qty": 100, "type": "limit", "price_cents": price }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["status"], "resting");
    assert_eq!(body["filled"], 0);
    let order_id = body["order_id"].as_u64().unwrap();

    let (_, p) = get(&bob, &format!("/api/traders/{id}")).await;
    assert_eq!(p["reserved_cents"], 100 * price);
    assert_eq!(p["free_cash_cents"], 10_000_000 - 100 * price);
    assert_eq!(p["open_orders"][0]["order_id"], order_id);
    assert_eq!(p["open_orders"][0]["symbol"], "PXCO");
    let (_, orders) = get(&bob, &format!("/api/symbols/PXCO/orders?trader_id={id}")).await;
    assert_eq!(orders.as_array().unwrap().len(), 1);
    let (status, o) = get(&bob, &format!("/api/symbols/PXCO/orders/{order_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(o["remaining"], 100);
    // Visible in the book at its price.
    let (_, b) = get(&bob, "/api/symbols/PXCO/book?depth=50").await;
    assert!(
        b["bids"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l["price_cents"] == price && l["qty"] == 100)
    );

    // Someone else cannot cancel it.
    let mallory = sign_up(&app, "mallory").await;
    let other = mallory.trader;
    let (status, _) = delete(
        &mallory,
        &format!("/api/symbols/PXCO/orders/{order_id}?trader_id={other}"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, body) = delete(
        &bob,
        &format!("/api/symbols/PXCO/orders/{order_id}?trader_id={id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["remaining"], 100);
    let (_, p) = get(&bob, &format!("/api/traders/{id}")).await;
    assert_eq!(p["reserved_cents"], 0);
    let (status, _) = get(&bob, &format!("/api/symbols/PXCO/orders/{order_id}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // cancel_all sweeps every symbol.
    for sym in ["ACME", "HLIO"] {
        let b = get(&bob, &format!("/api/symbols/{sym}/book")).await.1;
        let bid = b["bid_cents"].as_i64().unwrap();
        post(&bob,
            &format!("/api/symbols/{sym}/orders"),
            json!({ "trader_id": id, "side": "buy", "qty": 10, "type": "limit", "price_cents": bid - 5 }),
        )
        .await;
    }
    let (status, body) = post(&bob, &format!("/api/traders/{id}/cancel_all"), json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 2);
    let (_, p) = get(&bob, &format!("/api/traders/{id}")).await;
    assert!(p["open_orders"].as_array().unwrap().is_empty());
    assert_eq!(p["reserved_cents"], 0);
}

#[tokio::test]
async fn resting_bid_fills_when_the_market_trades_through_it() {
    let app = test_app();
    let player = sign_up(&app, "carol").await;
    let id = player.trader;
    let book = get(&player, "/api/symbols/NBLA/book").await.1;
    let bid = book["bid_cents"].as_i64().unwrap();
    let ask = book["ask_cents"].as_i64().unwrap();
    // Inside the spread: the next synthetic sell print hits it.
    let price = (bid + ask) / 2;
    let (status, body) = post(
        &player,
        "/api/symbols/NBLA/orders",
        json!({ "trader_id": id, "side": "buy", "qty": 50, "type": "limit", "price_cents": price }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["status"], "resting");
    let mut filled = 0;
    for k in 1..=600 {
        engine::advance_to(&app, Timestamp(NOW_MS + k * 1000));
        let (_, p) = get(&player, &format!("/api/traders/{id}")).await;
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
    let alice = sign_up(&app, "a").await;
    let bruno = sign_up(&app, "b").await;
    let (a, b) = (alice.trader, bruno.trader);
    // a buys some stock, then offers it inside the spread; b lifts it.
    let (status, body) = post(
        &alice,
        "/api/symbols/HLIO/orders",
        json!({ "trader_id": a, "side": "buy", "qty": 1000, "type": "market" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let book = get(&alice, "/api/symbols/HLIO/book").await.1;
    let mid = (book["bid_cents"].as_i64().unwrap() + book["ask_cents"].as_i64().unwrap()) / 2;
    let (status, body) = post(
        &alice,
        "/api/symbols/HLIO/orders",
        json!({ "trader_id": a, "side": "sell", "qty": 1000, "type": "limit", "price_cents": mid }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["status"], "resting");
    let (_, pa) = get(&alice, &format!("/api/traders/{a}")).await;
    assert_eq!(pa["positions"][0]["reserved_shares"], 1000);
    let (status, body) = post(&bruno,
        "/api/symbols/HLIO/orders",
        json!({ "trader_id": b, "side": "buy", "qty": 600, "type": "limit", "price_cents": mid, "tif": "ioc" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["status"], "filled");
    assert_eq!(body["trades"][0]["maker_trader"], a);
    assert_eq!(body["trades"][0]["price_cents"], mid);
    let (_, pa) = get(&alice, &format!("/api/traders/{a}")).await;
    assert_eq!(pa["positions"][0]["qty"], 400);
    assert_eq!(pa["positions"][0]["reserved_shares"], 400);
    assert_eq!(pa["open_orders"][0]["remaining"], 400);
    assert_eq!(pa["fills"][0]["counterparty"], "trader");
    let (_, pb) = get(&bruno, &format!("/api/traders/{b}")).await;
    assert_eq!(pb["positions"][0]["qty"], 600);
    // Trader-to-trader flow does not move the reference.
    let (_, book) = get(&alice, "/api/symbols/HLIO/book").await;
    assert_eq!(book["pending_flow"], 1000, "only a's market buy is pending");
}

#[tokio::test]
async fn orders_are_checked_and_rejected() {
    let app = test_app();
    let player = sign_up(&app, "dan").await;
    let id = player.trader;
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
        let (status, resp) = post(&player, "/api/symbols/ACME/orders", body.clone()).await;
        assert_eq!(status, want_status, "{body}: {resp}");
        assert_eq!(resp["error"]["code"], want_code, "{body}: {resp}");
    }
    let (status, _) = post(
        &player,
        "/api/symbols/NOPE/orders",
        json!({ "trader_id": id, "side": "buy", "qty": 1, "type": "market" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = get(&player, "/api/traders/99").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, p) = get(&player, &format!("/api/traders/{id}")).await;
    assert_eq!(p["cash_cents"], 10_000_000);
    assert!(p["positions"].as_array().unwrap().is_empty());
    let (_, list) = get(&player, "/api/traders").await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["equity_cents"], 10_000_000);
}

// ---------------------------------------------------------------------------
// Users, accounts and money

#[tokio::test]
async fn users_open_accounts_and_add_money() {
    let app = test_app();
    let (ada, user) = register(&app, json!({ "name": "ada", "email": "ada@example.com" })).await;
    assert!(
        user["api_key"].as_str().unwrap().starts_with("fehu_"),
        "the key is handed over once, here: {user}"
    );
    assert_eq!(user["id"], 1);
    assert_eq!(user["name"], "ada");
    assert_eq!(user["email"], "ada@example.com");
    assert_eq!(user["balance_cents"], 0);
    assert!(user["accounts"].as_array().unwrap().is_empty());

    // An account with an opening balance, then two deposits.
    let (status, account) = post(
        &ada,
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
        &ada,
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
        &ada,
        &format!("/api/accounts/{id}/withdraw"),
        json!({ "amount_cents": 5_000 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["account"]["balance_cents"], 120_000);
    assert_eq!(body["account"]["withdrawn_cents"], 5_000);
    assert_eq!(body["entries"][0]["amount_cents"], -5_000);

    // The ledger has all of it, newest first.
    let (status, ledger) = get(&ada, &format!("/api/accounts/{id}/ledger")).await;
    assert_eq!(status, StatusCode::OK);
    let kinds: Vec<&str> = ledger["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["withdrawal", "deposit", "open"]);

    let (_, user) = get(&ada, "/api/users/1").await;
    assert_eq!(user["accounts"], json!([id]));
    assert_eq!(user["balance_cents"], 120_000);
    let (_, accounts) = get(&ada, "/api/users/1/accounts").await;
    assert_eq!(accounts.as_array().unwrap().len(), 1);
    let (_, all) = get(&ada, "/api/accounts").await;
    assert_eq!(all.as_array().unwrap().len(), 1);
    let (_, health) = get(&ada, "/api/health").await;
    assert_eq!(health["users"], 1);
    assert_eq!(health["accounts"], 1);
    assert_eq!(health["cash_cents"], 120_000);
}

#[tokio::test]
async fn money_movements_are_validated() {
    let app = test_app();
    let (bo, _) = register(&app, json!({ "name": "bo" })).await;
    let (_, account) = post(&bo, "/api/users/1/accounts", json!({ "cash_cents": 1_000 })).await;
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
        let (status, resp) = post(&bo, &format!("/api/accounts/{id}/{path}"), body.clone()).await;
        assert_eq!(status, want_status, "{path} {body}: {resp}");
        assert_eq!(resp["error"]["code"], want_code, "{path} {body}: {resp}");
    }
    // Unknown ids, and an email that is not one.
    for uri in ["/api/accounts/99", "/api/accounts/99/ledger"] {
        let (status, body) = get(&bo, uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"]["code"], "unknown_account");
    }
    // Whether user 99 exists is not bo's business either way.
    let (status, body) = get(&bo, "/api/users/99").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["code"], "forbidden");
    let (status, body) = post(&bo, "/api/users", json!({ "email": "nope" })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // The balance never moved.
    let (_, account) = get(&bo, &format!("/api/accounts/{id}")).await;
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
    let cleo = player_from(&app, &trader);
    assert_eq!(trader["user_id"], 1);
    assert_eq!(trader["account_id"], 1);
    assert_eq!(trader["account_status"], "active");
    let id = trader["id"].as_u64().unwrap();

    // A market buy debits the account and writes one ledger entry per print.
    let (status, order) = post(
        &cleo,
        "/api/symbols/ACME/orders",
        json!({ "trader_id": id, "side": "buy", "qty": 100, "type": "market" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{order}");
    let notional = order["notional_cents"].as_i64().unwrap();
    let prints = order["trades"].as_array().unwrap().len();
    let (_, ledger) = get(&cleo, "/api/accounts/1/ledger").await;
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
        &cleo,
        &format!("/api/traders/{id}/deposit"),
        json!({ "amount_cents": 300_000 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{p}");
    assert_eq!(p["cash_cents"], 5_300_000 - notional);
    assert_eq!(p["free_cash_cents"], 5_300_000 - notional);

    // A resting buy reserves cash on the account, not just in the portfolio.
    let (_, book) = get(&cleo, "/api/symbols/PXCO/book").await;
    let price = book["bid_cents"].as_i64().unwrap() - 20;
    post(&cleo,
        "/api/symbols/PXCO/orders",
        json!({ "trader_id": id, "side": "buy", "qty": 100, "type": "limit", "price_cents": price }),
    )
    .await;
    let (_, account) = get(&cleo, "/api/accounts/1").await;
    assert_eq!(account["reserved_cents"], 100 * price);
    assert_eq!(
        account["available_cents"].as_i64().unwrap(),
        account["balance_cents"].as_i64().unwrap() - 100 * price
    );
    // Reserved cash cannot be withdrawn, and the account cannot be closed.
    let (status, body) = post(
        &cleo,
        "/api/accounts/1/withdraw",
        json!({ "amount_cents": account["balance_cents"] }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "insufficient_funds");
    let (status, body) = post(
        &cleo,
        "/api/accounts/1/status",
        json!({ "status": "closed" }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "cash_reserved");

    post(&cleo, &format!("/api/traders/{id}/cancel_all"), json!({})).await;
    let (_, check) = get(&cleo, "/api/accounts/1/validate").await;
    assert_eq!(check["valid"], true);
    assert_eq!(check["reserved_cents"], 0);
    assert_eq!(check["issues"], json!([]));
    assert_eq!(check["can_trade"], true);
}

#[tokio::test]
async fn a_frozen_account_cannot_trade() {
    let app = test_app();
    let player = sign_up(&app, "dee").await;
    let id = player.trader;
    let (status, body) = post(
        &player,
        "/api/accounts/1/status",
        json!({ "status": "frozen" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "frozen");

    let (status, body) = post(
        &player,
        "/api/symbols/ACME/orders",
        json!({ "trader_id": id, "side": "buy", "qty": 1, "type": "market" }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "account_not_active");
    // Deposits still land; withdrawals do not.
    let (status, _) = post(
        &player,
        "/api/accounts/1/deposit",
        json!({ "amount_cents": 1_000 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = post(
        &player,
        "/api/accounts/1/withdraw",
        json!({ "amount_cents": 1_000 }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    // Unfrozen, it trades again.
    post(
        &player,
        "/api/accounts/1/status",
        json!({ "status": "active" }),
    )
    .await;
    let (status, _) = post(
        &player,
        "/api/symbols/ACME/orders",
        json!({ "trader_id": id, "side": "buy", "qty": 1, "type": "market" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // A closed account is terminal.
    post(
        &player,
        "/api/accounts/1/status",
        json!({ "status": "closed" }),
    )
    .await;
    let (status, body) = post(
        &player,
        "/api/accounts/1/status",
        json!({ "status": "active" }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (status, _) = post(
        &player,
        "/api/accounts/1/deposit",
        json!({ "amount_cents": 1 }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn one_user_can_run_several_traders() {
    let app = test_app();
    let (eve, _) = register(&app, json!({ "name": "eve" })).await;
    let (status, first) = post(
        &eve,
        "/api/traders",
        json!({ "name": "alpha", "user_id": 1, "cash_cents": 10_000 }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{first}");
    let (status, second) = post(
        &eve,
        "/api/traders",
        json!({ "name": "beta", "user_id": 1, "cash_cents": 20_000 }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{second}");
    assert_eq!(first["user_id"], 1);
    assert_eq!(second["user_id"], 1);
    assert_ne!(first["account_id"], second["account_id"]);

    let (_, user) = get(&eve, "/api/users/1").await;
    assert_eq!(user["traders"].as_array().unwrap().len(), 2);
    assert_eq!(user["balance_cents"], 30_000);

    // A trader may also join an account that already exists…
    let (status, third) = post(
        &eve,
        "/api/traders",
        json!({ "name": "gamma", "user_id": 1, "account_id": first["account_id"] }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{third}");
    assert_eq!(third["account_id"], first["account_id"]);
    assert_eq!(third["cash_cents"], 10_000, "the same money, shared");

    // …but not one that belongs to somebody else.
    let (mallory, _) = register(&app, json!({ "name": "mallory" })).await;
    let (status, body) = post(
        &mallory,
        "/api/traders",
        json!({ "user_id": mallory.user, "account_id": first["account_id"] }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    // And eve cannot put a trader on mallory's user at all.
    let (status, body) = post(&eve, "/api/traders", json!({ "user_id": mallory.user })).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, body) = post(&eve, "/api/traders", json!({ "user_id": 99 })).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, body) = post(&eve, "/api/traders", json!({ "account_id": 1 })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn a_trader_can_only_sell_shares_it_holds() {
    let app = test_app();
    let player = sign_up(&app, "sam").await;
    let id = player.trader;

    // Nothing owned yet: there is nothing to sell.
    let (status, body) = post(
        &player,
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
        &player,
        "/api/symbols/ACME/orders",
        json!({ "trader_id": id, "side": "buy", "qty": 200, "type": "market" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["filled"], 200);
    let (_, p) = get(&player, &format!("/api/traders/{id}")).await;
    assert_eq!(p["positions"][0]["qty"], 200);
    assert_eq!(p["positions"][0]["free_shares"], 200);
    assert_eq!(p["positions"][0]["reserved_shares"], 0);
    let (status, body) = post(
        &player,
        "/api/symbols/ACME/orders",
        json!({ "trader_id": id, "side": "sell", "qty": 201, "type": "market" }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // A resting sell of 150 promises those shares away: only 50 stay free.
    let ask = get(&player, "/api/symbols/ACME/book").await.1["ask_cents"]
        .as_i64()
        .unwrap();
    let (status, body) = post(&player,
        "/api/symbols/ACME/orders",
        json!({ "trader_id": id, "side": "sell", "qty": 150, "type": "limit", "price_cents": ask * 2 }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["status"], "resting");
    let order_id = body["order_id"].as_u64().unwrap();
    let (_, p) = get(&player, &format!("/api/traders/{id}")).await;
    assert_eq!(p["positions"][0]["reserved_shares"], 150);
    assert_eq!(p["positions"][0]["free_shares"], 50);

    for (qty, expected) in [
        (51, StatusCode::UNPROCESSABLE_ENTITY),
        (50, StatusCode::CREATED),
    ] {
        let (status, body) = post(
            &player,
            "/api/symbols/ACME/orders",
            json!({ "trader_id": id, "side": "sell", "qty": qty, "type": "market" }),
        )
        .await;
        assert_eq!(status, expected, "selling {qty} of 50 free: {body}");
    }
    let (_, p) = get(&player, &format!("/api/traders/{id}")).await;
    assert_eq!(p["positions"][0]["qty"], 150, "sold the 50 that were free");
    assert_eq!(p["positions"][0]["free_shares"], 0);

    // Cancelling the resting sell frees them again, and then everything can go.
    let (status, _) = delete(
        &player,
        &format!("/api/symbols/ACME/orders/{order_id}?trader_id={id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, p) = get(&player, &format!("/api/traders/{id}")).await;
    assert_eq!(p["positions"][0]["free_shares"], 150);
    let (status, body) = post(
        &player,
        "/api/symbols/ACME/orders",
        json!({ "trader_id": id, "side": "sell", "qty": 150, "type": "market" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (_, p) = get(&player, &format!("/api/traders/{id}")).await;
    assert_eq!(p["positions"][0]["qty"], 0);
    let (status, _) = post(
        &player,
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
    let player = sign_up(&app, "tara").await;
    let id = player.trader;
    post(
        &player,
        "/api/symbols/ACME/orders",
        json!({ "trader_id": id, "side": "buy", "qty": 500, "type": "market" }),
    )
    .await;
    let bid = get(&player, "/api/symbols/ACME/book").await.1["bid_cents"]
        .as_i64()
        .unwrap();
    post(&player,
        "/api/symbols/ACME/orders",
        json!({ "trader_id": id, "side": "buy", "qty": 40, "type": "limit", "price_cents": bid / 2 }),
    )
    .await;
    let (_, shares) = get(&player, "/api/symbols/ACME/shares").await;
    assert_eq!(shares["held_shares"], 500);
    assert_eq!(shares["bid_shares"], 40);
    assert_eq!(shares["available_shares"], outstanding - 540);
    assert_eq!(shares["holders"][0]["trader_id"], id);
    assert_eq!(shares["holders"][0]["qty"], 500);
    assert_eq!(shares["holders"][0]["free_shares"], 500);
    let (_, detail) = get(&player, "/api/symbols/ACME").await;
    assert_eq!(detail["info"]["shares_outstanding"], outstanding);
    assert_eq!(detail["shares"]["held_shares"], 500);
    // The totals are public; who holds them is not.
    let (_, anon) = get(&app, "/api/symbols/ACME/shares").await;
    assert_eq!(anon["held_shares"], 500);
    assert!(
        anon["holders"].as_array().unwrap().is_empty(),
        "an anonymous caller sees no holders: {anon}"
    );

    // Nobody can buy shares that do not exist — checked before the money is.
    let (status, body) = post(
        &player,
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
    let (_, nbla) = get(&player, "/api/symbols/NBLA/shares").await;
    assert_eq!(nbla["shares_outstanding"], 85_000_000);
    assert_eq!(nbla["available_shares"], 85_000_000);
}

#[tokio::test]
async fn a_users_shares_are_the_sum_of_their_traders() {
    let app = test_app();
    let (nina, _) = register(&app, json!({ "name": "nina" })).await;
    let mut ids = Vec::new();
    for name in ["alpha", "beta"] {
        let (status, body) = post(
            &nina,
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
            &nina,
            "/api/symbols/ACME/orders",
            json!({ "trader_id": trader, "side": "buy", "qty": qty, "type": "market" }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    let (status, body) = post(
        &nina,
        "/api/symbols/HLIO/orders",
        json!({ "trader_id": beta, "side": "buy", "qty": 300, "type": "market" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let (status, holdings) = get(&nina, "/api/users/1/holdings").await;
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
    let (_, user) = get(&nina, "/api/users/1").await;
    assert_eq!(user["shares_owned"], 440);
    assert_eq!(user["holdings_value_cents"], holdings["market_value_cents"]);

    // Shares belong to the trader that bought them: alpha cannot sell beta's.
    let (status, body) = post(
        &nina,
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
        &nina,
        "/api/symbols/HLIO/orders",
        json!({ "trader_id": alpha, "side": "sell", "qty": 1, "type": "market" }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // A resting sell shows up as reserved for the whole user.
    let ask = get(&nina, "/api/symbols/ACME/book").await.1["ask_cents"]
        .as_i64()
        .unwrap();
    post(&nina,
        "/api/symbols/ACME/orders",
        json!({ "trader_id": alpha, "side": "sell", "qty": 60, "type": "limit", "price_cents": ask * 2 }),
    )
    .await;
    let (_, holdings) = get(&nina, "/api/users/1/holdings").await;
    assert_eq!(holdings["holdings"][0]["reserved_shares"], 60);
    assert_eq!(holdings["holdings"][0]["free_shares"], 80);
    assert_eq!(holdings["shares_owned"], 440, "reserving sells nothing");
    assert_eq!(holdings["free_shares"], 380);

    let (status, body) = get(&nina, "/api/users/99/holdings").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"]["code"], "forbidden");
}

#[tokio::test]
async fn a_client_order_id_makes_submission_idempotent() {
    let app = test_app();
    let ida = sign_up(&app, "ida").await;
    let id = ida.trader;
    let order = json!({ "trader_id": id, "side": "buy", "qty": 25, "type": "market", "client_order_id": "abc-1" });

    let (status, first) = post(&ida, "/api/symbols/ACME/orders", order.clone()).await;
    assert_eq!(status, StatusCode::CREATED, "{first}");
    assert_eq!(first["filled"], 25);

    // The same order again: placed once, and the first answer comes back.
    let (status, again) = post(&ida, "/api/symbols/ACME/orders", order.clone()).await;
    assert_eq!(status, StatusCode::OK, "a replay is not a new order");
    assert_eq!(again, first, "the original response, verbatim");
    let (_, p) = get(&ida, &format!("/api/traders/{id}")).await;
    assert_eq!(p["positions"][0]["qty"], 25, "bought once, not twice");

    // The same id for a different order is refused, not quietly obeyed.
    let (status, body) = post(&ida,
        "/api/symbols/ACME/orders",
        json!({ "trader_id": id, "side": "buy", "qty": 50, "type": "market", "client_order_id": "abc-1" }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "duplicate_client_order_id");

    // Ids are per trader, so somebody else may use the same one.
    let ivan = sign_up(&app, "ivan").await;
    let other = ivan.trader;
    let (status, body) = post(&ivan,
        "/api/symbols/ACME/orders",
        json!({ "trader_id": other, "side": "buy", "qty": 25, "type": "market", "client_order_id": "abc-1" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_ne!(body["order_id"], first["order_id"]);

    // An empty id is no id at all, and an oversized one is refused.
    for (value, expected) in [
        (json!("   "), StatusCode::CREATED),
        (json!("x".repeat(65)), StatusCode::BAD_REQUEST),
    ] {
        let (status, body) = post(&ida,
            "/api/symbols/ACME/orders",
            json!({ "trader_id": id, "side": "buy", "qty": 1, "type": "market", "client_order_id": value }),
        )
        .await;
        assert_eq!(status, expected, "{body}");
    }
}

#[tokio::test]
async fn orders_can_be_looked_up_after_they_are_done() {
    let app = test_app();
    let player = sign_up(&app, "otto").await;
    let id = player.trader;

    // One filled market order…
    let (_, filled) = post(&player,
        "/api/symbols/ACME/orders",
        json!({ "trader_id": id, "side": "buy", "qty": 10, "type": "market", "client_order_id": "m-1" }),
    )
    .await;
    let filled_id = filled["order_id"].as_u64().unwrap();
    // …and one resting limit, later cancelled.
    let bid = get(&player, "/api/symbols/ACME/book").await.1["bid_cents"]
        .as_i64()
        .unwrap();
    let (_, resting) = post(&player,
        "/api/symbols/ACME/orders",
        json!({ "trader_id": id, "side": "buy", "qty": 5, "type": "limit", "price_cents": bid / 2 }),
    )
    .await;
    let resting_id = resting["order_id"].as_u64().unwrap();

    // The book has forgotten the filled order; the log has not.
    let (status, _) = get(&player, &format!("/api/symbols/ACME/orders/{filled_id}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "not resting any more");
    let (status, record) = get(&player, &format!("/api/orders/{filled_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(record["status"], "filled");
    assert_eq!(record["client_order_id"], "m-1");
    assert_eq!(record["kind"], "market");
    assert_eq!(record["price_cents"], Value::Null);
    assert_eq!(record["filled"], 10);
    assert_eq!(record["remaining"], 0);
    assert!(record["avg_price_cents"].as_f64().unwrap() > 0.0);
    assert_eq!(record["submitted_at_ms"], record["updated_at_ms"]);

    let (_, records) = get(&player, &format!("/api/traders/{id}/orders")).await;
    assert_eq!(records.as_array().unwrap().len(), 2);
    assert_eq!(records[0]["order_id"], resting_id, "newest first");
    assert_eq!(records[0]["status"], "resting");
    assert_eq!(records[0]["price_cents"], bid / 2);
    assert_eq!(records[1]["order_id"], filled_id);

    let (_, open) = get(&player, &format!("/api/traders/{id}/orders?status=resting")).await;
    assert_eq!(open.as_array().unwrap().len(), 1);
    assert_eq!(open[0]["order_id"], resting_id);
    let (status, body) = get(&player, &format!("/api/traders/{id}/orders?status=nope")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // Cancelling moves the record on.
    delete(
        &player,
        &format!("/api/symbols/ACME/orders/{resting_id}?trader_id={id}"),
    )
    .await;
    let (_, record) = get(&player, &format!("/api/orders/{resting_id}")).await;
    assert_eq!(record["status"], "cancelled");
    assert_eq!(record["filled"], 0);
    assert_eq!(record["remaining"], 5);
    let (_, cancelled) = get(
        &player,
        &format!("/api/traders/{id}/orders?status=cancelled"),
    )
    .await;
    assert_eq!(cancelled.as_array().unwrap().len(), 1);

    let (status, body) = get(&player, "/api/orders/9999").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"]["code"], "unknown_order");
}

#[tokio::test]
async fn a_resting_order_record_follows_its_fills() {
    let app = test_app();
    let mae = sign_up(&app, "mae").await;
    let tom = sign_up(&app, "tom").await;
    let (maker, taker) = (mae.trader, tom.trader);
    let book = get(&mae, "/api/symbols/HLIO/book").await.1;
    let mid = (book["bid_cents"].as_i64().unwrap() + book["ask_cents"].as_i64().unwrap()) / 2;

    let (_, resting) = post(&mae,
        "/api/symbols/HLIO/orders",
        json!({ "trader_id": maker, "side": "buy", "qty": 100, "type": "limit", "price_cents": mid }),
    )
    .await;
    let order_id = resting["order_id"].as_u64().unwrap();
    let (_, record) = get(&mae, &format!("/api/orders/{order_id}")).await;
    assert_eq!(record["status"], "resting");
    assert_eq!(record["filled"], 0);

    // Somebody sells into it: 40 of the 100 fill.
    post(
        &tom,
        "/api/symbols/HLIO/orders",
        json!({ "trader_id": taker, "side": "buy", "qty": 400, "type": "market" }),
    )
    .await;
    post(&tom,
        "/api/symbols/HLIO/orders",
        json!({ "trader_id": taker, "side": "sell", "qty": 40, "type": "limit", "price_cents": mid, "tif": "ioc" }),
    )
    .await;
    let (_, record) = get(&mae, &format!("/api/orders/{order_id}")).await;
    assert_eq!(
        record["filled"], 40,
        "the maker's record moved with the fill"
    );
    assert_eq!(record["remaining"], 60);
    assert_eq!(record["status"], "resting");
    assert_eq!(record["avg_price_cents"], mid as f64);
    assert!(record["updated_at_ms"].as_i64() >= record["submitted_at_ms"].as_i64());

    // The rest fills: the record closes.
    post(&tom,
        "/api/symbols/HLIO/orders",
        json!({ "trader_id": taker, "side": "sell", "qty": 60, "type": "limit", "price_cents": mid, "tif": "ioc" }),
    )
    .await;
    let (_, record) = get(&mae, &format!("/api/orders/{order_id}")).await;
    assert_eq!(record["status"], "filled");
    assert_eq!(record["remaining"], 0);
    assert_eq!(record["notional_cents"], mid * 100);
}

#[tokio::test]
async fn private_endpoints_need_the_users_own_key() {
    let app = test_app();
    let mine = sign_up(&app, "mina").await;
    let theirs = sign_up(&app, "theo").await;
    let id = mine.trader;
    let account = get(&mine, &format!("/api/traders/{id}")).await.1["account_id"]
        .as_u64()
        .unwrap();

    // No key at all: 401 on everything that belongs to somebody.
    for uri in [
        format!("/api/traders/{id}"),
        format!("/api/traders/{id}/orders"),
        format!("/api/accounts/{account}"),
        format!("/api/accounts/{account}/ledger"),
        format!("/api/accounts/{account}/validate"),
        format!("/api/users/{}", mine.user),
        format!("/api/users/{}/holdings", mine.user),
        "/api/traders".into(),
        "/api/users".into(),
        "/api/accounts".into(),
    ] {
        let (status, body) = get(&app, &uri).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}: {body}");
        assert_eq!(body["error"]["code"], "unauthenticated", "{uri}");
    }
    let (status, body) = post(
        &app,
        "/api/symbols/ACME/orders",
        json!({ "trader_id": id, "side": "buy", "qty": 1, "type": "market" }),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");

    // Somebody else's key: 403, and their money stays where it is.
    for uri in [
        format!("/api/traders/{id}"),
        format!("/api/traders/{id}/orders"),
        format!("/api/accounts/{account}"),
        format!("/api/accounts/{account}/ledger"),
        format!("/api/users/{}", mine.user),
        format!("/api/users/{}/holdings", mine.user),
    ] {
        let (status, body) = get(&theirs, &uri).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri}: {body}");
        assert_eq!(body["error"]["code"], "forbidden", "{uri}");
    }
    for (uri, body) in [
        (
            "/api/symbols/ACME/orders".to_string(),
            json!({ "trader_id": id, "side": "buy", "qty": 1, "type": "market" }),
        ),
        (
            format!("/api/traders/{id}/deposit"),
            json!({ "amount_cents": 1_000 }),
        ),
        (
            format!("/api/accounts/{account}/withdraw"),
            json!({ "amount_cents": 1_000 }),
        ),
        (
            format!("/api/accounts/{account}/status"),
            json!({ "status": "closed" }),
        ),
        (format!("/api/traders/{id}/cancel_all"), json!({})),
    ] {
        let (status, resp) = post(&theirs, &uri, body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri}: {resp}");
    }
    let (_, p) = get(&mine, &format!("/api/traders/{id}")).await;
    assert_eq!(p["cash_cents"], 10_000_000, "nothing moved");
    assert_eq!(p["account_status"], "active");

    // A key that was never issued is no better than none.
    let forged = Player {
        app: Arc::clone(&app),
        user: mine.user,
        trader: id,
        key: "fehu_0123456789abcdef0123456789abcdef".into(),
    };
    let (status, body) = get(&forged, &format!("/api/traders/{id}")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["error"]["code"], "invalid_api_key");

    // Market data stays open to everyone.
    for uri in [
        "/api/health",
        "/api/symbols",
        "/api/symbols/ACME",
        "/api/symbols/ACME/book",
        "/api/symbols/ACME/trades",
        "/api/symbols/ACME/shares",
        "/api/symbols/ACME/bars?interval=M1&limit=5",
        "/api/events",
        "/api/game/catalog",
    ] {
        let (status, body) = get(&app, uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
    }
}

#[tokio::test]
async fn listings_show_the_caller_their_own_only() {
    let app = test_app();
    let mine = sign_up(&app, "mina").await;
    let theirs = sign_up(&app, "theo").await;

    let (_, traders) = get(&mine, "/api/traders").await;
    let ids: Vec<u64> = traders
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_u64().unwrap())
        .collect();
    assert_eq!(ids, vec![mine.trader], "not theo's");

    let (_, users) = get(&mine, "/api/users").await;
    assert_eq!(users.as_array().unwrap().len(), 1);
    assert_eq!(users[0]["id"], mine.user);
    assert!(
        users[0]["api_key"].is_null(),
        "the key is shown once, at creation only: {users}"
    );

    let (_, accounts) = get(&theirs, "/api/accounts").await;
    assert_eq!(accounts.as_array().unwrap().len(), 1);
    assert_eq!(accounts[0]["user_id"], theirs.user);

    // The key is accepted in either header.
    let (status, body) = call(
        &app,
        Request::get(format!("/api/traders/{}", mine.trader))
            .header("x-api-key", &mine.key)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["id"], mine.trader);
}

#[tokio::test]
async fn the_game_master_endpoints_can_be_locked() {
    // With `admin_key` set, pushing events into the simulation needs it.
    let app = App::new(Options {
        history_days: 2,
        warmup_hours: 1,
        now_ms: Some(NOW_MS),
        admin_key: Some("s3cret".into()),
        ..Options::default()
    });
    let event = json!({ "kind": "scandal", "symbol": "ACME", "magnitude": 1.0 });

    let (status, body) = post(&app, "/api/game/events", event.clone()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    let player = sign_up(&app, "not-the-game-master").await;
    let (status, body) = post(&player, "/api/game/events", event.clone()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["error"]["code"], "invalid_api_key");

    let master = Player {
        app: Arc::clone(&app),
        user: 0,
        trader: 0,
        key: "s3cret".into(),
    };
    let (status, body) = post(&master, "/api/game/events", event).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let (status, body) = post(
        &master,
        "/api/symbols/ACME/events",
        json!({ "type": "jump", "pct": 0.01 }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");

    // Reading the log stays open.
    let (status, body) = get(&app, "/api/events").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["events"].as_array().unwrap().len(), 2);
}

/// An app whose market keeps a trading calendar, and whose "now" is `now_ms`.
fn app_with(options: Options) -> Arc<App> {
    App::new(Options {
        history_days: 2,
        warmup_hours: 1,
        ..options
    })
}

#[tokio::test]
async fn a_big_move_halts_trading_and_the_halt_lifts_itself() {
    let app = app_with(Options {
        now_ms: Some(NOW_MS),
        price_limit_pct: 0.05,
        halt_secs: 60,
        ..Options::default()
    });
    let player = sign_up(&app, "hal").await;
    let id = player.trader;

    let (_, status) = get(&app, "/api/symbols/ACME/status").await;
    assert_eq!(status["tradable"], true);
    assert_eq!(status["limit_pct"], 0.05);
    let band = status["band_cents"].as_i64().unwrap();
    assert!(band > 0, "the band starts at the price: {status}");

    // A jump well past the limit, then a tick to notice it.
    post(
        &app,
        "/api/symbols/ACME/events",
        json!({ "type": "jump", "pct": 0.20, "source": "test" }),
    )
    .await;
    engine::advance_to(&app, app.clock.now() + Duration::from_secs(2));

    let (_, status) = get(&app, "/api/symbols/ACME/status").await;
    assert_eq!(status["halted"], true, "{status}");
    assert_eq!(status["tradable"], false);
    assert_eq!(status["halt"]["reason"], "limit_move");
    assert_eq!(status["halt"]["band_cents"], band);
    assert!(
        status["halt"]["move_pct"].as_f64().unwrap() >= 0.05,
        "{status}"
    );
    let until = status["halt"]["until_ms"]
        .as_i64()
        .expect("an automatic halt ends");

    // No new orders while it is halted…
    let (code, body) = post(
        &player,
        "/api/symbols/ACME/orders",
        json!({ "trader_id": id, "side": "buy", "qty": 1, "type": "market" }),
    )
    .await;
    assert_eq!(code, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "symbol_halted");
    // …but the other symbols carry on.
    let (code, body) = post(
        &player,
        "/api/symbols/HLIO/orders",
        json!({ "trader_id": id, "side": "buy", "qty": 10, "type": "market" }),
    )
    .await;
    assert_eq!(
        code,
        StatusCode::CREATED,
        "one symbol halting is not four: {body}"
    );

    // The halt lifts by itself, and the band is measured again from here.
    engine::advance_to(&app, fehu::Timestamp(until + 1_000));
    let (_, status) = get(&app, "/api/symbols/ACME/status").await;
    assert_eq!(status["halted"], false, "{status}");
    assert_eq!(status["halt"], Value::Null);
    assert_ne!(status["band_cents"], band, "the band moved with the price");
    assert!(status["move_pct"].as_f64().unwrap().abs() < 0.05);
    let (code, body) = post(
        &player,
        "/api/symbols/ACME/orders",
        json!({ "trader_id": id, "side": "buy", "qty": 1, "type": "market" }),
    )
    .await;
    assert_eq!(code, StatusCode::CREATED, "{body}");
}

#[tokio::test]
async fn a_resting_order_survives_a_halt_and_can_be_cancelled() {
    let app = app_with(Options {
        now_ms: Some(NOW_MS),
        ..Options::default()
    });
    let player = sign_up(&app, "hana").await;
    let id = player.trader;
    let bid = get(&app, "/api/symbols/PXCO/book").await.1["bid_cents"]
        .as_i64()
        .unwrap();
    let (_, order) = post(
        &player,
        "/api/symbols/PXCO/orders",
        json!({ "trader_id": id, "side": "buy", "qty": 10, "type": "limit", "price_cents": bid / 2 }),
    )
    .await;
    let order_id = order["order_id"].as_u64().unwrap();

    let (code, status) = post(&app, "/api/symbols/PXCO/halt", json!({})).await;
    assert_eq!(code, StatusCode::OK, "{status}");
    assert_eq!(status["halt"]["reason"], "manual");
    assert_eq!(
        status["halt"]["until_ms"],
        Value::Null,
        "a manual halt has no end"
    );

    // It stays put through the halt: still resting, still reserving cash.
    let (_, p) = get(&player, &format!("/api/traders/{id}")).await;
    assert_eq!(p["open_orders"][0]["order_id"], order_id);
    assert!(p["reserved_cents"].as_i64().unwrap() > 0);
    // A player can always pull an order out of a stopped market.
    let (code, body) = delete(
        &player,
        &format!("/api/symbols/PXCO/orders/{order_id}?trader_id={id}"),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let (_, p) = get(&player, &format!("/api/traders/{id}")).await;
    assert!(p["open_orders"].as_array().unwrap().is_empty());
    assert_eq!(p["reserved_cents"], 0);

    // Time alone does not lift a manual halt.
    engine::advance_to(&app, app.clock.now() + Duration::from_secs(3_600));
    let (_, status) = get(&app, "/api/symbols/PXCO/status").await;
    assert_eq!(status["halted"], true, "{status}");
    let (code, status) = post(&app, "/api/symbols/PXCO/resume", json!({})).await;
    assert_eq!(code, StatusCode::OK, "{status}");
    assert_eq!(status["tradable"], true);
}

#[tokio::test]
async fn a_halt_freezes_the_book_and_resuming_settles_the_backlog() {
    let app = test_app();
    let player = sign_up(&app, "halina").await;
    let id = player.trader;
    let book = get(&player, "/api/symbols/NBLA/book").await.1;
    let bid = book["bid_cents"].as_i64().unwrap();
    let ask = book["ask_cents"].as_i64().unwrap();
    // Inside the spread: with the market open the next synthetic print takes
    // it, as `resting_bid_fills_when_the_market_trades_through_it` shows.
    let price = (bid + ask) / 2;
    let (code, order) = post(
        &player,
        "/api/symbols/NBLA/orders",
        json!({ "trader_id": id, "side": "buy", "qty": 50, "type": "limit", "price_cents": price }),
    )
    .await;
    assert_eq!(code, StatusCode::CREATED, "{order}");
    assert_eq!(order["status"], "resting");
    let resting = get(&player, "/api/symbols/NBLA/book").await.1;

    let (code, status) = post(&app, "/api/symbols/NBLA/halt", json!({})).await;
    assert_eq!(code, StatusCode::OK, "{status}");
    assert_eq!(status["halted"], true);

    // Ten minutes of price moves with trading stopped. The reference keeps
    // ticking, but nothing may execute against the frozen book.
    engine::advance_to(&app, Timestamp(NOW_MS + 600_000));
    let (_, p) = get(&player, &format!("/api/traders/{id}")).await;
    assert!(
        p["fills"].as_array().unwrap().is_empty(),
        "a halted book filled an order: {p}"
    );
    assert_eq!(p["open_orders"][0]["order_id"], order["order_id"]);
    assert_eq!(p["open_orders"][0]["remaining"], 50);
    assert_eq!(p["cash_cents"], 10_000_000);
    assert!(p["reserved_cents"].as_i64().unwrap() > 0);
    let frozen = get(&player, "/api/symbols/NBLA/book").await.1;
    assert_eq!(
        (&frozen["bids"], &frozen["asks"]),
        (&resting["bids"], &resting["asks"]),
        "the book moved while the symbol was halted"
    );
    assert_ne!(
        frozen["reference_cents"], resting["reference_cents"],
        "the reference price should keep moving through a halt"
    );

    // Resuming requotes around wherever the price went, and whatever that
    // crosses settles through the account like any other fill.
    let (code, status) = post(&app, "/api/symbols/NBLA/resume", json!({})).await;
    assert_eq!(code, StatusCode::OK, "{status}");
    assert_eq!(status["tradable"], true);
    let mut filled = 0;
    for k in 601..=1_200 {
        let (_, p) = get(&player, &format!("/api/traders/{id}")).await;
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
            assert_eq!(p["cash_cents"], 10_000_000 - 50 * price);
            break;
        }
        engine::advance_to(&app, Timestamp(NOW_MS + k * 1000));
    }
    assert_eq!(filled, 50, "the bid never filled after the resume");
    let (_, report) = get(&app, "/api/reconcile").await;
    assert_eq!(report["valid"], true, "{report}");
}

#[tokio::test]
async fn only_the_game_master_can_halt_a_symbol() {
    let app = app_with(Options {
        now_ms: Some(NOW_MS),
        admin_key: Some("s3cret".into()),
        ..Options::default()
    });
    let player = sign_up(&app, "hettie").await;

    let (code, _) = post(&app, "/api/symbols/ACME/halt", json!({})).await;
    assert_eq!(code, StatusCode::UNAUTHORIZED, "no key");
    let (code, body) = post(&player, "/api/symbols/ACME/halt", json!({})).await;
    assert_eq!(
        code,
        StatusCode::UNAUTHORIZED,
        "a player's key is not the master's"
    );
    assert_eq!(body["error"]["code"], "invalid_api_key");

    let master = Player {
        app: Arc::clone(&app),
        user: 0,
        trader: 0,
        key: "s3cret".into(),
    };
    let (code, status) = post(&master, "/api/symbols/ACME/halt", json!({})).await;
    assert_eq!(code, StatusCode::OK, "{status}");
    assert_eq!(status["halted"], true);
    // Reading the state stays open to everyone.
    let (code, status) = get(&app, "/api/symbols/ACME/status").await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(status["halted"], true);
    post(&master, "/api/symbols/ACME/resume", json!({})).await;
}

#[tokio::test]
async fn a_closed_session_takes_no_orders() {
    // 2023-11-14T22:13:20Z is a Tuesday evening: outside 09:30–16:00 UTC.
    let app = app_with(Options {
        now_ms: Some(NOW_MS),
        market_hours: Some(fehu::MarketHours::default()),
        ..Options::default()
    });
    let player = sign_up(&app, "nox").await;
    let id = player.trader;

    let (_, status) = get(&app, "/api/symbols/ACME/status").await;
    assert_eq!(status["market_open"], false, "{status}");
    assert_eq!(status["tradable"], false);
    assert_eq!(status["halted"], false, "closed is not halted");
    let next_open = status["next_open_ms"]
        .as_i64()
        .expect("a calendar has a next open");
    assert!(next_open > NOW_MS, "{status}");
    assert_eq!(
        status["next_close_ms"],
        Value::Null,
        "no session is running"
    );

    let (code, body) = post(
        &player,
        "/api/symbols/ACME/orders",
        json!({ "trader_id": id, "side": "buy", "qty": 1, "type": "market" }),
    )
    .await;
    assert_eq!(code, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "market_closed");
    let (_, quotes) = get(&app, "/api/symbols").await;
    assert_eq!(quotes["symbols"][0]["market_open"], false);
    assert_eq!(quotes["symbols"][0]["halted"], false);

    // Inside the session the same order goes through.
    let open = app_with(Options {
        // 2023-11-14T14:00:00Z, a Tuesday afternoon.
        now_ms: Some(1_699_970_400_000),
        market_hours: Some(fehu::MarketHours::default()),
        ..Options::default()
    });
    let player = sign_up(&open, "diurnal").await;
    let id = player.trader;
    let (_, status) = get(&open, "/api/symbols/ACME/status").await;
    assert_eq!(status["market_open"], true, "{status}");
    assert_eq!(status["tradable"], true);
    assert!(status["next_close_ms"].as_i64().unwrap() > 1_699_970_400_000);
    let (code, body) = post(
        &player,
        "/api/symbols/ACME/orders",
        json!({ "trader_id": id, "side": "buy", "qty": 5, "type": "market" }),
    )
    .await;
    assert_eq!(code, StatusCode::CREATED, "{body}");
}

#[tokio::test]
async fn a_trader_cannot_trade_with_itself() {
    let app = test_app();
    let me = sign_up(&app, "sol").await;
    let other = sign_up(&app, "luna").await;
    let id = me.trader;
    let book = get(&app, "/api/symbols/PXCO/book").await.1;
    let (bid, ask) = (
        book["bid_cents"].as_i64().unwrap(),
        book["ask_cents"].as_i64().unwrap(),
    );

    // Rest a buy at the top of the book, then try to sell into it.
    let (status, resting) = post(
        &me,
        "/api/symbols/PXCO/orders",
        json!({ "trader_id": id, "side": "buy", "qty": 40, "type": "limit", "price_cents": bid + 5 }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{resting}");
    let resting_id = resting["order_id"].as_u64().unwrap();

    // Enough shares to sell, bought elsewhere so the sell is about the cross.
    post(
        &me,
        "/api/symbols/PXCO/orders",
        json!({ "trader_id": id, "side": "buy", "qty": 100, "type": "market" }),
    )
    .await;

    let (status, body) = post(
        &me,
        "/api/symbols/PXCO/orders",
        json!({ "trader_id": id, "side": "sell", "qty": 40, "type": "limit", "price_cents": bid }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "self_trade");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains(&resting_id.to_string()),
        "the message names the order in the way: {body}"
    );

    // Somebody else's order at the same price is not a self-trade.
    post(
        &other,
        "/api/symbols/PXCO/orders",
        json!({ "trader_id": other.trader, "side": "buy", "qty": 50, "type": "market" }),
    )
    .await;
    let (status, body) = post(
        &other,
        "/api/symbols/PXCO/orders",
        json!({ "trader_id": other.trader, "side": "sell", "qty": 40, "type": "limit", "price_cents": bid }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["trades"][0]["maker_trader"], id, "it hit my bid");

    // A sell that stops short of my own bid is fine.
    let (status, body) = post(
        &me,
        "/api/symbols/PXCO/orders",
        json!({ "trader_id": id, "side": "sell", "qty": 1, "type": "limit", "price_cents": ask * 2 }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "resting well above: {body}");
    let far = body["order_id"].as_u64().unwrap();

    // And once the bid is out of the way, the crossing sell goes through.
    delete(
        &me,
        &format!("/api/symbols/PXCO/orders/{resting_id}?trader_id={id}"),
    )
    .await;
    delete(
        &me,
        &format!("/api/symbols/PXCO/orders/{far}?trader_id={id}"),
    )
    .await;
    let (status, body) = post(
        &me,
        "/api/symbols/PXCO/orders",
        json!({ "trader_id": id, "side": "sell", "qty": 40, "type": "market" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

#[tokio::test]
async fn a_post_only_order_must_rest() {
    let app = test_app();
    let player = sign_up(&app, "polly").await;
    let id = player.trader;
    let ask = get(&app, "/api/symbols/ACME/book").await.1["ask_cents"]
        .as_i64()
        .unwrap();

    // A buy at the offer would trade, so post-only refuses it.
    let (status, body) = post(
        &player,
        "/api/symbols/ACME/orders",
        json!({ "trader_id": id, "side": "buy", "qty": 10, "type": "limit", "price_cents": ask, "post_only": true }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "post_only_would_cross");

    // The same order without the flag trades, as it always did.
    let (status, body) = post(
        &player,
        "/api/symbols/ACME/orders",
        json!({ "trader_id": id, "side": "buy", "qty": 10, "type": "limit", "price_cents": ask }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["status"], "filled");

    // Below the offer it rests, which is the whole point.
    let (status, body) = post(
        &player,
        "/api/symbols/ACME/orders",
        json!({ "trader_id": id, "side": "buy", "qty": 10, "type": "limit", "price_cents": ask / 2, "post_only": true }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["status"], "resting");

    // Post-only makes no sense for orders that exist to take.
    for order in [
        json!({ "trader_id": id, "side": "buy", "qty": 1, "type": "market", "post_only": true }),
        json!({ "trader_id": id, "side": "buy", "qty": 1, "type": "limit", "price_cents": ask / 2, "tif": "ioc", "post_only": true }),
    ] {
        let (status, body) = post(&player, "/api/symbols/ACME/orders", order).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(body["error"]["code"], "invalid_order");
    }
}

#[tokio::test]
async fn an_order_can_be_amended_in_place() {
    let app = test_app();
    let maker = sign_up(&app, "ada").await;
    let taker = sign_up(&app, "bo").await;
    let id = maker.trader;
    let book = get(&app, "/api/symbols/HLIO/book").await.1;
    let mid = (book["bid_cents"].as_i64().unwrap() + book["ask_cents"].as_i64().unwrap()) / 2;

    post(
        &maker,
        "/api/symbols/HLIO/orders",
        json!({ "trader_id": id, "side": "buy", "qty": 200, "type": "market" }),
    )
    .await;
    let (_, resting) = post(
        &maker,
        "/api/symbols/HLIO/orders",
        json!({ "trader_id": id, "side": "sell", "qty": 100, "type": "limit", "price_cents": mid }),
    )
    .await;
    let first = resting["order_id"].as_u64().unwrap();

    // Somebody takes 40 of it, so the amendment starts from what is left.
    post(
        &taker,
        "/api/symbols/HLIO/orders",
        json!({ "trader_id": taker.trader, "side": "buy", "qty": 40, "type": "limit", "price_cents": mid, "tif": "ioc" }),
    )
    .await;

    let (status, amended) = call(
        &maker,
        Request::patch(format!("/api/symbols/HLIO/orders/{first}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({ "trader_id": id, "price_cents": mid + 50 }).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{amended}");
    assert_eq!(amended["replaced_order_id"], first);
    assert_eq!(amended["replaced_filled"], 40, "what had already traded");
    assert_eq!(amended["qty"], 60, "the rest, unless another is asked for");
    assert_eq!(amended["status"], "resting");
    let second = amended["order_id"].as_u64().unwrap();
    assert_ne!(second, first, "an amendment is a new order");

    // The old one is gone from the book and closed in the log; the new one
    // rests at its new price with the shares still reserved against it.
    let (status, _) = get(&maker, &format!("/api/symbols/HLIO/orders/{first}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, record) = get(&maker, &format!("/api/orders/{first}")).await;
    assert_eq!(record["status"], "cancelled");
    assert_eq!(record["filled"], 40);
    let (_, p) = get(&maker, &format!("/api/traders/{id}")).await;
    assert_eq!(p["open_orders"].as_array().unwrap().len(), 1);
    assert_eq!(p["open_orders"][0]["order_id"], second);
    assert_eq!(p["open_orders"][0]["price_cents"], mid + 50);
    assert_eq!(p["positions"][0]["reserved_shares"], 60);

    // Quantity alone can be amended too.
    let (status, amended) = call(
        &maker,
        Request::patch(format!("/api/symbols/HLIO/orders/{second}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({ "trader_id": id, "qty": 25 }).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{amended}");
    assert_eq!(amended["qty"], 25);
    assert_eq!(amended["replaced_filled"], 0);
    let (_, p) = get(&maker, &format!("/api/traders/{id}")).await;
    assert_eq!(
        p["positions"][0]["reserved_shares"], 25,
        "the rest is free again"
    );

    // Somebody else's order, and one that is not resting, cannot be amended.
    let third = amended["order_id"].as_u64().unwrap();
    let (status, body) = call(
        &taker,
        Request::patch(format!("/api/symbols/HLIO/orders/{third}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({ "trader_id": taker.trader, "qty": 1 }).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "not theirs to amend: {body}");
    let (status, body) = call(
        &maker,
        Request::patch(format!("/api/symbols/HLIO/orders/{first}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(json!({ "trader_id": id, "qty": 1 }).to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "already replaced: {body}");
}

#[tokio::test]
async fn reconciliation_checks_a_busy_market_and_detects_broken_reservations() {
    let app = test_app();
    let player = sign_up(&app, "audit").await;
    for order in [
        json!({"trader_id":player.trader,"type":"market","side":"buy","qty":100}),
        json!({"trader_id":player.trader,"type":"limit","side":"buy","qty":10,"price_cents":1}),
        json!({"trader_id":player.trader,"type":"limit","side":"sell","qty":10,"price_cents":1000000}),
    ] {
        let (status, body) = post(&player, "/api/symbols/ACME/orders", order).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    engine::step(&app);
    let (status, report) = get(&app, "/api/reconcile").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(report["valid"], true, "{report}");
    assert_eq!(report["resting_orders_checked"], 2);
    let restored = App::restore(app.options.clone(), app.save());
    assert!(restored.market().reconcile().valid);
    {
        let mut market = app.market();
        let account_id = market.traders[&fehu::TraderId(player.trader)].account_id;
        market.accounts.get_mut(&account_id).unwrap().reserve(1);
        market
            .traders
            .get_mut(&fehu::TraderId(player.trader))
            .unwrap()
            .reserved_shares
            .insert("ACME", 101);
    }
    let (_, report) = get(&app, "/api/reconcile").await;
    assert_eq!(report["valid"], false);
    let issues = report["issues"].to_string();
    assert!(issues.contains("cash reservation"), "{report}");
    assert!(issues.contains("share reservation"), "{report}");
    assert!(issues.contains("exceed holdings"), "{report}");
}

#[tokio::test]
async fn reconciliation_requires_the_configured_admin_key() {
    let app = app_with(Options {
        admin_key: Some("audit-secret".into()),
        ..Options::default()
    });
    assert_eq!(
        get(&app, "/api/reconcile").await.0,
        StatusCode::UNAUTHORIZED
    );
    let player = sign_up(&app, "player").await;
    assert_eq!(
        get(&player, "/api/reconcile").await.0,
        StatusCode::UNAUTHORIZED
    );
    let master = Player {
        app,
        user: 0,
        trader: 0,
        key: "audit-secret".into(),
    };
    assert_eq!(get(&master, "/api/reconcile").await.0, StatusCode::OK);
}

#[tokio::test]
async fn order_ids_are_unique_across_symbols_and_survive_retries() {
    let app = App::new(Options {
        history_days: 0,
        warmup_hours: 0,
        now_ms: Some(NOW_MS),
        ..Options::default()
    });
    let player = sign_up(&app, "multi-symbol").await;
    let mut orders = Vec::new();
    for symbol in ["ACME", "NBLA", "HLIO", "PXCO"] {
        let request = json!({"trader_id":player.trader,"side":"buy","type":"limit","price_cents":1,"qty":10,"client_order_id":symbol});
        let (status, response) = post(
            &player,
            &format!("/api/symbols/{symbol}/orders"),
            request.clone(),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{response}");
        orders.push((symbol, request, response));
    }
    let ids: std::collections::BTreeSet<_> = orders
        .iter()
        .map(|(_, _, r)| r["order_id"].as_u64().unwrap())
        .collect();
    assert_eq!(ids.len(), 4, "each symbol's order needs its own identity");
    let restored = App::restore(app.options.clone(), app.save());
    let player = Player {
        app: restored,
        user: player.user,
        trader: player.trader,
        key: player.key,
    };
    let (status, new_order) = post(
        &player,
        "/api/symbols/ACME/orders",
        json!({"trader_id":player.trader,"side":"buy","type":"limit","price_cents":1,"qty":1}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(!ids.contains(&new_order["order_id"].as_u64().unwrap()));
    for (symbol, request, response) in orders {
        let (status, retry) =
            post(&player, &format!("/api/symbols/{symbol}/orders"), request).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(retry, response);
        let (status, record) = get(&player, &format!("/api/orders/{}", response["order_id"])).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(record["symbol"], symbol);
    }
}

#[tokio::test]
async fn a_lagging_stream_disconnects_instead_of_silently_skipping_messages() {
    let app = App::new(Options {
        history_days: 0,
        warmup_hours: 0,
        now_ms: Some(NOW_MS),
        ..Options::default()
    });
    let response = router(Arc::clone(&app))
        .oneshot(Request::get("/api/stream").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body();
    let hello = body.frame().await.unwrap().unwrap().into_data().unwrap();
    assert!(std::str::from_utf8(&hello).unwrap().contains("hello"));
    // Do not poll the body while overflowing its bounded receiver.
    for _ in 0..8192 {
        app.tx
            .send(fehu_webapp::market::StreamMessage::Hello {
                sim_now_ms: NOW_MS,
                time_scale: 1.0,
                quotes: Vec::new(),
            })
            .unwrap();
    }
    let next = tokio::time::timeout(Duration::from_secs(1), body.frame())
        .await
        .expect("a gap must terminate promptly");
    assert!(next.is_none(), "later messages must not hide the gap");
}
