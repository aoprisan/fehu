//! The JSON contract the TypeScript UI is typed against.
//!
//! Every assertion here pins the exact key set of one response, mirroring a
//! declaration in `webapp/ui/src/types.ts`. Renaming, adding or removing a
//! serialised field fails these tests, so the break surfaces here instead of
//! as an `undefined` in the browser. When one fails, update both sides.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use fehu_webapp::market::{App, Options, StreamMessage};
use fehu_webapp::{engine, router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::util::ServiceExt;

/// 2023-11-14T22:13:20Z, as in `api.rs`. Fixed so the tests are deterministic.
const NOW_MS: i64 = 1_700_000_000_000;

fn test_app() -> Arc<App> {
    App::new(Options {
        history_days: 2,
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

async fn get(app: &Arc<App>, uri: &str) -> Value {
    let (status, body) = call(app, Request::get(uri).body(Body::empty()).unwrap()).await;
    assert!(status.is_success(), "GET {uri} → {status}: {body}");
    body
}

async fn post(app: &Arc<App>, uri: &str, body: Value) -> Value {
    let (status, body) = call(
        app,
        Request::post(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await;
    assert!(status.is_success(), "POST {uri} → {status}: {body}");
    body
}

/// Assert that `value` is an object with exactly `expected` as its keys.
#[track_caller]
fn assert_keys(what: &str, value: &Value, expected: &[&str]) {
    let object = value
        .as_object()
        .unwrap_or_else(|| panic!("{what}: expected a JSON object, got {value}"));
    let mut got: Vec<&str> = object.keys().map(String::as_str).collect();
    let mut want: Vec<&str> = expected.to_vec();
    got.sort_unstable();
    want.sort_unstable();
    assert_eq!(
        got, want,
        "{what}: JSON keys drifted from `webapp/ui/src/types.ts` — update both",
    );
}

/// The first element of an array field, which must not be empty.
#[track_caller]
fn first<'a>(what: &str, value: &'a Value, field: &str) -> &'a Value {
    value[field]
        .as_array()
        .unwrap_or_else(|| panic!("{what}: `{field}` is not an array: {value}"))
        .first()
        .unwrap_or_else(|| panic!("{what}: `{field}` is empty, nothing to check"))
}

#[tokio::test]
async fn market_data_shapes() {
    let app = test_app();

    let symbols = get(&app, "/api/symbols").await;
    assert_keys("SymbolsResponse", &symbols, &["sim_now_ms", "symbols"]);
    assert_keys(
        "Quote",
        first("SymbolsResponse", &symbols, "symbols"),
        &[
            "symbol",
            "name",
            "sector",
            "ts_ms",
            "price_cents",
            "prev_close_cents",
            "change_pct",
            "day_open_cents",
            "day_high_cents",
            "day_low_cents",
            "day_volume",
            "fundamental_cents",
            "annual_vol",
            "pending_events",
            "bid_cents",
            "ask_cents",
            "shares_outstanding",
            "market_cap_cents",
        ],
    );

    let shares = get(&app, "/api/symbols/ACME/shares").await;
    assert_keys(
        "SharesDto",
        &shares,
        &[
            "symbol",
            "shares_outstanding",
            "held_shares",
            "bid_shares",
            "available_shares",
            "price_cents",
            "market_cap_cents",
            "holders",
        ],
    );

    let bars = get(&app, "/api/symbols/ACME/bars?interval=M1&limit=5").await;
    assert_keys(
        "BarsResponse",
        &bars,
        &["symbol", "interval", "interval_ms", "sim_now_ms", "bars"],
    );
    assert_keys(
        "Candle",
        first("BarsResponse", &bars, "bars"),
        &["open_ts", "open", "high", "low", "close", "volume", "ticks"],
    );
    assert!(
        bars["bars"][0]["open_ts"].is_i64(),
        "Candle.open_ts must stay a bare number (`Timestamp` is #[serde(transparent)])",
    );
    assert_eq!(
        bars["interval"], "M1",
        "Interval serialises as its variant name"
    );

    let book = get(&app, "/api/symbols/ACME/book?depth=4").await;
    assert_keys(
        "BookResponse",
        &book,
        &[
            "symbol",
            "ts_ms",
            "reference_cents",
            "bid_cents",
            "ask_cents",
            "pending_flow",
            // BookDto is flattened into the response.
            "bids",
            "asks",
        ],
    );
    assert_keys(
        "Level",
        first("BookResponse", &book, "bids"),
        &["price_cents", "qty", "orders"],
    );
}

#[tokio::test]
async fn event_shapes() {
    let app = test_app();

    let catalog = get(&app, "/api/game/catalog").await;
    assert_keys(
        "CatalogEntry",
        first("catalog", &json!({ "entries": catalog }), "entries"),
        &["kind", "label", "scope", "description", "effects"],
    );

    let record = post(
        &app,
        "/api/game/events",
        json!({ "kind": "scandal", "symbol": "ACME", "magnitude": 1.5, "source": "test" }),
    )
    .await;
    assert_keys(
        "EventRecord",
        &record,
        &[
            "id",
            "received_at_ms",
            "at_ms",
            "symbols",
            "kind",
            "source",
            "note",
            "magnitude",
            "effects",
            "summary",
        ],
    );

    // SimEvent is internally tagged: `{"type": "...", ...}`.
    let effect = first("EventRecord", &record, "effects");
    assert!(
        effect["type"].is_string(),
        "SimEvent must stay internally tagged on `type`: {effect}",
    );

    let events = get(&app, "/api/events?limit=10").await;
    assert_keys("EventsResponse", &events, &["sim_now_ms", "events"]);

    let raw = post(
        &app,
        "/api/symbols/ACME/events",
        json!({ "type": "jump", "pct": 0.01, "source": "test" }),
    )
    .await;
    assert_eq!(raw["kind"], "sim:jump");
}

#[tokio::test]
async fn trading_shapes() {
    let app = test_app();

    let trader = post(&app, "/api/traders", json!({ "name": "contract" })).await;
    let portfolio_keys = [
        "id",
        "user_id",
        "account_id",
        "name",
        "created_at_ms",
        "account_status",
        "cash_cents",
        "reserved_cents",
        "free_cash_cents",
        "equity_cents",
        "realised_pnl_cents",
        "unrealised_pnl_cents",
        "positions",
        "open_orders",
        "fills",
    ];
    assert_keys("PortfolioDto", &trader, &portfolio_keys);
    let id = trader["id"].as_u64().unwrap();

    // A market buy fills against the synthetic ladder, giving a position,
    // a fill and prints on the tape.
    let order = post(
        &app,
        "/api/symbols/ACME/orders",
        json!({ "trader_id": id, "side": "buy", "type": "market", "qty": 50 }),
    )
    .await;
    assert_keys(
        "OrderResponse",
        &order,
        &[
            "symbol",
            "trader_id",
            "order_id",
            "side",
            "qty",
            "filled",
            "remaining",
            "status",
            "avg_price_cents",
            "notional_cents",
            "trades",
        ],
    );
    let trade_keys = [
        "ts_ms",
        "price_cents",
        "qty",
        "taker_side",
        "taker_order_id",
        "maker_order_id",
        "taker_trader",
        "maker_trader",
        "hidden",
    ];
    assert_keys(
        "TradeDto",
        first("OrderResponse", &order, "trades"),
        &trade_keys,
    );
    assert_eq!(order["side"], "buy", "Side serialises lower-case");

    // A far-from-the-market limit rests instead of filling.
    post(
        &app,
        "/api/symbols/ACME/orders",
        json!({ "trader_id": id, "side": "buy", "type": "limit", "price_cents": 1, "qty": 1, "tif": "gtc" }),
    )
    .await;

    let portfolio = get(&app, &format!("/api/traders/{id}")).await;
    assert_keys("PortfolioDto", &portfolio, &portfolio_keys);
    assert_keys(
        "PositionDto",
        first("PortfolioDto", &portfolio, "positions"),
        &[
            "symbol",
            "qty",
            "avg_cost_cents",
            "mark_cents",
            "market_value_cents",
            "unrealised_pnl_cents",
            "realised_pnl_cents",
            "reserved_shares",
            "free_shares",
        ],
    );
    assert_keys(
        "OpenOrderDto",
        first("PortfolioDto", &portfolio, "open_orders"),
        &[
            "symbol",
            "order_id",
            "trader_id",
            "side",
            "price_cents",
            "qty",
            "remaining",
            "ts_ms",
        ],
    );
    assert_keys(
        "FillRecord",
        first("PortfolioDto", &portfolio, "fills"),
        &[
            "id",
            "trader_id",
            "ts_ms",
            "symbol",
            "order_id",
            "side",
            "qty",
            "price_cents",
            "liquidity",
            "counterparty",
        ],
    );

    let holdings = get(
        &app,
        &format!("/api/users/{}/holdings", portfolio["user_id"]),
    )
    .await;
    assert_keys(
        "UserHoldingsResponse",
        &holdings,
        &[
            "user_id",
            "shares_owned",
            "reserved_shares",
            "free_shares",
            "market_value_cents",
            "holdings",
        ],
    );
    assert_keys(
        "HoldingDto",
        first("UserHoldingsResponse", &holdings, "holdings"),
        &[
            "symbol",
            "qty",
            "reserved_shares",
            "free_shares",
            "cost_cents",
            "avg_cost_cents",
            "mark_cents",
            "market_value_cents",
            "unrealised_pnl_cents",
            "realised_pnl_cents",
            "traders",
        ],
    );

    let shares = get(&app, "/api/symbols/ACME/shares").await;
    assert_keys(
        "HolderDto",
        first("SharesDto", &shares, "holders"),
        &[
            "trader_id",
            "user_id",
            "qty",
            "reserved_shares",
            "free_shares",
        ],
    );

    let trades = get(&app, "/api/symbols/ACME/trades?limit=5").await;
    assert_keys(
        "TradesResponse",
        &trades,
        &["symbol", "sim_now_ms", "trades"],
    );
    assert_keys(
        "TradeDto",
        first("TradesResponse", &trades, "trades"),
        &trade_keys,
    );
}

#[tokio::test]
async fn account_shapes() {
    let app = test_app();

    let user = post(
        &app,
        "/api/users",
        json!({ "name": "ada", "email": "ada@example.com" }),
    )
    .await;
    assert_keys(
        "UserDto",
        &user,
        &[
            "id",
            "name",
            "email",
            "created_at_ms",
            "accounts",
            "traders",
            "balance_cents",
            "shares_owned",
            "holdings_value_cents",
        ],
    );
    let user_id = user["id"].as_u64().unwrap();

    let account_keys = [
        "id",
        "user_id",
        "name",
        "status",
        "opened_at_ms",
        "balance_cents",
        "reserved_cents",
        "available_cents",
        "deposited_cents",
        "withdrawn_cents",
        "entries_total",
        "trader_id",
        "valid",
    ];
    let account = post(
        &app,
        &format!("/api/users/{user_id}/accounts"),
        json!({ "name": "main", "cash_cents": 250_000 }),
    )
    .await;
    assert_keys("AccountDto", &account, &account_keys);
    assert_eq!(account["status"], "active", "AccountStatus is lower-case");
    let account_id = account["id"].as_u64().unwrap();

    let ledger = post(
        &app,
        &format!("/api/accounts/{account_id}/deposit"),
        json!({ "amount_cents": 1_000, "memo": "allowance" }),
    )
    .await;
    assert_keys("LedgerResponse", &ledger, &["account", "entries"]);
    assert_keys("AccountDto", &ledger["account"], &account_keys);
    assert_keys(
        "LedgerEntry",
        first("LedgerResponse", &ledger, "entries"),
        &[
            "id",
            "ts_ms",
            "kind",
            "amount_cents",
            "balance_cents",
            "symbol",
            "order_id",
            "memo",
        ],
    );
    assert_eq!(ledger["entries"][0]["kind"], "deposit");

    let check = get(&app, &format!("/api/accounts/{account_id}/validate")).await;
    assert_keys(
        "AccountCheck",
        &check,
        &[
            "account_id",
            "status",
            "valid",
            "issues",
            "balance_cents",
            "reserved_cents",
            "available_cents",
            "can_trade",
            "can_deposit",
            "can_withdraw",
        ],
    );
    assert_eq!(check["valid"], true);
}

#[tokio::test]
async fn stream_message_shapes() {
    let app = test_app();

    // `hello` is built by the SSE handler from the same pieces.
    let hello = serde_json::to_value(StreamMessage::Hello {
        sim_now_ms: NOW_MS,
        time_scale: 1.0,
        quotes: Vec::new(),
    })
    .unwrap();
    assert_keys(
        "HelloMessage",
        &hello,
        &["type", "sim_now_ms", "time_scale", "quotes"],
    );
    assert_eq!(hello["type"], "hello");

    // Drive one engine step and capture what the stream would carry.
    let mut rx = app.tx.subscribe();
    let target = app.clock.now() + Duration::from_secs(120);
    engine::advance_to(&app, target);

    let mut saw_tick = false;
    while let Ok(message) = rx.try_recv() {
        let value = serde_json::to_value(&message).unwrap();
        if value["type"] == "tick" {
            assert_keys(
                "TickMessage",
                &value,
                &[
                    "type",
                    "symbol",
                    "ts_ms",
                    "price_cents",
                    "volume",
                    "closed",
                    "bid_cents",
                    "ask_cents",
                    "book",
                    "trades",
                ],
            );
            assert_keys("BookDto", &value["book"], &["bids", "asks"]);
            saw_tick = true;
            break;
        }
    }
    assert!(saw_tick, "the engine step published no tick to check");

    // `event` flattens the record next to the tag.
    let record = post(
        &app,
        "/api/game/events",
        json!({ "kind": "hype", "symbol": "ACME" }),
    )
    .await;
    let mut saw_event = false;
    while let Ok(message) = rx.try_recv() {
        let value = serde_json::to_value(&message).unwrap();
        if value["type"] == "event" {
            assert_eq!(
                value["id"], record["id"],
                "EventRecord is flattened into the message"
            );
            assert!(value["symbols"].is_array());
            saw_event = true;
        }
    }
    assert!(saw_event, "the accepted event was not published");
}

#[tokio::test]
async fn ui_assets_are_served() {
    let app = test_app();

    for (path, content_type) in [
        ("/", "text/html"),
        ("/assets/app.js", "text/javascript"),
        ("/assets/app.css", "text/css"),
    ] {
        let resp = router(Arc::clone(&app))
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{path}");
        assert!(
            resp.headers()[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with(content_type),
            "{path} has the wrong content type",
        );
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert!(!body.is_empty(), "{path} is empty — run `just ui`");
    }

    // The built index must reference the assets the router actually serves.
    let index = router(Arc::clone(&app))
        .oneshot(Request::get("/").body(Body::empty()).unwrap())
        .await
        .unwrap()
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
    let index = std::str::from_utf8(&index).unwrap();
    for asset in ["/assets/app.js", "/assets/app.css"] {
        assert!(
            index.contains(asset),
            "index.html does not reference {asset} — rebuild with `just ui`",
        );
    }
}
