//! Saving the market and starting it up again.
//!
//! What matters here is not that the JSON round-trips but that the server
//! comes back the same: the same prices and books, the same money, the same
//! positions and orders, and keys that still open the accounts they opened
//! before.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use fehu_webapp::market::{App, Options};
use fehu_webapp::{engine, router, save};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tempdir_lite::TempDir;
use tower::util::ServiceExt;

/// 2023-11-14T22:13:20Z, as in the other suites.
const NOW_MS: i64 = 1_700_000_000_000;

/// A small, quick market. `now_ms` is fixed so runs are comparable.
fn options(state_file: Option<std::path::PathBuf>) -> Options {
    Options {
        history_days: 2,
        warmup_hours: 1,
        now_ms: Some(NOW_MS),
        state_file,
        // Persistence, not timing: the limiter has its own tests.
        rate_per_sec: 0.0,
        ..Options::default()
    }
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

/// Sign a player up and trade a little, so there is something to lose.
async fn busy_market(app: &Arc<App>) -> (u64, String) {
    let (status, trader) = post(app, None, "/api/traders", json!({ "name": "saver" })).await;
    assert_eq!(status, StatusCode::CREATED, "{trader}");
    let id = trader["id"].as_u64().unwrap();
    let key = trader["api_key"].as_str().unwrap().to_owned();

    let (status, body) = post(
        app,
        Some(&key),
        "/api/symbols/ACME/orders",
        json!({ "trader_id": id, "side": "buy", "qty": 120, "type": "market", "client_order_id": "keep-me" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let bid = get(app, None, "/api/symbols/ACME/book").await.1["bid_cents"]
        .as_i64()
        .unwrap();
    let (status, body) = post(
        app,
        Some(&key),
        "/api/symbols/ACME/orders",
        json!({ "trader_id": id, "side": "buy", "qty": 30, "type": "limit", "price_cents": bid / 2 }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["status"], "resting");

    post(
        app,
        Some(&key),
        "/api/game/events",
        json!({ "kind": "hype", "symbol": "ACME" }),
    )
    .await;
    engine::advance_to(app, app.clock.now() + std::time::Duration::from_secs(30));
    (id, key)
}

#[tokio::test]
async fn a_saved_market_comes_back_whole() {
    let dir = TempDir::new("fehu-save");
    let path = dir.path().join("state.json");
    let before = App::new(options(Some(path.clone())));
    let (id, key) = busy_market(&before).await;

    let (_, portfolio) = get(&before, Some(&key), &format!("/api/traders/{id}")).await;
    let (_, ledger) = get(
        &before,
        Some(&key),
        &format!("/api/accounts/{}/ledger", portfolio["account_id"]),
    )
    .await;
    let (_, orders) = get(&before, Some(&key), &format!("/api/traders/{id}/orders")).await;
    let (_, book) = get(&before, None, "/api/symbols/ACME/book?depth=8").await;
    let (_, bars) = get(
        &before,
        None,
        "/api/symbols/ACME/bars?interval=M1&limit=200",
    )
    .await;
    let (_, events) = get(&before, None, "/api/events").await;
    let (_, shares) = get(&before, Some(&key), "/api/symbols/ACME/shares").await;

    save::write(&before, &path).expect("state written");
    assert!(path.exists(), "the save file is where it was asked for");
    let saved_text = std::fs::read_to_string(&path).unwrap();
    assert!(
        !saved_text.contains(&key),
        "saves must not contain bearer credentials"
    );

    // A brand-new process would do exactly this.
    let after = App::restore(options(Some(path.clone())), save::read(&path).unwrap());

    let (status, restored) = get(&after, Some(&key), &format!("/api/traders/{id}")).await;
    assert_eq!(status, StatusCode::OK, "the key still opens the account");
    assert_eq!(restored, portfolio, "cash, positions, orders and fills");
    let (_, restored_ledger) = get(
        &after,
        Some(&key),
        &format!("/api/accounts/{}/ledger", portfolio["account_id"]),
    )
    .await;
    assert_eq!(restored_ledger, ledger, "every movement of money");
    let (_, restored_orders) = get(&after, Some(&key), &format!("/api/traders/{id}/orders")).await;
    assert_eq!(restored_orders, orders, "the order log");
    let (_, restored_book) = get(&after, None, "/api/symbols/ACME/book?depth=8").await;
    assert_eq!(restored_book, book, "the book, resting order included");
    // `sim_now_ms` is the clock, which has of course moved on; the bars and
    // the events themselves must not have.
    let (_, restored_bars) =
        get(&after, None, "/api/symbols/ACME/bars?interval=M1&limit=200").await;
    assert_eq!(restored_bars["bars"], bars["bars"], "the bar history");
    let (_, restored_events) = get(&after, None, "/api/events").await;
    assert_eq!(restored_events["events"], events["events"], "the event log");
    let (_, restored_shares) = get(&after, Some(&key), "/api/symbols/ACME/shares").await;
    assert_eq!(restored_shares, shares, "who holds what");

    // And it carries on: the same order id is not handed out twice, the
    // client id still replays, and the market keeps ticking.
    let (status, replay) = post(
        &after,
        Some(&key),
        "/api/symbols/ACME/orders",
        json!({ "trader_id": id, "side": "buy", "qty": 120, "type": "market", "client_order_id": "keep-me" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "a retry across a restart: {replay}");
    assert_eq!(replay["order_id"], orders[1]["order_id"]);
    assert!(!replay["trades"].as_array().unwrap().is_empty(), "{replay}");

    let ticks = engine::advance_to(
        &after,
        after.clock.now() + std::time::Duration::from_secs(5),
    );
    assert!(ticks > 0, "the market runs on");
    let (status, body) = post(
        &after,
        Some(&key),
        "/api/symbols/ACME/orders",
        json!({ "trader_id": id, "side": "sell", "qty": 120, "type": "market" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert!(
        body["order_id"].as_u64().unwrap() > orders[0]["order_id"].as_u64().unwrap(),
        "order ids carry on rather than repeat: {body}"
    );
}

#[tokio::test]
async fn a_second_player_can_still_sign_up_after_a_restore() {
    let dir = TempDir::new("fehu-save-ids");
    let path = dir.path().join("state.json");
    let before = App::new(options(Some(path.clone())));
    let (first, first_key) = busy_market(&before).await;
    save::write(&before, &path).unwrap();

    let after = App::restore(options(Some(path.clone())), save::read(&path).unwrap());
    let (status, second) = post(&after, None, "/api/traders", json!({ "name": "newcomer" })).await;
    assert_eq!(status, StatusCode::CREATED, "{second}");
    assert!(
        second["id"].as_u64().unwrap() > first,
        "ids continue from the save: {second}"
    );
    assert_ne!(second["user_id"].as_u64().unwrap(), 0);
    let second_key = second["api_key"].as_str().unwrap();
    assert_ne!(second_key, first_key, "and the new key is its own");

    // Each key opens its own account and no other.
    let (status, _) = get(&after, Some(second_key), &format!("/api/traders/{first}")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = get(
        &after,
        Some(&first_key),
        &format!("/api/traders/{}", second["id"]),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_halt_survives_a_restart() {
    let dir = TempDir::new("fehu-save-halt");
    let path = dir.path().join("state.json");
    let before = App::new(options(Some(path.clone())));
    busy_market(&before).await;

    let (status, halted) = post(&before, None, "/api/symbols/ACME/halt", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{halted}");
    save::write(&before, &path).unwrap();

    let after = App::restore(options(Some(path.clone())), save::read(&path).unwrap());
    let (_, status) = get(&after, None, "/api/symbols/ACME/status").await;
    assert_eq!(status["halted"], true, "still stopped: {status}");
    assert_eq!(status["halt"]["reason"], "manual");
    assert_eq!(
        status["halt"], halted["halt"],
        "the same halt, not a new one"
    );
    assert_eq!(status["band_cents"], halted["band_cents"]);
    // The other symbols came back tradable.
    let (_, other) = get(&after, None, "/api/symbols/HLIO/status").await;
    assert_eq!(other["tradable"], true, "{other}");
}

#[tokio::test]
async fn held_stops_survive_a_restart_and_still_fire() {
    let dir = TempDir::new("fehu-save-stops");
    let path = dir.path().join("state.json");
    // Halts hold stops rather than firing them, which is its own test; here
    // the move that reaches the trigger must not also stop the symbol.
    let options = |file| Options {
        price_limit_pct: 0.0,
        ..options(file)
    };
    let before = App::new(options(Some(path.clone())));
    let (id, key) = busy_market(&before).await;
    let price = get(&before, None, "/api/symbols/ACME/book").await.1["reference_cents"]
        .as_i64()
        .unwrap();
    let trigger = price * 105 / 100;
    let (status, stop) = post(
        &before,
        Some(&key),
        "/api/symbols/ACME/stops",
        json!({ "trader_id": id, "side": "buy", "qty": 5, "stop_price_cents": trigger,
                "limit_price_cents": trigger * 2, "client_order_id": "the-stop" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{stop}");
    save::write(&before, &path).unwrap();

    let after = App::restore(options(Some(path.clone())), save::read(&path).unwrap());
    let (_, stops) = get(&after, Some(&key), &format!("/api/traders/{id}/stops")).await;
    assert_eq!(stops[0], stop, "the same trigger came back, not a new one");

    // The id counter came back with it, so the next stop is not the old one.
    let (status, second) = post(
        &after,
        Some(&key),
        "/api/symbols/ACME/stops",
        json!({ "trader_id": id, "side": "buy", "qty": 1, "stop_price_cents": trigger * 3 }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{second}");
    assert_ne!(second["stop_id"], stop["stop_id"]);

    // And the restored trigger still fires, on the market that came back.
    post(
        &after,
        Some(&key),
        "/api/symbols/ACME/events",
        json!({ "type": "jump", "pct": 0.10, "source": "restart" }),
    )
    .await;
    engine::advance_to(
        &after,
        after.clock.now() + std::time::Duration::from_secs(10),
    );
    let (_, portfolio) = get(&after, Some(&key), &format!("/api/traders/{id}")).await;
    assert_eq!(
        portfolio["stops"].as_array().unwrap().len(),
        1,
        "the reached trigger fired, the far one did not: {portfolio}"
    );
    assert!(
        portfolio["fills"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["qty"] == 5),
        "the stop it fired should have traded: {portfolio}"
    );
}

#[tokio::test]
async fn an_iceberg_comes_back_with_what_it_was_hiding() {
    let dir = TempDir::new("fehu-save-iceberg");
    let path = dir.path().join("state.json");
    let before = App::new(options(Some(path.clone())));
    let (id, key) = busy_market(&before).await;
    let (bid, ask) = {
        let book = get(&before, None, "/api/symbols/ACME/book").await.1;
        (
            book["bid_cents"].as_i64().unwrap(),
            book["ask_cents"].as_i64().unwrap(),
        )
    };
    let price = bid + 1;
    assert!(price < ask, "the spread has room");
    let (status, body) = post(
        &before,
        Some(&key),
        "/api/symbols/ACME/orders",
        json!({ "trader_id": id, "side": "buy", "qty": 200, "type": "limit",
                "price_cents": price, "display_qty": 25 }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let order_id = body["order_id"].as_u64().unwrap();
    let reserved = get(&before, Some(&key), &format!("/api/traders/{id}"))
        .await
        .1["reserved_cents"]
        .as_i64()
        .unwrap();
    save::write(&before, &path).unwrap();

    // A file whose books do not add up is refused, so getting this far is
    // already most of the check.
    let after = App::restore(options(Some(path.clone())), save::read(&path).unwrap());
    let (_, p) = get(&after, Some(&key), &format!("/api/traders/{id}")).await;
    let open = p["open_orders"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["order_id"] == order_id)
        .unwrap_or_else(|| panic!("the iceberg came back: {p}"));
    assert_eq!(open["remaining"], 200, "hidden size and all");
    assert_eq!(open["shown_qty"], 25);
    assert_eq!(open["display_qty"], 25);
    assert_eq!(
        p["reserved_cents"].as_i64().unwrap(),
        reserved,
        "and it still reserves the whole thing"
    );

    // The book that came back shows the slice, and still refreshes.
    let book = get(&after, None, "/api/symbols/ACME/book").await.1;
    let top = book["bids"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["price_cents"] == price)
        .unwrap_or_else(|| panic!("no level at {price}: {book}"));
    assert_eq!(top["qty"], 25, "only the slice is on show: {book}");
    let (status, second) = post(
        &after,
        None,
        "/api/traders",
        json!({ "name": "restored-seller" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{second}");
    let other = second["id"].as_u64().unwrap();
    let other_key = second["api_key"].as_str().unwrap().to_owned();
    post(
        &after,
        Some(&other_key),
        "/api/symbols/ACME/orders",
        json!({ "trader_id": other, "side": "buy", "qty": 50, "type": "market" }),
    )
    .await;
    let (status, hit) = post(
        &after,
        Some(&other_key),
        "/api/symbols/ACME/orders",
        json!({ "trader_id": other, "side": "sell", "qty": 25, "type": "limit",
                "price_cents": price, "tif": "ioc" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{hit}");
    assert_eq!(hit["filled"], 25);
    let book = get(&after, None, "/api/symbols/ACME/book").await.1;
    let top = book["bids"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["price_cents"] == price)
        .unwrap_or_else(|| panic!("the next slice should be showing: {book}"));
    assert_eq!(top["qty"], 25, "a restored iceberg still refreshes: {book}");
    assert!(after.market().reconcile().valid);
}

#[tokio::test]
async fn the_symbols_a_market_comes_back_with_are_the_ones_it_was_saved_with() {
    let dir = TempDir::new("fehu-save-listings");
    let path = dir.path().join("state.json");
    let before = App::new(options(Some(path.clone())));

    // A symbol the build has never heard of, and one of the build's own
    // taken away. Neither is what `seeded_symbols` says the market is.
    let (status, listed) = post(
        &before,
        None,
        "/api/symbols",
        json!({
            "symbol": "WDGT",
            "name": "Widget Corp",
            "sector": "Industrials",
            "shares_outstanding": 1_000_000,
            "start_price_cents": 5_000,
            "seed": 99,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{listed}");
    let (status, _) = post(&before, None, "/api/symbols/HLIO/delist", json!({})).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    engine::advance_to(
        &before,
        before.clock.now() + std::time::Duration::from_secs(5),
    );

    let (_, symbols) = get(&before, None, "/api/symbols").await;
    let (_, detail) = get(&before, None, "/api/symbols/WDGT").await;
    save::write(&before, &path).expect("state written");

    let after = App::restore(options(Some(path.clone())), save::read(&path).unwrap());
    let (_, restored) = get(&after, None, "/api/symbols").await;
    let tickers: Vec<&str> = restored["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["symbol"].as_str().unwrap())
        .collect();
    assert_eq!(
        tickers,
        ["ACME", "NBLA", "PXCO", "WDGT"],
        "the file is the symbol table, not the build"
    );
    assert_eq!(restored["symbols"], symbols["symbols"], "the same quotes");
    let (status, restored_detail) = get(&after, None, "/api/symbols/WDGT").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        restored_detail["info"], detail["info"],
        "name, sector, share count and seed all came back"
    );
    let (status, _) = get(&after, None, "/api/symbols/HLIO").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "a delisting stays done");
    assert!(after.market().reconcile().valid);

    // And the restored market goes on listing and delisting.
    let (status, body) = post(&after, None, "/api/symbols/WDGT/delist", json!({})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
}

#[tokio::test]
async fn a_version_4_save_takes_its_symbols_from_the_build() {
    let dir = TempDir::new("fehu-save-v4");
    let path = dir.path().join("state.json");
    let app = App::new(options(Some(path.clone())));
    busy_market(&app).await;
    save::write(&app, &path).unwrap();

    // Version 4 knew nothing of listings: it named its symbols and left the
    // metadata to the build, which is where the migration has to find it.
    let mut value: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    value["version"] = json!(4);
    for symbol in value["symbols"].as_array_mut().unwrap() {
        symbol.as_object_mut().unwrap().remove("info");
    }
    value["market"]
        .as_object_mut()
        .unwrap()
        .remove("next_order_id");
    std::fs::write(&path, value.to_string()).unwrap();

    let restored = save::read(&path).expect("a version-4 file still loads");
    assert_eq!(restored.version, save::STATE_VERSION);
    let after = App::restore(options(None), restored);
    let (_, symbols) = get(&after, None, "/api/symbols").await;
    let tickers: Vec<&str> = symbols["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["symbol"].as_str().unwrap())
        .collect();
    assert_eq!(tickers, ["ACME", "NBLA", "HLIO", "PXCO"]);
    assert_eq!(symbols["symbols"][0]["name"], "Acme Industrial");
    assert!(after.market().reconcile().valid);
}

#[tokio::test]
async fn a_save_this_build_cannot_use_is_refused() {
    let dir = TempDir::new("fehu-save-bad");
    let path = dir.path().join("state.json");
    let app = App::new(options(Some(path.clone())));
    busy_market(&app).await;
    save::write(&app, &path).unwrap();

    let good = std::fs::read_to_string(&path).unwrap();
    let mut value: Value = serde_json::from_str(&good).unwrap();

    // A file from another version of the format.
    value["version"] = json!(save::STATE_VERSION + 1);
    std::fs::write(&path, value.to_string()).unwrap();
    assert!(
        matches!(save::read(&path), Err(save::SaveError::Version { .. })),
        "a version this build does not read is refused"
    );

    // A file whose symbol table does not agree with itself: the entry is
    // filed under one ticker and carries the listing of another. Which of the
    // two the market would have is not a question worth guessing at.
    let mut value: Value = serde_json::from_str(&good).unwrap();
    value["symbols"][0]["symbol"] = json!("WHAT");
    std::fs::write(&path, value.to_string()).unwrap();
    assert!(
        matches!(save::read(&path), Err(save::SaveError::Invalid(_))),
        "a symbol filed under the wrong listing is refused"
    );

    // The same ticker listed twice: the second book would be unreachable
    // behind the first.
    let mut value: Value = serde_json::from_str(&good).unwrap();
    let duplicate = value["symbols"][0].clone();
    value["symbols"].as_array_mut().unwrap().push(duplicate);
    std::fs::write(&path, value.to_string()).unwrap();
    assert!(
        matches!(save::read(&path), Err(save::SaveError::Invalid(_))),
        "a ticker listed twice is refused"
    );

    // A version-4 file naming a symbol this build does not seed. Its
    // metadata lived in the build, and this build does not have it.
    let mut value: Value = serde_json::from_str(&good).unwrap();
    value["version"] = json!(4);
    for symbol in value["symbols"].as_array_mut().unwrap() {
        symbol.as_object_mut().unwrap().remove("info");
    }
    value["symbols"][0]["symbol"] = json!("WHAT");
    std::fs::write(&path, value.to_string()).unwrap();
    assert!(
        matches!(save::read(&path), Err(save::SaveError::Symbols { .. })),
        "a version-4 file with no metadata for a symbol is refused"
    );

    // Not JSON at all.
    std::fs::write(&path, "{oh no").unwrap();
    assert!(matches!(save::read(&path), Err(save::SaveError::Format(_))));

    // Missing entirely.
    assert!(matches!(
        save::read(&dir.path().join("nope.json")),
        Err(save::SaveError::Io(_))
    ));

    // The good file still loads, which is what makes the checks above mean
    // something.
    std::fs::write(&path, &good).unwrap();
    assert!(save::read(&path).is_ok());
}

#[tokio::test]
async fn a_failed_write_leaves_the_previous_save_alone() {
    let dir = TempDir::new("fehu-save-atomic");
    let path = dir.path().join("state.json");
    let app = App::new(options(Some(path.clone())));
    busy_market(&app).await;
    save::write(&app, &path).unwrap();
    let first = std::fs::read_to_string(&path).unwrap();

    // A directory where the temporary file wants to be: the write fails and
    // the previous save is untouched.
    std::fs::create_dir(dir.path().join("state.json.tmp")).unwrap();
    assert!(save::write(&app, &path).is_err(), "the write cannot finish");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        first,
        "the previous save survived"
    );
}

/// A throwaway directory that cleans up after itself. `tempfile` is not a
/// dependency of this workspace and one test directory does not justify one.
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

#[tokio::test]
async fn version_two_keys_are_migrated_without_changing_player_credentials() {
    let dir = TempDir::new("fehu-save-key-migration");
    let path = dir.path().join("state.json");
    let app = App::new(options(None));
    let (id, key) = busy_market(&app).await;
    let mut legacy = app.save();
    legacy.version = 2;
    let user = app.market().traders[&fehu::TraderId(id)].user_id.0;
    legacy.market.api_keys = vec![(key.clone(), user)];
    std::fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
    let migrated = save::read(&path).unwrap();
    assert_eq!(migrated.version, save::STATE_VERSION);
    assert!(!serde_json::to_string(&migrated).unwrap().contains(&key));
    let digest = migrated.market.api_keys[0].0.clone();
    let restored = App::restore(options(None), migrated);
    assert_eq!(
        get(&restored, Some(&key), &format!("/api/traders/{id}"))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        get(&restored, Some(&digest), &format!("/api/traders/{id}"))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    save::write(&restored, &path).unwrap();
    assert!(!std::fs::read_to_string(&path).unwrap().contains(&key));
    let again = App::restore(options(None), save::read(&path).unwrap());
    assert_eq!(
        get(&again, Some(&key), &format!("/api/traders/{id}"))
            .await
            .0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn inconsistent_accounting_and_identity_saves_are_refused() {
    let dir = TempDir::new("fehu-save-invariants");
    let path = dir.path().join("state.json");
    let app = App::new(options(None));
    busy_market(&app).await;
    let good = serde_json::to_value(app.save()).unwrap();
    let mut cases = Vec::new();
    let mut bad = good.clone();
    bad["symbols"][0]["exchange"]["book"]["index"] = json!({});
    cases.push(("book index disagrees with resting orders", bad));
    let mut bad = good.clone();
    let duplicate = bad["market"]["users"][0].clone();
    bad["market"]["users"]
        .as_array_mut()
        .unwrap()
        .push(duplicate);
    cases.push(("duplicate user", bad));
    let mut bad = good.clone();
    bad["market"]["next_trader_id"] = json!(1);
    cases.push(("counter overlaps live trader", bad));
    let mut bad = good.clone();
    bad["market"]["accounts"][0]["balance_cents"] = json!(0);
    cases.push(("ledger does not match cash", bad));
    let mut bad = good.clone();
    bad["market"]["accounts"][0]["reserved_cents"] = json!(0);
    cases.push(("reservation does not match book", bad));
    let mut bad = good.clone();
    bad["market"]["traders"][0]["account_id"] = json!(999999);
    cases.push(("orphan trader", bad));
    let mut bad = good.clone();
    bad["market"]["api_keys"][0][1] = json!(999999);
    cases.push(("orphan key", bad));
    let mut bad = good.clone();
    bad["market"]["api_keys"][0][0] = json!("fehu_plaintext");
    cases.push(("plaintext in current format", bad));
    for (name, value) in cases {
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(
            matches!(save::read(&path), Err(save::SaveError::Invalid(_))),
            "{name}"
        );
    }
    std::fs::write(&path, serde_json::to_vec(&good).unwrap()).unwrap();
    assert!(save::read(&path).is_ok());
}
