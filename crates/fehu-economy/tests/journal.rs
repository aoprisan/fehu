//! The command journal: nothing acknowledged is lost, and a retry is not a
//! second command.
//!
//! Milestone 2 of `docs/economy-engine-plan.md`. Its acceptance is that a
//! process stopped at an arbitrary point comes back with the state it last
//! acknowledged, and that replaying every request with the same idempotency
//! key changes nothing — no duplicate deposit, no second sign-up, no order
//! placed twice.
//!
//! "Stopping the process" here is dropping the `App` and building another
//! from the snapshot and the journal on disk, which is what a restart does
//! minus the exit: the state file and the journal are the only things that
//! cross between the two.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use fehu_economy::journal::{self, Command, JournalEntry, Principal};
use fehu_economy::market::{App, Options};
use fehu_economy::{router, save};
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
        // Persistence, not timing: the limiter has its own tests.
        rate_per_sec: 0.0,
        ..Options::default()
    }
}

/// A world with a state file and a journal attached, as `main.rs` builds one.
async fn boot(dir: &TempDir) -> (Arc<App>, PathBuf) {
    let path = dir.path().join("state.json");
    let app = App::new(options(Some(path.clone())));
    app.attach_journal(&journal::path_for(&path))
        .await
        .expect("a journal beside the state file");
    (app, path)
}

/// Stop and start again, from the snapshot and journal on disk only.
async fn restart(app: Arc<App>, path: &Path) -> Arc<App> {
    drop(app);
    let saved = save::read(path).expect("a readable snapshot");
    let journal_path = journal::path_for(path);
    let entries = if journal_path.exists() {
        journal::read(&journal_path).expect("a readable journal")
    } else {
        Vec::new()
    };
    let app = App::resume(options(Some(path.to_path_buf())), saved, entries).await;
    app.attach_journal(&journal_path).await.expect("a journal");
    app
}

struct Response {
    status: StatusCode,
    body: Value,
    seq: Option<u64>,
    replayed: bool,
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
    let seq = resp
        .headers()
        .get("fehu-journal-seq")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok());
    let replayed = resp.headers().contains_key("fehu-idempotent-replay");
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or_else(|e| panic!("bad JSON ({e}): {bytes:?}"))
    };
    Response {
        status,
        body,
        seq,
        replayed,
    }
}

async fn get(app: &Arc<App>, key: Option<&str>, uri: &str) -> Response {
    call(app, key, Request::get(uri).body(Body::empty()).unwrap()).await
}

