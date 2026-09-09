//! The operator's one read of the whole economy, and the flow meter under it.
//!
//! Two things are being tested. That `GET /api/overview` says the same as the
//! endpoints it replaces — a dashboard built on a summary is only worth
//! having if the summary is the same world the detail pages show. And that
//! the ledger's flow meter counts what actually moved: a reward is currency
//! changing hands, a deposit is currency being made, and an audit that could
//! not tell them apart would be no audit at all.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use fehu_economy::market::{App, Options};
use fehu_economy::{journal, router, save};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::util::ServiceExt;

/// 2023-11-14T22:13:20Z, as in the other suites.
const NOW_MS: i64 = 1_700_000_000_000;

fn options() -> Options {
    Options {
        history_days: 0,
        warmup_hours: 0,
        now_ms: Some(NOW_MS),
        rate_per_sec: 0.0,
        ..Options::default()
    }
}

fn test_app() -> Arc<App> {
    App::new(options())
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

/// `GET /api/overview`, which must succeed.
async fn overview(app: &Arc<App>) -> Value {
    let (status, body) = get(app, None, "/api/overview").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

/// One reason's row out of the flow meter.
fn flow<'a>(overview: &'a Value, reason: &str) -> &'a Value {
    overview["flows"]
        .as_array()
        .expect("flows is a list")
        .iter()
        .find(|f| f["reason"] == reason)
        .unwrap_or_else(|| panic!("no flow for {reason}"))
}

