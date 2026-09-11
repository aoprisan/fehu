//! The JSON contract the TypeScript UI is typed against.
//!
//! Every assertion here pins the exact key set of one response, mirroring a
//! declaration in `ui/src/types.ts`. Renaming, adding or removing a
//! serialised field fails these tests, so the break surfaces here instead of
//! as an `undefined` in the browser. When one fails, update both sides.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use fehu_economy::market::{App, Options, StreamMessage};
use fehu_economy::{engine, router};
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
        // Shapes, not timing: the limiter has its own tests.
        rate_per_sec: 0.0,
        ..Options::default()
    })
}

/// Send `req` as the holder of `key`, if there is one.
async fn call_as(app: &Arc<App>, key: Option<&str>, mut req: Request<Body>) -> (StatusCode, Value) {
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

async fn get(app: &Arc<App>, uri: &str) -> Value {
    get_as(app, None, uri).await
}

/// `GET` as the holder of `key`: everything that belongs to a user needs one.
async fn get_as(app: &Arc<App>, key: Option<&str>, uri: &str) -> Value {
    let (status, body) = call_as(app, key, Request::get(uri).body(Body::empty()).unwrap()).await;
    assert!(status.is_success(), "GET {uri} → {status}: {body}");
    body
}

async fn post(app: &Arc<App>, uri: &str, body: Value) -> Value {
    post_as(app, None, uri, body).await
}

async fn post_as(app: &Arc<App>, key: Option<&str>, uri: &str, body: Value) -> Value {
    let (status, body) = call_as(
        app,
        key,
        Request::post(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await;
    assert!(status.is_success(), "POST {uri} → {status}: {body}");
    body
}

async fn patch_as(app: &Arc<App>, key: Option<&str>, uri: &str, body: Value) -> Value {
    let (status, body) = call_as(
        app,
        key,
        Request::patch(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await;
    assert!(status.is_success(), "PATCH {uri} → {status}: {body}");
    body
}

/// The API key in a response that created a user, which is the only place it
/// is ever shown.
fn api_key_of(body: &Value) -> String {
    body["api_key"]
        .as_str()
        .unwrap_or_else(|| panic!("no api_key in {body}"))
        .to_owned()
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
        "{what}: JSON keys drifted from `ui/src/types.ts` — update both",
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
            "asset_kind",
            "unit",
            "shares_outstanding",
            "market_cap_cents",
            "market_open",
            "halted",
        ],
    );

    let shares = get(&app, "/api/symbols/ACME/shares").await;
    assert_keys(
        "SharesDto",
        &shares,
        &[
            "symbol",
            "asset_kind",
            "unit",
            "shares_outstanding",
            "held_shares",
            "bid_shares",
            "available_shares",
            "price_cents",
            "market_cap_cents",
            "holders",
        ],
    );

    let status_keys = [
        "symbol",
        "ts_ms",
        "market_open",
        "halted",
        "tradable",
        "halt",
        "next_open_ms",
        "next_close_ms",
        "band_cents",
        "move_pct",
        "limit_pct",
    ];
    let status = get(&app, "/api/symbols/ACME/status").await;
    assert_keys("SymbolStatus", &status, &status_keys);
    assert_eq!(status["tradable"], true);
    assert_eq!(
        status["halt"],
        Value::Null,
        "nothing is halted to begin with"
    );

    // Halting fills the `halt` in, and the game master is not locked out
    // here because this app sets no admin key.
    let halted = post(&app, "/api/symbols/ACME/halt", json!({})).await;
    assert_keys("SymbolStatus", &halted, &status_keys);
    assert_keys(
        "Halt",
        &halted["halt"],
        &[
            "reason",
            "since_ms",
            "until_ms",
            "band_cents",
            "price_cents",
            "move_pct",
        ],
    );
    assert_eq!(
        halted["halt"]["reason"], "manual",
        "HaltReason is snake_case"
    );
    assert_eq!(halted["tradable"], false);
    let resumed = post(&app, "/api/symbols/ACME/resume", json!({})).await;
    assert_eq!(resumed["halt"], Value::Null);
    assert_eq!(resumed["tradable"], true);

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
        &["kind", "label", "scope", "description", "effects", "world"],
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
    let key = api_key_of(&trader);
    let key = Some(key.as_str());
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
        "stops",
        "fills",
        "api_key",
    ];
    assert_keys("PortfolioDto", &trader, &portfolio_keys);
    let id = trader["id"].as_u64().unwrap();

    // A stop is its own shape: a trigger, not an order.
    let price = get(&app, "/api/symbols/ACME/book").await["reference_cents"]
        .as_i64()
        .unwrap();
    let stop = post_as(
        &app,
        key,
        "/api/symbols/ACME/stops",
        json!({ "trader_id": id, "side": "buy", "qty": 1, "stop_price_cents": price * 2 }),
    )
    .await;
    assert_keys(
        "StopOrder",
        &stop,
        &[
            "stop_id",
            "trader_id",
            "symbol",
            "side",
            "qty",
            "stop_price_cents",
            "limit_price_cents",
            "tif",
            "client_order_id",
            "created_at_ms",
        ],
    );

    // A market buy fills against the synthetic ladder, giving a position,
    // a fill and prints on the tape.
    let order = post_as(
        &app,
        key,
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
    post_as(
        &app,
        key,
        "/api/symbols/ACME/orders",
        json!({ "trader_id": id, "side": "buy", "type": "limit", "price_cents": 1, "qty": 1, "tif": "gtc" }),
    )
    .await;

    let portfolio = get_as(&app, key, &format!("/api/traders/{id}")).await;
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
            "display_qty",
            "shown_qty",
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
            "fee_cents",
        ],
    );

    // Amending replaces one resting order with another; the response is an
    // `OrderResponse` with the withdrawn order named alongside it.
    let resting_id = portfolio["open_orders"][0]["order_id"].as_u64().unwrap();
    let amended = patch_as(
        &app,
        key,
        &format!("/api/symbols/ACME/orders/{resting_id}"),
        json!({ "trader_id": id, "qty": 2 }),
    )
    .await;
    assert_keys(
        "AmendResponse",
        &amended,
        &[
            "replaced_order_id",
            "replaced_filled",
            // OrderResponse is flattened into it.
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
    assert_eq!(amended["replaced_order_id"], resting_id);

    let records = get_as(&app, key, &format!("/api/traders/{id}/orders")).await;
    assert_keys(
        "OrderRecord",
        first("orders", &json!({ "orders": records }), "orders"),
        &[
            "order_id",
            "client_order_id",
            "trader_id",
            "symbol",
            "kind",
            "price_cents",
            "side",
            "tif",
            "qty",
            "filled",
            "remaining",
            "status",
            "notional_cents",
            "avg_price_cents",
            "submitted_at_ms",
            "updated_at_ms",
            "expires_at_ms",
        ],
    );
    assert_eq!(records[0]["status"], "resting", "OrderStatus is lower-case");
    assert_eq!(records[0]["tif"], "gtc", "TimeInForce is lower-case");

    let holdings = get_as(
        &app,
        key,
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

    let shares = get_as(&app, key, "/api/symbols/ACME/shares").await;
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
    let key = api_key_of(&user);
    let key = Some(key.as_str());
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
            "api_key",
        ],
    );
    let user_id = user["id"].as_u64().unwrap();

    let account_keys = [
        "id",
        "user_id",
        "name",
        "status",
        "opened_at_ms",
        "wallet_id",
        "balance_cents",
        "reserved_cents",
        "available_cents",
        "deposited_cents",
        "withdrawn_cents",
        "entries_total",
        "trader_id",
        "valid",
    ];
    let account = post_as(
        &app,
        key,
        &format!("/api/users/{user_id}/accounts"),
        json!({ "name": "main", "cash_cents": 250_000 }),
    )
    .await;
    assert_keys("AccountDto", &account, &account_keys);
    assert_eq!(account["status"], "active", "AccountStatus is lower-case");
    let account_id = account["id"].as_u64().unwrap();

    let ledger = post_as(
        &app,
        key,
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
            "tx_id",
            "amount_cents",
            "balance_cents",
            "symbol",
            "order_id",
            "memo",
        ],
    );
    assert_eq!(ledger["entries"][0]["kind"], "deposit");
    assert!(
        ledger["entries"][0]["tx_id"].as_u64().unwrap() > 0,
        "every entry names the balanced transaction behind it"
    );

    let check = get_as(&app, key, &format!("/api/accounts/{account_id}/validate")).await;
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

    // `hello` is built by the SSE handler from the same pieces, and every
    // message goes out inside the envelope that numbers it.
    let hello = serde_json::to_value(fehu_economy::market::Sequenced {
        seq: 7,
        message: StreamMessage::Hello {
            sim_now_ms: NOW_MS,
            time_scale: 1.0,
            quotes: Vec::new(),
            oldest_seq: 1,
            gap: false,
        },
    })
    .unwrap();
    assert_keys(
        "HelloMessage",
        &hello,
        &[
            "seq",
            "type",
            "sim_now_ms",
            "time_scale",
            "quotes",
            "oldest_seq",
            "gap",
        ],
    );
    assert_eq!(hello["type"], "hello");
    assert_eq!(hello["seq"], 7, "the envelope numbers every message");

    // Drive one engine step and capture what the stream would carry.
    let mut rx = app.listen();
    let target = app.clock.now() + Duration::from_secs(120);
    engine::advance_to(&app, target).await;

    let mut saw_tick = false;
    let mut last_seq = 0;
    while let Ok(message) = rx.try_recv() {
        let value = serde_json::to_value(&message).unwrap();
        let seq = value["seq"].as_u64().expect("every message is numbered");
        assert!(seq > last_seq, "sequence numbers only go up: {value}");
        last_seq = seq;
        if value["type"] == "tick" && !saw_tick {
            assert_keys(
                "TickMessage",
                &value,
                &[
                    "seq",
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
        }
    }
    assert!(saw_tick, "the engine step published no tick to check");

    // `status` flattens the symbol's state next to the tag.
    let status = serde_json::to_value(StreamMessage::Status(
        app.status("ACME").await.expect("ACME exists"),
    ))
    .unwrap();
    assert_keys(
        "StatusMessage",
        &status,
        &[
            "type",
            "symbol",
            "ts_ms",
            "market_open",
            "halted",
            "tradable",
            "halt",
            "next_open_ms",
            "next_close_ms",
            "band_cents",
            "move_pct",
            "limit_pct",
        ],
    );
    assert_eq!(status["type"], "status");

    // `stop_triggered` carries the trigger and whatever it became.
    let triggered = serde_json::to_value(StreamMessage::StopTriggered {
        trader_id: 1,
        stop: fehu_economy::trading::StopOrder {
            stop_id: 1,
            trader_id: 1,
            symbol: "ACME",
            side: fehu::Side::Buy,
            qty: 1,
            stop_price_cents: 100,
            limit_price_cents: None,
            tif: fehu::TimeInForce::Gtc,
            client_order_id: None,
            created_at_ms: NOW_MS,
        },
        price_cents: 101,
        order: None,
        refused: Some("insufficient funds".into()),
    })
    .unwrap();
    assert_keys(
        "StopTriggeredMessage",
        &triggered,
        &[
            "type",
            "trader_id",
            "stop",
            "price_cents",
            "order",
            "refused",
        ],
    );
    assert_eq!(triggered["type"], "stop_triggered");

    // `order_expired` carries the record of the order the clock took away.
    let owner = post(&app, "/api/traders", json!({ "name": "expiry" })).await;
    let owner_key = api_key_of(&owner);
    let owner_id = owner["id"].as_u64().unwrap();
    let bid = get(&app, "/api/symbols/ACME/book").await["bid_cents"]
        .as_i64()
        .unwrap();
    post_as(
        &app,
        Some(owner_key.as_str()),
        "/api/symbols/ACME/orders",
        json!({ "trader_id": owner_id, "side": "buy", "qty": 1, "type": "limit",
                "price_cents": bid / 2 }),
    )
    .await;
    let record = app
        .market
        .call(move |m| m.orders_of(fehu::TraderId(owner_id)).next().cloned())
        .await
        .unwrap()
        .expect("the order is in the log");
    let expired = serde_json::to_value(StreamMessage::OrderExpired {
        trader_id: owner_id,
        order: record,
    })
    .unwrap();
    assert_keys(
        "OrderExpiredMessage",
        &expired,
        &["type", "trader_id", "order"],
    );
    assert_eq!(expired["type"], "order_expired");

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

    // `listed` carries the new symbol's first quote; `delisted` flattens what
    // the delisting undid next to the tag.
    let listed = serde_json::to_value(StreamMessage::Listed {
        quote: app.quotes().await[0].clone(),
    })
    .unwrap();
    assert_keys("ListedMessage", &listed, &["type", "quote"]);
    assert_eq!(listed["type"], "listed");

    let (_, delisted) = call_as(
        &app,
        None,
        Request::post("/api/symbols/PXCO/delist")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(json!({}).to_string()))
            .unwrap(),
    )
    .await;
    let mut saw_delisted = false;
    while let Ok(message) = rx.try_recv() {
        let value = serde_json::to_value(&message).unwrap();
        if value["type"] == "delisted" {
            assert_keys(
                "DelistedMessage",
                &value,
                &[
                    "seq",
                    "type",
                    "symbol",
                    "cents_per_share",
                    "last_price_cents",
                    "orders_cancelled",
                    "stops_cancelled",
                    "shares_bought_out",
                    "accounts_paid",
                    "total_cents",
                ],
            );
            assert_eq!(value["symbol"], "PXCO");
            saw_delisted = true;
        }
    }
    assert!(saw_delisted, "the delisting was not published: {delisted}");
    assert_keys("DelistResponse", &delisted, &["delisting", "event"]);
    assert_keys(
        "Delisting",
        &delisted["delisting"],
        &[
            "symbol",
            "cents_per_share",
            "last_price_cents",
            "orders_cancelled",
            "stops_cancelled",
            "shares_bought_out",
            "accounts_paid",
            "total_cents",
        ],
    );
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

#[tokio::test]
async fn reconciliation_contract() {
    let report = get(&test_app(), "/api/reconcile").await;
    assert_keys(
        "Reconciliation",
        &report,
        &[
            "valid",
            "accounts_checked",
            "traders_checked",
            "symbols_checked",
            "resting_orders_checked",
            "wallets_checked",
            "minted_cents",
            "burned_cents",
            "outstanding_cents",
            "circulating_cents",
            "synthetic_debt_cents",
            "jobs_running",
            "budgets_checked",
            "players_checked",
            "issues",
        ],
    );
    assert_eq!(report["valid"], true);
    assert!(report["issues"].as_array().unwrap().is_empty());
    assert_eq!(
        report["circulating_cents"], report["outstanding_cents"],
        "what the wallets hold is what was minted less what was burned"
    );
}

#[tokio::test]
async fn outbox_contract() {
    let app = test_app();
    let page_keys = [
        "events", "next", "cursor", "oldest", "latest", "pending", "dropped", "gap", "cap",
    ];
    assert_keys("OutboxPage", &get(&app, "/api/outbox").await, &page_keys);

    post(
        &app,
        "/api/game/events",
        json!({ "kind": "scandal", "magnitude": 0.4, "symbol": "ACME", "source": "quest-1" }),
    )
    .await;
    let page = get(&app, "/api/v1/economy/outbox?after=0").await;
    assert_keys("OutboxPage", &page, &page_keys);
    let entry = first("OutboxPage", &page, "events");
    assert_keys(
        "OutboxEntry",
        entry,
        &["seq", "command_seq", "kind", "at_ms", "wall_ms", "event"],
    );
    assert_eq!(entry["kind"], "event");
    assert_eq!(
        entry["event"]["type"], "event",
        "the payload is the stream message, tag and all"
    );

    let cursor = post(&app, "/api/outbox/ack", json!({ "through": 1 })).await;
    assert_keys(
        "OutboxCursor",
        &cursor,
        &["cursor", "oldest", "latest", "pending", "dropped", "cap"],
    );
    assert_eq!(cursor["cursor"], 1);
}

#[tokio::test]
async fn supply_contract() {
    let supply = get(&test_app(), "/api/supply").await;
    assert_keys(
        "SupplyDto",
        &supply,
        &[
            "minted_cents",
            "burned_cents",
            "outstanding_cents",
            "circulating_cents",
            "balanced",
            "treasury_cents",
            "venue_cents",
            "issuer_cents",
            "player_cents",
            "npc_cents",
            "budget_cents",
            "synthetic_debt_cents",
            "wallets",
        ],
    );
    assert_eq!(supply["balanced"], true);
    assert_eq!(supply["burned_cents"], 0);
    assert_eq!(supply["circulating_cents"], supply["outstanding_cents"]);
}

#[tokio::test]
async fn health_contract() {
    let health = get(&test_app(), "/api/health").await;
    assert_keys(
        "Health",
        &health,
        &[
            "status",
            "uptime_secs",
            "sim_now_ms",
            "time_scale",
            "symbols",
            "events_logged",
            "ticks_total",
            "trades_total",
            "users",
            "accounts",
            "traders",
            "cash_cents",
            "resting_orders",
            "stops_held",
            "orders_placed",
            "orders_refused",
            "fills_booked",
            "settlement_failures",
            "stream_messages",
            "stream_subscribers",
            "requests_in_flight",
            "max_in_flight",
            "max_streams",
            "tracked_clients",
            "metrics",
        ],
    );
    assert_keys(
        "MetricsDto",
        &health["metrics"],
        &[
            "requests",
            "requests_failed",
            "requests_limited",
            "requests_shed",
            "engine_step",
        ],
    );
    assert_keys(
        "TimingDto",
        &health["metrics"]["requests"],
        &["count", "micros_last", "micros_max", "micros_mean"],
    );
}

#[tokio::test]
async fn overview_contract() {
    let app = test_app();
    // A world with something in every list the overview carries.
    post(
        &app,
        "/api/traders",
        json!({ "name": "wren", "cash_cents": 50_000 }),
    )
    .await;
    let budget = post(
        &app,
        "/api/budgets",
        json!({ "name": "quests", "cash_cents": 100_000 }),
    )
    .await;
    post(
        &app,
        "/api/rewards/rules",
        json!({ "id": "boss", "budget": budget["wallet"], "amount_cents": 5_000 }),
    )
    .await;
    post(
        &app,
        "/api/npcs",
        json!({ "symbol": "ACME", "name": "acme desk", "cash_cents": 1_000_000 }),
    )
    .await;

    let overview = get(&app, "/api/overview").await;
    assert_keys(
        "OverviewDto",
        &overview,
        &[
            "at_ms",
            "supply",
            "flows",
            "wallets",
            "budgets",
            "rules",
            "npcs",
            "effects",
            "modifiers",
            "jobs",
            "people",
            "outbox",
            "journal_seq",
        ],
    );
    assert_keys(
        "FlowDto",
        first("OverviewDto", &overview, "flows"),
        &["reason", "count", "cents"],
    );
    assert_keys(
        "WalletRow",
        first("OverviewDto", &overview, "wallets"),
        &[
            "wallet",
            "kind",
            "status",
            "balance_cents",
            "reserved_cents",
            "available_cents",
            "account_id",
            "owner",
        ],
    );
    assert_keys(
        "JobsSummary",
        &overview["jobs"],
        &["held", "running", "done", "cancelled", "next_due_ms"],
    );
    assert_keys(
        "PeopleSummary",
        &overview["people"],
        &[
            "users", "accounts", "traders", "players", "frozen", "closed",
        ],
    );
    assert_keys(
        "OutboxCursor",
        &overview["outbox"],
        &["cursor", "oldest", "latest", "pending", "dropped", "cap"],
    );
    // The nested lists are the same shapes their own endpoints serve, which
    // the tests above pin; here it is enough that they arrived.
    assert!(!overview["budgets"].as_array().unwrap().is_empty());
    assert!(!overview["rules"].as_array().unwrap().is_empty());
    assert!(!overview["npcs"].as_array().unwrap().is_empty());
    assert!(!overview["effects"].as_array().unwrap().is_empty());
    assert_eq!(
        overview["flows"].as_array().unwrap().len(),
        16,
        "every reason has a row, moved or not"
    );
}

#[tokio::test]
async fn goods_contract() {
    let app = test_app();
    post(
        &app,
        "/api/symbols",
        json!({
            "symbol": "ORE", "kind": "good", "unit": "kg",
            "name": "Iron Ore", "start_price_cents": 250,
        }),
    )
    .await;
    let item = post(
        &app,
        "/api/catalog",
        json!({ "symbol": "ORE", "price_cents": 250, "available": 500 }),
    )
    .await;
    let item_keys = ["symbol", "price_cents", "available", "issued", "note"];
    assert_keys("CatalogItem", &item, &item_keys);

    let catalog = get(&app, "/api/catalog").await;
    assert_keys("CatalogResponse", &catalog, &["items"]);
    assert_keys(
        "CatalogItem",
        first("CatalogResponse", &catalog, "items"),
        &item_keys,
    );

    let player = post(
        &app,
        "/api/traders",
        json!({ "name": "wanda", "cash_cents": 1_000_000 }),
    )
    .await;
    let key = api_key_of(&player);
    let id = player["id"].as_u64().unwrap();
    let receipt = post_as(
        &app,
        Some(&key),
        &format!("/api/traders/{id}/purchases"),
        json!({ "symbol": "ORE", "qty": 10 }),
    )
    .await;
    assert_keys(
        "PurchaseReceipt",
        &receipt,
        &[
            "trader_id",
            "symbol",
            "qty",
            "unit_price_cents",
            "total_cents",
            "tx_id",
            "position_qty",
            "units_outstanding",
            "available",
        ],
    );
    let consumed = post_as(
        &app,
        Some(&key),
        &format!("/api/traders/{id}/consume"),
        json!({ "symbol": "ORE", "qty": 4 }),
    )
    .await;
    assert_keys(
        "ConsumeReceipt",
        &consumed,
        &[
            "trader_id",
            "symbol",
            "qty",
            "position_qty",
            "units_outstanding",
        ],
    );

    // The purchase credited ORE's issuer; sweeping brings it home.
    let overview = get(&app, "/api/overview").await;
    let issuer = overview["wallets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["kind"] == "issuer" && w["owner"] == "ORE")
        .unwrap_or_else(|| panic!("ORE has an issuer wallet: {overview}"));
    let swept = post(
        &app,
        &format!("/api/wallets/{}/sweep", issuer["wallet"]),
        json!({}),
    )
    .await;
    assert_keys(
        "SweepResponse",
        &swept,
        &["wallet", "swept_cents", "treasury_cents"],
    );
    assert_keys(
        "WalletDto",
        &swept["wallet"],
        &[
            "wallet",
            "kind",
            "status",
            "balance_cents",
            "reserved_cents",
            "available_cents",
            "account_id",
        ],
    );
}

#[tokio::test]
async fn npc_contract() {
    let app = test_app();
    let npc = post(
        &app,
        "/api/npcs",
        json!({ "symbol": "ACME", "name": "Acme Merchant", "cash_cents": 1_000_000, "inventory": 500 }),
    )
    .await;
    let npc_keys = [
        "trader_id",
        "user_id",
        "account_id",
        "symbol",
        "name",
        "policy",
        "active",
        "quoted_size",
        "cash_cents",
        "inventory",
        "reserved",
    ];
    assert_keys("NpcDto", &npc, &npc_keys);
    assert_keys(
        "Policy",
        &npc["policy"],
        &[
            "half_spread_bps",
            "levels",
            "level_step_bps",
            "size",
            "requote_bps",
        ],
    );

    let npcs = get(&app, "/api/npcs").await;
    assert_keys("NpcsResponse", &npcs, &["npcs"]);
    assert_keys("NpcDto", first("NpcsResponse", &npcs, "npcs"), &npc_keys);

    let off = post_as(
        &app,
        None,
        &format!("/api/npcs/{}/active", npc["trader_id"]),
        json!({ "active": false }),
    )
    .await;
    assert_keys("NpcDto", &off, &npc_keys);
    assert_eq!(off["active"], false);
}

/// The game backend's own credentials, and the players it provisions.
#[tokio::test]
async fn service_and_player_shapes() {
    let app = test_app();

    let service_keys = [
        "id",
        "name",
        "scopes",
        "revoked",
        "created_ms",
        "revoked_ms",
        "api_key",
    ];
    let service = post(
        &app,
        "/api/v1/economy/admin/services",
        json!({ "name": "quests", "scopes": ["provision", "reward"] }),
    )
    .await;
    assert_keys("ServiceDto", &service, &service_keys);
    assert_eq!(
        service["scopes"],
        json!(["provision", "reward"]),
        "a scope set is a list of names, in the order they are declared"
    );
    assert!(
        service["api_key"].is_string(),
        "the response that issues a service shows its key once"
    );

    let services = get(&app, "/api/v1/economy/admin/services").await;
    assert_keys("ServicesResponse", &services, &["services"]);
    let listed = first("ServicesResponse", &services, "services");
    assert_keys("ServiceDto", listed, &service_keys);
    assert_eq!(
        listed["api_key"],
        Value::Null,
        "and never again after that one"
    );

    let player_keys = [
        "external_id",
        "user_id",
        "account_id",
        "trader_id",
        "wallet_id",
        "created_at_ms",
        "created",
        "api_key",
    ];
    let player = post(
        &app,
        "/api/v1/economy/players",
        json!({ "external_id": "steam:1", "name": "Ada" }),
    )
    .await;
    assert_keys("PlayerDto", &player, &player_keys);
    assert_eq!(player["created"], true);

    let again = post(
        &app,
        "/api/v1/economy/players",
        json!({ "external_id": "steam:1", "name": "Ada" }),
    )
    .await;
    assert_keys("PlayerDto", &again, &player_keys);
    assert_eq!(again["created"], false);
    assert_eq!(again["api_key"], Value::Null);

    let players = get(&app, "/api/v1/economy/players").await;
    assert_keys("PlayersResponse", &players, &["players"]);
    assert_keys(
        "PlayerDto",
        first("PlayersResponse", &players, "players"),
        &player_keys,
    );
}
