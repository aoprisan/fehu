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

    // A file listing symbols this build does not have.
    let mut value: Value = serde_json::from_str(&good).unwrap();
    value["symbols"][0]["symbol"] = json!("WHAT");
    std::fs::write(&path, value.to_string()).unwrap();
    assert!(
        matches!(save::read(&path), Err(save::SaveError::Symbols { .. })),
        "an unknown symbol list is refused"
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