async fn sign_up(app: &Arc<App>, name: &str, cash_cents: i64) -> (u64, u64, String) {
    let (status, body) = post(
        app,
        None,
        "/api/traders",
        json!({ "name": name, "cash_cents": cash_cents }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    (
        body["id"].as_u64().unwrap(),
        body["account_id"].as_u64().unwrap(),
        body["api_key"].as_str().unwrap().to_owned(),
    )
}

#[tokio::test]
async fn the_overview_is_the_operator_s_to_read() {
    let app = App::new(Options {
        admin_key: Some("op-key".into()),
        ..options()
    });

    let (status, body) = get(&app, None, "/api/overview").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");

    let (status, body) = get(&app, Some("op-key"), "/api/overview").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // The aggregate half of it stays public: how much currency exists is not
    // a secret, whose it is is.
    let (status, supply) = get(&app, None, "/api/supply").await;
    assert_eq!(status, StatusCode::OK, "{supply}");
    assert_eq!(body["supply"], supply);

    // And the v1 path is the same read.
    let (status, v1) = get(&app, Some("op-key"), "/api/v1/economy/overview").await;
    assert_eq!(status, StatusCode::OK, "{v1}");
    assert_eq!(v1["wallets"], body["wallets"]);
}

#[tokio::test]
async fn the_overview_says_what_the_reads_it_replaces_say() {
    let app = test_app();
    let (trader, _, _) = sign_up(&app, "wren", 50_000).await;
    let (status, body) = post(
        &app,
        None,
        "/api/budgets",
        json!({ "name": "quests", "cash_cents": 100_000 }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, npc) = post(
        &app,
        None,
        "/api/npcs",
        json!({ "symbol": "ACME", "name": "acme desk", "cash_cents": 1_000_000 }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{npc}");

    let overview = overview(&app).await;
    let (_, supply) = get(&app, None, "/api/supply").await;
    let (_, budgets) = get(&app, None, "/api/budgets").await;
    let (_, npcs) = get(&app, None, "/api/npcs").await;
    let (_, world) = get(&app, None, "/api/world").await;

    assert_eq!(overview["supply"], supply);
    assert_eq!(overview["budgets"], budgets["budgets"]);
    assert_eq!(overview["rules"], budgets["rules"]);
    assert_eq!(overview["npcs"], npcs["npcs"]);
    assert_eq!(overview["effects"], world["symbols"]);
    assert_eq!(overview["modifiers"], world["modifiers"]);
    assert_eq!(overview["people"]["traders"], 2, "the player and the desk");
    assert!(overview["people"]["accounts"].as_u64().unwrap() >= 2);
    assert_eq!(overview["jobs"]["running"], 0);
    assert_eq!(overview["jobs"]["next_due_ms"], Value::Null);
    let _ = trader;
}

#[tokio::test]
async fn every_wallet_is_named_by_whoever_holds_it() {
    let app = test_app();
    sign_up(&app, "wren", 50_000).await;
    post(
        &app,
        None,
        "/api/budgets",
        json!({ "name": "quests", "cash_cents": 100_000 }),
    )
    .await;
    post(
        &app,
        None,
        "/api/npcs",
        json!({ "symbol": "ACME", "name": "acme desk", "cash_cents": 1_000_000 }),
    )
    .await;

    let overview = overview(&app).await;
    let wallets = overview["wallets"].as_array().unwrap();
    assert_eq!(
        wallets.len(),
        overview["supply"]["wallets"].as_u64().unwrap() as usize,
        "the directory lists every wallet the ledger has"
    );

    let named = |kind: &str, owner: &str| {
        wallets
            .iter()
            .any(|w| w["kind"] == kind && w["owner"] == owner)
    };
    assert!(named("player", "wren"), "{wallets:#?}");
    assert!(named("budget", "quests"), "{wallets:#?}");
    assert!(named("npc", "acme desk"), "{wallets:#?}");
    assert!(named("issuer", "ACME"), "{wallets:#?}");

    // The world's own wallets are their kind and nothing else.
    for kind in ["treasury", "venue", "issuance", "synthetic"] {
        let world = wallets.iter().find(|w| w["kind"] == kind).unwrap();
        assert_eq!(world["owner"], Value::Null, "{kind} belongs to nobody");
        assert_eq!(world["account_id"], Value::Null);
    }

    // Every row's arithmetic is the wallet's own: what can be spent is the
    // balance less what is committed, and never below zero — issuance and
    // the synthetic counterparty are the two that run negative.
    for w in wallets {
        let (balance, reserved) = (
            w["balance_cents"].as_i64().unwrap(),
            w["reserved_cents"].as_i64().unwrap(),
        );
        assert_eq!(
            w["available_cents"].as_i64().unwrap(),
            (balance - reserved).max(0),
            "{w}"
        );
    }
}

#[tokio::test]
async fn the_meter_tells_currency_made_from_currency_moved() {
    let app = test_app();
    let before = overview(&app).await;
    // A fresh world has minted exactly once: its genesis.
    assert_eq!(flow(&before, "genesis")["count"], 1);
    assert_eq!(
        flow(&before, "genesis")["cents"].as_i64().unwrap(),
        before["supply"]["minted_cents"].as_i64().unwrap()
    );
    assert_eq!(flow(&before, "mint")["count"], 0);

    // Signing up is the faucet: currency that already existed, moving.
    let (_, account, key) = sign_up(&app, "wren", 50_000).await;
    let after = overview(&app).await;
    assert_eq!(flow(&after, "faucet")["cents"], 50_000);
    assert_eq!(
        after["supply"]["minted_cents"], before["supply"]["minted_cents"],
        "a sign-up does not make currency"
    );

    // A deposit is the operator minting, and the meter says so.
    let (status, body) = post(
        &app,
        None,
        &format!("/api/accounts/{account}/deposit"),
        json!({ "amount_cents": 25_000 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let minted = overview(&app).await;
    assert_eq!(flow(&minted, "mint")["count"], 1);
    assert_eq!(flow(&minted, "mint")["cents"], 25_000);
    assert_eq!(
        minted["supply"]["minted_cents"].as_i64().unwrap(),
        before["supply"]["minted_cents"].as_i64().unwrap() + 25_000
    );

    // A withdrawal is the operator burning it again.
    let (status, body) = post(
        &app,
        Some(&key),
        &format!("/api/accounts/{account}/withdraw"),
        json!({ "amount_cents": 5_000 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let burned = overview(&app).await;
    assert_eq!(flow(&burned, "burn")["cents"], 5_000);
    assert_eq!(burned["supply"]["burned_cents"], 5_000);
    assert_eq!(burned["supply"]["balanced"], true);
}

#[tokio::test]
async fn a_reward_shows_up_as_a_reward_and_not_as_a_mint() {
    let app = test_app();
    let (trader, _, _) = sign_up(&app, "quinn", 0).await;
    let (_, budget) = post(
        &app,
        None,
        "/api/budgets",
        json!({ "name": "quests", "cash_cents": 100_000 }),
    )
    .await;
    let wallet = budget["wallet"].as_u64().unwrap();
    post(
        &app,
        None,
        "/api/rewards/rules",
        json!({ "id": "boss", "budget": wallet, "amount_cents": 5_000 }),
    )
    .await;
    let before = overview(&app).await;

    let (status, body) = post(
        &app,
        None,
        "/api/rewards",
        json!({ "rule": "boss", "trader_id": trader, "source": "kill-42" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let after = overview(&app).await;
    assert_eq!(flow(&after, "reward")["count"], 1);
    assert_eq!(flow(&after, "reward")["cents"], 5_000);
    assert_eq!(
        after["supply"]["minted_cents"], before["supply"]["minted_cents"],
        "a reward moves currency, it does not make it"
    );
    assert_eq!(after["budgets"][0]["paid_cents"], 5_000);

    // Paying for the same game event again pays nothing and counts nothing.
    let (status, again) = post(
        &app,
        None,
        "/api/rewards",
        json!({ "rule": "boss", "trader_id": trader, "source": "kill-42" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again["duplicate"], true);
    assert_eq!(flow(&overview(&app).await, "reward")["count"], 1);
}

#[tokio::test]
async fn a_refused_movement_is_not_counted() {
    let app = test_app();
    let (trader, _, _) = sign_up(&app, "quinn", 0).await;
    let (_, budget) = post(
        &app,
        None,
        "/api/budgets",
        json!({ "name": "quests", "cash_cents": 1_000 }),
    )
    .await;
    let wallet = budget["wallet"].as_u64().unwrap();
    post(
        &app,
        None,
        "/api/rewards/rules",
        json!({ "id": "boss", "budget": wallet, "amount_cents": 5_000 }),
    )
    .await;
    let before = overview(&app).await;

    // The budget cannot fund it, so nothing moves and nothing is metered.
    let (status, body) = post(
        &app,
        None,
        "/api/rewards",
        json!({ "rule": "boss", "trader_id": trader, "source": "kill-1" }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let after = overview(&app).await;
    assert_eq!(after["flows"], before["flows"]);
    assert_eq!(after["supply"], before["supply"]);
}

#[tokio::test]
async fn a_world_comes_back_knowing_what_has_moved() {
    let dir = tempdir_lite::TempDir::new("fehu-overview");
    let path = dir.path().join("state.json");
    let opts = || Options {
        state_file: Some(path.clone()),
        ..options()
    };
    let before = App::new(opts());
    let (_, account, _) = sign_up(&before, "wren", 50_000).await;
    post(
        &before,
        None,
        &format!("/api/accounts/{account}/deposit"),
        json!({ "amount_cents": 25_000 }),
    )
    .await;
    let taken = overview(&before).await;

    save::write(&before, &path).await.expect("state written");
    let after = App::restore(opts(), save::read(&path).unwrap());
    let restored = overview(&after).await;

    assert_eq!(restored["flows"], taken["flows"], "the meter is saved");
    assert_eq!(restored["wallets"], taken["wallets"]);
    assert_eq!(restored["supply"], taken["supply"]);
}

#[tokio::test]
async fn a_world_replayed_from_its_journal_counts_the_same_movements() {
    let dir = tempdir_lite::TempDir::new("fehu-overview-journal");
    let path = dir.path().join("state.json");
    let journal_path = journal::path_for(&path);
    let opts = || Options {
        state_file: Some(path.clone()),
        ..options()
    };
    // Snapshot an empty world, then work it: everything after the snapshot
    // is in the journal beside it and has to be replayed to be counted.
    let before = App::new(opts());
    before
        .attach_journal(&journal_path)
        .await
        .expect("a journal beside the state file");
    save::write(&before, &path).await.expect("state written");
    let (_, account, _) = sign_up(&before, "wren", 50_000).await;
    post(
        &before,
        None,
        &format!("/api/accounts/{account}/deposit"),
        json!({ "amount_cents": 25_000 }),
    )
    .await;
    let taken = overview(&before).await;
    assert!(taken["journal_seq"].as_u64().unwrap() >= 2);
    drop(before);

    let entries = journal::read(&journal_path).expect("a readable journal");
    let after = App::resume(opts(), save::read(&path).unwrap(), entries).await;
    let replayed = overview(&after).await;
    assert_eq!(
        replayed["flows"], taken["flows"],
        "replay moves the same currency for the same reasons"
    );
    assert_eq!(replayed["journal_seq"], taken["journal_seq"]);
}

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
