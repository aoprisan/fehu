//! The outbox: what the game backend reads, and what it cannot miss.
//!
//! Milestone 5 of `docs/economy-engine-plan.md`. Its promise is at-least-once
//! delivery of every fact nobody asked for — a fill, a job coming due, an
//! order the venue withdrew, a dividend, an accepted game event — to a
//! consumer that reads at its own pace, from a cursor that means the same
//! thing on both sides of a restart.
//!
//! "Stopping the process" here is what `tests/journal.rs` does: drop the
//! `App` and build another from the snapshot and the journal on disk, which
//! is a restart minus the exit.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use fehu_webapp::market::{App, Options};
use fehu_webapp::{engine, journal, router, save};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::util::ServiceExt;

/// 2023-11-14T22:13:20Z, as in the other suites.
const NOW_MS: i64 = 1_700_000_000_000;

fn options(state_file: Option<PathBuf>) -> Options {
    Options {
        history_days: 1,
        warmup_hours: 1,
        now_ms: Some(NOW_MS),
        state_file,
        // Delivery, not timing: the limiter has its own tests.
        rate_per_sec: 0.0,
        ..Options::default()
    }
}

fn test_app() -> Arc<App> {
    App::new(options(None))
}

/// A world with a state file and a journal attached, as `main.rs` builds one.
async fn boot(dir: &TempDir, options: Options) -> (Arc<App>, PathBuf) {
    let path = dir.path().join("state.json");
    let options = Options {
        state_file: Some(path.clone()),
        ..options
    };
    let app = App::new(options);
    app.attach_journal(&journal::path_for(&path))
        .await
        .expect("a journal beside the state file");
    (app, path)
}

/// Stop and start again, from the snapshot and journal on disk only.
async fn restart(app: Arc<App>, path: &Path, options: Options) -> Arc<App> {
    drop(app);
    let saved = save::read(path).expect("a readable snapshot");
    let journal_path = journal::path_for(path);
    let entries = if journal_path.exists() {
        journal::read(&journal_path).expect("a readable journal")
    } else {
        Vec::new()
    };
    let options = Options {
        state_file: Some(path.to_path_buf()),
        ..options
    };
    let app = App::resume(options, saved, entries).await;
    app.attach_journal(&journal_path).await.expect("a journal");
    app
}

struct Response {
    status: StatusCode,
    body: Value,
}

async fn call(app: &Arc<App>, key: Option<&str>, mut req: Request<Body>) -> Response {
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
    Response { status, body }
}

async fn get(app: &Arc<App>, key: Option<&str>, uri: &str) -> Response {
    call(app, key, Request::get(uri).body(Body::empty()).unwrap()).await
}