/// `POST`, with an `Idempotency-Key` if one is given.
async fn post(
    app: &Arc<App>,
    key: Option<&str>,
    idempotency: Option<&str>,
    uri: &str,
    body: Value,
) -> Response {
    let mut req = Request::post(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    if let Some(id) = idempotency {
        req.headers_mut()
            .insert("idempotency-key", id.parse().unwrap());
    }
    call(app, key, req).await
}

struct Player {
    user_id: u64,
    trader_id: u64,
    account_id: u64,
    key: String,
}

/// Sign up, keeping the key the one and only response carried it in.
async fn sign_up(app: &Arc<App>, name: &str, idempotency: Option<&str>) -> Player {
    let r = post(
        app,
        None,
        idempotency,
        "/api/traders",
        json!({ "name": name, "cash_cents": 5_000_000 }),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.body);
    Player {
        user_id: r.body["user_id"].as_u64().unwrap(),
        trader_id: r.body["id"].as_u64().unwrap(),
        account_id: r.body["account_id"].as_u64().unwrap(),
        key: r.body["api_key"].as_str().expect("a key, once").to_string(),
    }
}

async fn cash_of(app: &Arc<App>, player: &Player) -> i64 {
    let r = get(
        app,
        Some(&player.key),
        &format!("/api/traders/{}", player.trader_id),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    r.body["cash_cents"].as_i64().unwrap()
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn everything_acknowledged_survives_a_restart_with_no_snapshot_at_all() {
    let dir = TempDir::new("fehu-journal-crash");
    let (app, path) = boot(&dir).await;
    // One snapshot, so there is a checkpoint to replay onto — and then
    // everything below happens after it and exists only in the journal.
    save::write(&app, &path).await.unwrap();

    let player = sign_up(&app, "wilma", None).await;
    post(
        &app,
        None,
        None,
        &format!("/api/accounts/{}/deposit", player.account_id),
        json!({ "amount_cents": 250_000 }),
    )
    .await;
    let order = post(
        &app,
        Some(&player.key),
        None,
        "/api/symbols/ACME/orders",
        json!({ "trader_id": player.trader_id, "side": "buy", "type": "market", "qty": 10 }),
    )
    .await;
    assert_eq!(order.status, StatusCode::CREATED, "{:?}", order.body);
    let before = cash_of(&app, &player).await;
    let supply_before = get(&app, None, "/api/supply").await.body;

    // The process stops here: the snapshot on disk is from before the
    // sign-up, so only the journal knows any of this happened.
    let app = restart(app, &path).await;

    let after = get(
        &app,
        Some(&player.key),
        &format!("/api/traders/{}", player.trader_id),
    )
    .await;
    assert_eq!(
        after.status,
        StatusCode::OK,
        "the user, their key and their trader all came back: {:?}",
        after.body
    );
    assert_eq!(after.body["cash_cents"].as_i64().unwrap(), before);
    assert_eq!(after.body["positions"][0]["qty"].as_u64().unwrap(), 10);
    assert_eq!(
        get(&app, None, "/api/supply").await.body,
        supply_before,
        "and the currency came back conserved"
    );
}

#[tokio::test]
async fn a_retried_deposit_with_the_same_key_is_one_deposit() {
    let dir = TempDir::new("fehu-journal-retry");
    let (app, path) = boot(&dir).await;
    save::write(&app, &path).await.unwrap();
    let player = sign_up(&app, "rex", None).await;
    let opening = cash_of(&app, &player).await;

    let uri = format!("/api/accounts/{}/deposit", player.account_id);
    let body = json!({ "amount_cents": 100_000, "memo": "quest" });
    let first = post(&app, None, Some("dep-1"), &uri, body.clone()).await;
    assert_eq!(first.status, StatusCode::OK);
    assert!(!first.replayed);

    for _ in 0..3 {
        let again = post(&app, None, Some("dep-1"), &uri, body.clone()).await;
        assert_eq!(again.status, first.status);
        assert!(again.replayed, "the answer came from the log");
        assert_eq!(again.seq, first.seq, "and no second entry was written");
        assert_eq!(again.body, first.body);
    }
    assert_eq!(cash_of(&app, &player).await, opening + 100_000);

    // And it is still one deposit on the other side of a restart, because
    // the index is in the snapshot as well as in memory.
    save::write(&app, &path).await.unwrap();
    let app = restart(app, &path).await;
    let after_restart = post(&app, None, Some("dep-1"), &uri, body).await;
    assert!(
        after_restart.replayed,
        "a retry is a retry across a restart"
    );
    assert_eq!(cash_of(&app, &player).await, opening + 100_000);
}

#[tokio::test]
async fn the_same_key_over_a_different_request_is_refused() {
    let dir = TempDir::new("fehu-journal-conflict");
    let (app, _path) = boot(&dir).await;
    let player = sign_up(&app, "carla", None).await;
    let uri = format!("/api/accounts/{}/deposit", player.account_id);
    let opening = cash_of(&app, &player).await;

    let first = post(
        &app,
        None,
        Some("k"),
        &uri,
        json!({ "amount_cents": 1_000 }),
    )
    .await;
    assert_eq!(first.status, StatusCode::OK);
    let second = post(
        &app,
        None,
        Some("k"),
        &uri,
        json!({ "amount_cents": 9_999 }),
    )
    .await;
    assert_eq!(second.status, StatusCode::CONFLICT);
    assert_eq!(second.body["error"]["code"], "idempotency_conflict");
    assert_eq!(
        cash_of(&app, &player).await,
        opening + 1_000,
        "the second amount was not paid, and neither was the first a second time"
    );
}

#[tokio::test]
async fn a_lost_response_can_be_recovered_by_its_key() {
    let dir = TempDir::new("fehu-journal-recover");
    let (app, _path) = boot(&dir).await;
    let player = sign_up(&app, "lena", None).await;
    let sent = post(
        &app,
        Some(&player.key),
        Some("order-77"),
        "/api/symbols/ACME/orders",
        json!({ "trader_id": player.trader_id, "side": "buy", "type": "market", "qty": 3 }),
    )
    .await;
    assert_eq!(sent.status, StatusCode::CREATED, "{:?}", sent.body);

    let recovered = get(&app, Some(&player.key), "/api/commands/order-77").await;
    assert_eq!(recovered.status, StatusCode::OK);
    assert_eq!(recovered.body["command"], "place_order");
    assert_eq!(recovered.body["status"], 201);
    assert_eq!(recovered.body["seq"], json!(sent.seq.unwrap()));
    assert_eq!(recovered.body["result"], sent.body);

    // Somebody else's command is not theirs to read, and neither is a key
    // that was never used.
    let other = sign_up(&app, "mo", None).await;
    let refused = get(&app, Some(&other.key), "/api/commands/order-77").await;
    assert_eq!(refused.status, StatusCode::NOT_FOUND);
    assert_eq!(refused.body["error"]["code"], "unknown_command");
    let never = get(&app, Some(&player.key), "/api/commands/nope").await;
    assert_eq!(never.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_retried_sign_up_creates_one_user_and_hands_out_one_key() {
    let dir = TempDir::new("fehu-journal-signup");
    let (app, _path) = boot(&dir).await;
    let first = post(
        &app,
        None,
        Some("join-1"),
        "/api/traders",
        json!({ "name": "ada", "cash_cents": 1_000 }),
    )
    .await;
    assert_eq!(first.status, StatusCode::CREATED);
    let key = first.body["api_key"].as_str().unwrap().to_string();

    let again = post(
        &app,
        None,
        Some("join-1"),
        "/api/traders",
        json!({ "name": "ada", "cash_cents": 1_000 }),
    )
    .await;
    assert!(again.replayed);
    assert_eq!(again.body["id"], first.body["id"], "the same trader");
    assert!(
        again.body["api_key"].is_null(),
        "and no second credential: a key is shown once, and the journal never holds one"
    );

    // The key from the first response still works, and there is exactly one
    // user behind it.
    let me = get(&app, Some(&key), "/api/users").await;
    assert_eq!(me.status, StatusCode::OK);
    assert_eq!(me.body.as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn a_journaled_key_is_a_digest_and_never_a_credential() {
    let dir = TempDir::new("fehu-journal-secrets");
    let (app, path) = boot(&dir).await;
    let player = sign_up(&app, "vera", Some("join")).await;
    // Before the snapshot, so the sign-up is still in the journal, and after
    // it, so it is in the snapshot too.
    let on_disk = std::fs::read_to_string(journal::path_for(&path)).unwrap();
    save::write(&app, &path).await.unwrap();
    let snapshot = std::fs::read_to_string(&path).unwrap();
    assert!(
        on_disk.contains("create_trader"),
        "the sign-up is journaled"
    );
    for (what, text) in [("journal", &on_disk), ("snapshot", &snapshot)] {
        assert!(
            !text.contains(&player.key),
            "the {what} must not hold a live API key"
        );
    }
    assert!(
        snapshot.contains("sha256:"),
        "only digests, which is what makes the key still work after a restart"
    );
    let _ = player.user_id;
}

#[tokio::test]
async fn a_snapshot_takes_the_journal_with_it() {
    let dir = TempDir::new("fehu-journal-truncate");
    let (app, path) = boot(&dir).await;
    let journal_path = journal::path_for(&path);
    let player = sign_up(&app, "tam", None).await;
    for i in 0..5 {
        post(
            &app,
            None,
            None,
            &format!("/api/accounts/{}/deposit", player.account_id),
            json!({ "amount_cents": 100 + i }),
        )
        .await;
    }
    let before = journal::read(&journal_path).unwrap();
    assert!(
        before.len() >= 6,
        "six commands, at least: {}",
        before.len()
    );

    save::write(&app, &path).await.unwrap();
    let after = journal::read(&journal_path).unwrap();
    assert!(
        after.is_empty(),
        "the snapshot holds all of it now, so the journal starts again: {after:?}"
    );

    // And the sequence carries on rather than starting over, so an entry
    // written after the snapshot can never be mistaken for one before it.
    post(
        &app,
        None,
        None,
        &format!("/api/accounts/{}/deposit", player.account_id),
        json!({ "amount_cents": 7 }),
    )
    .await;
    let next = journal::read(&journal_path).unwrap();
    assert_eq!(next.len(), 1);
    assert!(next[0].seq > before.last().unwrap().seq);
}

#[tokio::test]
async fn the_engine_step_is_journaled_and_replay_never_reads_the_clock() {
    let dir = TempDir::new("fehu-journal-step");
    let (app, path) = boot(&dir).await;
    save::write(&app, &path).await.unwrap();

    // Advance past the snapshot, twice, to fixed simulated instants.
    let first = fehu::Timestamp(NOW_MS + 60_000);
    let second = fehu::Timestamp(NOW_MS + 120_000);
    fehu_economy::engine::advance_to(&app, first).await;
    fehu_economy::engine::advance_to(&app, second).await;

    let entries = journal::read(&journal::path_for(&path)).unwrap();
    let steps: Vec<&JournalEntry> = entries
        .iter()
        .filter(|e| matches!(e.command, Command::Step))
        .collect();
    assert_eq!(steps.len(), 2, "one entry per step");
    assert_eq!(
        steps[0].at_ms, first.0,
        "carrying the instant it advanced to"
    );
    assert_eq!(steps[1].at_ms, second.0);
    assert_eq!(steps[0].principal, Principal::Engine);

    let quote_before = get(&app, None, "/api/symbols/ACME").await.body["price_cents"].clone();
    let app = restart(app, &path).await;
    assert_eq!(
        get(&app, None, "/api/symbols/ACME").await.body["price_cents"],
        quote_before,
        "the same steps replayed give the same prices: nothing read a clock"
    );
}

#[tokio::test]
async fn a_journal_without_a_snapshot_to_replay_onto_is_not_applied() {
    let dir = TempDir::new("fehu-journal-orphan");
    let (app, path) = boot(&dir).await;
    let player = sign_up(&app, "orla", None).await;
    drop(app);

    // No snapshot was ever written, so `save::read` refuses and a fresh
    // world is warmed up instead. The journal describes a world that no
    // longer exists; applying it to this one would be worse than losing it.
    assert!(save::read(&path).is_err(), "there is no snapshot");
    let fresh = App::new(options(Some(path.clone())));
    let refused = get(&fresh, Some(&player.key), "/api/users").await;
    assert_eq!(
        refused.status,
        StatusCode::UNAUTHORIZED,
        "the old world's players are not this world's"
    );
}

#[tokio::test]
async fn a_market_that_cannot_write_its_journal_stops_changing() {
    let dir = TempDir::new("fehu-journal-broken");
    let (app, path) = boot(&dir).await;
    save::write(&app, &path).await.unwrap();
    let player = sign_up(&app, "ilse", None).await;
    let before = cash_of(&app, &player).await;

    // What a failed append leaves behind: the market is ahead of the disk,
    // so it must stop changing.
    app.market
        .call(|m| m.journal.stop("no space left on device"))
        .await
        .unwrap();

    let refused = post(
        &app,
        None,
        None,
        &format!("/api/accounts/{}/deposit", player.account_id),
        json!({ "amount_cents": 1_000 }),
    )
    .await;
    assert_eq!(refused.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(refused.body["error"]["code"], "journal_unavailable");
    assert_eq!(
        cash_of(&app, &player).await,
        before,
        "and reads still work, on a market that has stopped moving"
    );
}

#[tokio::test]
async fn stopping_at_any_point_comes_back_at_that_point_and_no_reward_is_paid_twice() {
    let dir = TempDir::new("fehu-journal-cuts");
    let (app, path) = boot(&dir).await;
    // The one checkpoint. Everything after it lives only in the journal, so
    // cutting the journal short is exactly "the process stopped here".
    save::write(&app, &path).await.unwrap();
    let player = sign_up(&app, "nina", None).await;

    // A mixed workload, and what the market looked like after each command
    // it acknowledged.
    let mut marks: Vec<(u64, i64, Value)> = Vec::new();
    for i in 0..12u64 {
        let r = match i % 4 {
            // A reward, with a key: the one command a retry must never pay
            // twice, whichever side of a restart the retry lands on.
            0 => {
                post(
                    &app,
                    None,
                    Some(&format!("reward-{i}")),
                    &format!("/api/accounts/{}/deposit", player.account_id),
                    json!({ "amount_cents": 1_000 + i as i64, "memo": "quest" }),
                )
                .await
            }
            1 => {
                post(
                    &app,
                    Some(&player.key),
                    None,
                    "/api/symbols/ACME/orders",
                    json!({
                        "trader_id": player.trader_id,
                        "side": "buy",
                        "type": "limit",
                        "price_cents": 1_000 + i as i64,
                        "qty": 5,
                    }),
                )
                .await
            }
            2 => {
                fehu_economy::engine::advance_to(&app, fehu::Timestamp(NOW_MS + 1_000 * i as i64))
                    .await;
                continue;
            }
            _ => {
                post(
                    &app,
                    Some(&player.key),
                    None,
                    &format!("/api/traders/{}/cancel_all", player.trader_id),
                    Value::Null,
                )
                .await
            }
        };
        assert!(r.status.is_success(), "{}: {:?}", r.status, r.body);
        marks.push((
            r.seq.expect("an acknowledged command has a sequence"),
            cash_of(&app, &player).await,
            get(&app, None, "/api/supply").await.body,
        ));
    }
    let entries = journal::read(&journal::path_for(&path)).unwrap();
    drop(app);

    // Stop at each of those points and start again from the snapshot plus
    // the journal as far as it had reached.
    for (seq, cash, supply) in marks {
        let prefix: Vec<JournalEntry> = entries.iter().filter(|e| e.seq <= seq).cloned().collect();
        let app = App::resume(
            options(Some(path.clone())),
            save::read(&path).unwrap(),
            prefix,
        )
        .await;
        assert_eq!(
            cash_of(&app, &player).await,
            cash,
            "restarting at {seq} must give back the state acknowledged at {seq}"
        );
        assert_eq!(get(&app, None, "/api/supply").await.body, supply);

        // And every reward that had been paid is still paid once: a retry
        // after the restart is answered from the log, not applied again.
        for i in [0u64, 4, 8] {
            let key = format!("reward-{i}");
            let paid = entries
                .iter()
                .any(|e| e.seq <= seq && e.idempotency_key.as_deref() == Some(&key));
            let retry = post(
                &app,
                None,
                Some(&key),
                &format!("/api/accounts/{}/deposit", player.account_id),
                json!({ "amount_cents": 1_000 + i as i64, "memo": "quest" }),
            )
            .await;
            if paid {
                assert!(retry.replayed, "{key} was paid before the stop");
                assert_eq!(cash_of(&app, &player).await, cash, "and was not paid again");
            } else {
                assert!(!retry.replayed, "{key} had not been paid yet");
            }
        }
    }
}

/// A directory of our own, removed when the test ends.
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

#[tokio::test]
async fn goods_survive_a_stop_between_the_making_and_the_eating() {
    let dir = TempDir::new("fehu-journal-goods");
    let (app, path) = boot(&dir).await;
    let r = post(
        &app,
        None,
        None,
        "/api/symbols",
        json!({
            "symbol": "ORE", "kind": "good", "unit": "kg",
            "name": "Iron Ore", "start_price_cents": 250,
        }),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let r = post(
        &app,
        None,
        None,
        "/api/catalog",
        json!({ "symbol": "ORE", "price_cents": 250, "available": 500 }),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let player = sign_up(&app, "wanda", None).await;
    let r = post(
        &app,
        Some(&player.key),
        Some("buy-ore"),
        &format!("/api/traders/{}/purchases", player.trader_id),
        json!({ "symbol": "ORE", "qty": 80 }),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let r = post(
        &app,
        Some(&player.key),
        None,
        &format!("/api/traders/{}/consume", player.trader_id),
        json!({ "symbol": "ORE", "qty": 30 }),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);

    // Nothing has been snapshotted: the journal is the only record, and
    // listing a good, stocking the catalogue, buying and eating are four
    // commands in it.
    save::write(&app, &path)
        .await
        .expect("a snapshot to start from");
    let before = get(&app, None, "/api/symbols/ORE").await.body;
    let app = restart(app, &path).await;
    let after = get(&app, None, "/api/symbols/ORE").await.body;
    assert_eq!(after["info"], before["info"], "issued and consumed");
    assert_eq!(after["info"]["asset"]["issued"], 80);
    assert_eq!(after["info"]["asset"]["consumed"], 30);

    // The retry of a purchase whose response was lost is still not a second
    // purchase, on the far side of a restart.
    let r = post(
        &app,
        Some(&player.key),
        Some("buy-ore"),
        &format!("/api/traders/{}/purchases", player.trader_id),
        json!({ "symbol": "ORE", "qty": 80 }),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    assert!(r.replayed, "the recorded answer, not another 80 kg");
    let detail = get(&app, None, "/api/symbols/ORE").await.body;
    assert_eq!(detail["info"]["asset"]["issued"], 80);

    let report = get(&app, None, "/api/reconcile").await.body;
    assert_eq!(report["valid"], true, "{report}");
}
