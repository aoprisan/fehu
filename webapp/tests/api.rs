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