async fn post(app: &Arc<App>, key: Option<&str>, uri: &str, body: Value) -> Response {
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

/// The outbox as the operator sees it, from `after`.
async fn read_outbox(app: &Arc<App>, after: Option<u64>) -> Value {
    let uri = match after {
        Some(after) => format!("/api/outbox?after={after}"),
        None => "/api/outbox".to_string(),
    };
    let r = get(app, None, &uri).await;
    assert_eq!(r.status, StatusCode::OK, "GET {uri}: {:?}", r.body);
    r.body
}

fn kinds(page: &Value) -> Vec<String> {
    page["events"]
        .as_array()
        .expect("events is an array")
        .iter()
        .map(|e| e["kind"].as_str().unwrap_or_default().to_string())
        .collect()
}

fn seqs(page: &Value) -> Vec<u64> {
    page["events"]
        .as_array()
        .expect("events is an array")
        .iter()
        .map(|e| e["seq"].as_u64().unwrap())
        .collect()
}

/// Push a game event at ACME: a fact with a source id, and the cheapest way
/// to put one in the outbox.
async fn push_event(app: &Arc<App>, source: &str) {
    let r = post(
        app,
        None,
        "/api/game/events",
        json!({ "kind": "scandal", "magnitude": 0.2, "symbol": "ACME", "source": source }),
    )
    .await;
    assert!(r.status.is_success(), "push {source}: {:?}", r.body);
}

struct Player {
    trader_id: u64,
    account_id: u64,
    key: String,
}

async fn sign_up(app: &Arc<App>, name: &str) -> Player {
    let r = post(
        app,
        None,
        "/api/traders",
        json!({ "name": name, "cash_cents": 5_000_000 }),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.body);
    Player {
        trader_id: r.body["id"].as_u64().unwrap(),
        account_id: r.body["account_id"].as_u64().unwrap(),
        key: r.body["api_key"].as_str().expect("a key, once").to_string(),
    }
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_fresh_world_has_an_empty_outbox_that_admits_nothing_is_missing() {
    let page = read_outbox(&test_app(), None).await;
    assert!(page["events"].as_array().unwrap().is_empty());
    assert_eq!(page["latest"], 0);
    assert_eq!(page["oldest"], 0);
    assert_eq!(page["cursor"], 0);
    assert_eq!(page["dropped"], 0);
    assert_eq!(page["gap"], false, "nothing was lost: nothing has happened");
    assert_eq!(page["cap"], fehu_webapp::outbox::DEFAULT_OUTBOX);
}

#[tokio::test]
async fn a_game_event_lands_in_the_outbox_under_the_command_that_made_it() {
    let app = test_app();
    let r = post(
        &app,
        None,
        "/api/game/events",
        json!({ "kind": "scandal", "magnitude": 0.5, "symbol": "ACME", "source": "quest-9" }),
    )
    .await;
    assert!(r.status.is_success(), "{:?}", r.body);

    let page = read_outbox(&app, None).await;
    assert_eq!(kinds(&page), ["event"]);
    let entry = &page["events"][0];
    assert_eq!(entry["seq"], 1, "the first fact is 1");
    assert_eq!(entry["event"]["type"], "event");
    assert_eq!(entry["event"]["source"], "quest-9");
    assert!(
        entry["command_seq"].as_u64().unwrap() > 0,
        "a fact names the journal entry that caused it: {entry}"
    );
    assert_eq!(page["next"], 1);
    assert_eq!(page["latest"], 1);
    assert_eq!(page["pending"], 0);
}

#[tokio::test]
async fn only_what_nobody_asked_for_is_in_it() {
    let app = test_app();
    let player = sign_up(&app, "wilma").await;
    // Every one of these is a command with a response the caller already
    // has, so none of them is a fact the outbox has to deliver.
    post(
        &app,
        None,
        &format!("/api/accounts/{}/deposit", player.account_id),
        json!({ "amount_cents": 100_000 }),
    )
    .await;
    post(
        &app,
        None,
        "/api/budgets",
        json!({ "name": "quests", "cents": 1_000_000 }),
    )
    .await;
    let page = read_outbox(&app, None).await;
    assert!(
        page["events"].as_array().unwrap().is_empty(),
        "a sign-up, a deposit and a budget are answers, not facts: {page}"
    );

    // A fill is: the resting side never asked for it. Buy first, because
    // nothing here shorts, and then rest the shares back at a price the
    // other player will lift.
    let seller = sign_up(&app, "rex").await;
    let r = post(
        &app,
        Some(&seller.key),
        "/api/symbols/ACME/orders",
        json!({ "trader_id": seller.trader_id, "side": "buy", "type": "market", "qty": 5 }),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.body);
    let filled = read_outbox(&app, None).await;
    let after_buy = filled["latest"].as_u64().unwrap();

    let r = post(
        &app,
        Some(&seller.key),
        "/api/symbols/ACME/orders",
        json!({
            "trader_id": seller.trader_id,
            "side": "sell",
            "type": "limit",
            "qty": 5,
            "price_cents": 1,
        }),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.body);
    let r = post(
        &app,
        Some(&player.key),
        "/api/symbols/ACME/orders",
        json!({ "trader_id": player.trader_id, "side": "buy", "type": "market", "qty": 5 }),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.body);

    let page = read_outbox(&app, Some(after_buy)).await;
    let fills: Vec<_> = kinds(&page).into_iter().filter(|k| k == "fill").collect();
    assert_eq!(
        fills.len(),
        2,
        "both sides of the trade were told, the resting one included: {}",
        page["events"]
    );
}

#[tokio::test]
async fn ticks_are_not_in_it() {
    let app = test_app();
    let published = app.published().await;
    for i in 1..=3 {
        engine::advance_to(&app, fehu::Timestamp(NOW_MS + i * 60_000)).await;
    }
    assert!(
        app.published().await > published,
        "the stream carried the ticks"
    );
    let page = read_outbox(&app, None).await;
    assert!(
        !kinds(&page).iter().any(|k| k == "tick"),
        "market data is not a fact the backend has to be told: {page}"
    );
}

#[tokio::test]
async fn reading_does_not_consume_and_acknowledging_does_not_delete() {
    let app = test_app();
    for i in 0..3 {
        push_event(&app, &format!("e{i}")).await;
    }
    let first = read_outbox(&app, None).await;
    assert_eq!(seqs(&first).len(), 3);
    assert_eq!(
        seqs(&read_outbox(&app, None).await),
        seqs(&first),
        "at-least-once: a consumer that died before acting reads the same again"
    );

    let ack = post(&app, None, "/api/outbox/ack", json!({ "through": 2 })).await;
    assert_eq!(ack.status, StatusCode::OK, "{:?}", ack.body);
    assert_eq!(ack.body["cursor"], 2);
    assert_eq!(ack.body["pending"], 1);

    let after_ack = read_outbox(&app, None).await;
    assert_eq!(
        seqs(&after_ack),
        [3],
        "a read with no `after` carries on from the acknowledged cursor"
    );
    assert_eq!(
        seqs(&read_outbox(&app, Some(0)).await).len(),
        3,
        "and everything is still there for a consumer that asks for it"
    );
}

#[tokio::test]
async fn a_page_is_bounded_and_the_next_cursor_continues_it() {
    let app = test_app();
    for i in 0..5 {
        push_event(&app, &format!("e{i}")).await;
    }
    let r = get(&app, None, "/api/outbox?after=0&limit=2").await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(seqs(&r.body), [1, 2]);
    assert_eq!(r.body["next"], 2);
    assert_eq!(r.body["pending"], 3);

    let r = get(&app, None, "/api/outbox?after=2&limit=2").await;
    assert_eq!(seqs(&r.body), [3, 4]);
    assert_eq!(r.body["pending"], 1);
}

#[tokio::test]
async fn the_outbox_is_the_operators() {
    let app = App::new(Options {
        admin_key: Some("secret".into()),
        ..options(None)
    });
    let player = sign_up(&app, "wilma").await;
    assert_eq!(
        get(&app, Some(&player.key), "/api/outbox").await.status,
        StatusCode::UNAUTHORIZED,
        "the log is the whole world's: one player's fills are in it beside another's"
    );
    assert_eq!(
        post(
            &app,
            Some(&player.key),
            "/api/outbox/ack",
            json!({ "through": 1 })
        )
        .await
        .status,
        StatusCode::UNAUTHORIZED,
    );
    assert_eq!(
        get(&app, Some("secret"), "/api/outbox").await.status,
        StatusCode::OK,
    );
}

#[tokio::test]
async fn a_consumer_that_falls_too_far_behind_is_told_it_has() {
    let app = App::new(Options {
        outbox: 2,
        ..options(None)
    });
    for i in 0..5 {
        push_event(&app, &format!("e{i}")).await;
    }
    let page = read_outbox(&app, Some(0)).await;
    assert_eq!(page["cap"], 2);
    assert_eq!(page["latest"], 5);
    assert_eq!(page["oldest"], 4);
    assert_eq!(page["dropped"], 3, "three facts nobody will ever see");
    assert_eq!(
        page["gap"], true,
        "and the read says so rather than looking complete"
    );
    assert_eq!(seqs(&page), [4, 5]);
    assert_eq!(
        read_outbox(&app, Some(3)).await["gap"],
        false,
        "a consumer that kept up sees no gap"
    );
}

#[tokio::test]
async fn switching_the_outbox_off_keeps_nothing_and_says_so() {
    let app = App::new(Options {
        outbox: 0,
        ..options(None)
    });
    push_event(&app, "quest-1").await;
    let page = read_outbox(&app, None).await;
    assert_eq!(page["cap"], 0);
    assert!(page["events"].as_array().unwrap().is_empty());
    assert_eq!(page["latest"], 0);
    assert_eq!(page["gap"], false, "nothing was lost: nothing was kept");
}

#[tokio::test]
async fn the_log_and_the_cursor_come_back_the_same_after_a_restart() {
    let dir = TempDir::new("fehu-outbox-restart");
    let (app, path) = boot(&dir, options(None)).await;
    for i in 0..4 {
        push_event(&app, &format!("e{i}")).await;
    }
    post(&app, None, "/api/outbox/ack", json!({ "through": 2 })).await;
    // A snapshot, and then one more fact that only the journal knows about.
    save::write(&app, &path).await.unwrap();
    push_event(&app, "after-the-snapshot").await;
    let before = read_outbox(&app, Some(0)).await;
    assert_eq!(seqs(&before), [1, 2, 3, 4, 5]);

    let app = restart(app, &path, options(None)).await;

    let after = read_outbox(&app, Some(0)).await;
    assert_eq!(
        seqs(&after),
        [1, 2, 3, 4, 5],
        "the same facts, under the same numbers"
    );
    assert_eq!(
        after["events"][4]["event"]["source"], "after-the-snapshot",
        "including the one replay had to rebuild"
    );
    assert_eq!(
        after["cursor"], 2,
        "and the consumer is exactly where it said it was"
    );
    assert_eq!(
        seqs(&read_outbox(&app, None).await),
        [3, 4, 5],
        "so it resumes without being handed what it has already acted on"
    );
}

#[tokio::test]
async fn a_refused_command_leaves_nothing_in_the_outbox() {
    let app = test_app();
    let before = read_outbox(&app, None).await["latest"].as_u64().unwrap();
    // A delisting the issuer cannot fund is refused after the market has
    // begun looking at it; nothing may be left behind, in the outbox least
    // of all.
    let r = post(
        &app,
        None,
        "/api/symbols/NOPE/delist",
        json!({ "source": "gm" }),
    )
    .await;
    assert!(r.status.is_client_error(), "{:?}", r.body);
    assert_eq!(
        read_outbox(&app, None).await["latest"].as_u64().unwrap(),
        before,
        "a refusal changed nothing, so it published nothing worth keeping"
    );
}

#[tokio::test]
async fn a_job_coming_due_is_delivered_to_the_backend() {
    let app = test_app();
    let player = sign_up(&app, "smith").await;
    let r = post(
        &app,
        None,
        "/api/symbols",
        json!({
            "symbol": "IRON",
            "kind": "good",
            "name": "Iron Ingot",
            "unit": "ingot",
            "start_price_cents": 500,
        }),
    )
    .await;
    assert!(r.status.is_success(), "{:?}", r.body);
    let r = post(
        &app,
        None,
        "/api/recipes",
        json!({
            "id": "smelt",
            "duration_secs": 1,
            "cost_cents": 0,
            "outputs": [{ "symbol": "IRON", "qty": 2 }],
        }),
    )
    .await;
    assert!(r.status.is_success(), "{:?}", r.body);
    let r = post(
        &app,
        Some(&player.key),
        "/api/jobs",
        json!({ "trader_id": player.trader_id, "recipe": "smelt" }),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.body);

    // Starting a job is a command with a response; its completion is not.
    assert!(
        !kinds(&read_outbox(&app, None).await)
            .iter()
            .any(|k| k == "job_done"),
    );
    for _ in 0..3 {
        engine::step(&app).await;
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    }
    engine::step(&app).await;

    let page = read_outbox(&app, Some(0)).await;
    let done = page["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "job_done")
        .unwrap_or_else(|| panic!("no job_done in {}", page["events"]));
    assert_eq!(done["event"]["trader_id"], player.trader_id);
    assert_eq!(done["event"]["delivered"][0]["symbol"], "IRON");
    assert_eq!(done["event"]["delivered"][0]["qty"], 2);
}

#[tokio::test]
async fn an_acknowledgement_is_a_command_like_any_other() {
    let app = test_app();
    push_event(&app, "e0").await;
    let first = call(
        &app,
        None,
        Request::post("/api/outbox/ack")
            .header(header::CONTENT_TYPE, "application/json")
            .header("idempotency-key", "ack-1")
            .body(Body::from(json!({ "through": 1 }).to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(first.status, StatusCode::OK, "{:?}", first.body);
    let seq = get(&app, None, "/api/commands/ack-1").await;
    assert_eq!(seq.status, StatusCode::OK, "{:?}", seq.body);
    assert_eq!(seq.body["command"], "ack_outbox");
    assert_eq!(seq.body["result"]["cursor"], 1);
}

#[tokio::test]
async fn an_acknowledgement_never_rewinds_or_runs_ahead() {
    let app = test_app();
    for i in 0..2 {
        push_event(&app, &format!("e{i}")).await;
    }
    assert_eq!(
        post(&app, None, "/api/outbox/ack", json!({ "through": 2 }))
            .await
            .body["cursor"],
        2
    );
    assert_eq!(
        post(&app, None, "/api/outbox/ack", json!({ "through": 1 }))
            .await
            .body["cursor"],
        2,
        "a late acknowledgement is not a rewind"
    );
    assert_eq!(
        post(&app, None, "/api/outbox/ack", json!({ "through": 999 }))
            .await
            .body["cursor"],
        2,
        "and never past what exists"
    );
}

// ---------------------------------------------------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new(prefix: &str) -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let path = std::env::temp_dir().join(format!("{prefix}-{unique}-{}", std::process::id()));
        std::fs::create_dir_all(&path).expect("a temporary directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
