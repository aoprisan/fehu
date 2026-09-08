//! The declared load, and what it costs.
//!
//! Milestone 5 of `docs/economy-engine-plan.md` asks for a load test with a
//! declared target, and for the recovery time and the reconciliation after
//! it. This is that test, and the numbers below are the declaration.
//!
//! # What is declared
//!
//! **The load.** 32 players sending 16 mutations each — 512 orders and
//! deposits — as fast as their tasks can post them, on four symbols, with
//! the rate limiter switched off so that what is being measured is the
//! server rather than the allowance. That is a burst well past anything a
//! single-player world produces and enough to keep the market actor's
//! mailbox non-empty throughout.
//!
//! **The target.** Every one of those is answered, none is shed, the whole
//! burst is through in under [`BURST_TARGET`], and the 95th percentile of
//! one request is under [`P95_TARGET`]. Both bounds are deliberately loose:
//! this runs in a debug build on whatever CI is given, beside every other
//! test binary, and a test that fails when the machine is busy is a test
//! nobody trusts. What they are really asserting is a *shape*. The whole
//! burst is in flight at once against an actor that runs one job at a time,
//! so the median is most of a queue's worth of waiting by construction; what
//! must not happen is a tail that runs away from it. On the machine this was
//! written on the p50 is around 70 ms and the p95 around 130 ms — a ratio
//! under two — for about four thousand commands a second.
//!
//! **The recovery.** After the burst, a restart from the snapshot and the
//! journal comes back in under [`RECOVERY_TARGET`] and reconciles: every
//! wallet, every reservation and the currency itself add up, and the outbox
//! comes back where it was.
//!
//! # Why it is bounded at all
//!
//! Because the server refuses to queue without limit. Every mutation is one
//! job on the market actor, whose mailbox is unbounded; `FEHU_MAX_INFLIGHT`
//! is what keeps the number of jobs waiting in it bounded, and past that a
//! client is told `503 overloaded` at once rather than being made to wait
//! for a reply it can no longer use. The second half of this file is that
//! promise: with the bound set to one, the server sheds instead of queueing,
//! and it recovers the moment the work drains.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use fehu_webapp::market::{App, Options};
use fehu_webapp::{journal, router, save};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::util::ServiceExt;

/// 2023-11-14T22:13:20Z, as in the other suites.
const NOW_MS: i64 = 1_700_000_000_000;

/// Players in the burst.
const PLAYERS: usize = 32;

/// Mutations each of them sends.
const PER_PLAYER: usize = 16;

/// The 95th percentile one mutation may take under the load above.
const P95_TARGET: Duration = Duration::from_millis(750);

/// How long the whole burst may take. The steadier of the two numbers: it is
/// throughput, and it does not move with how the tail happened to land.
const BURST_TARGET: Duration = Duration::from_secs(5);

/// How long a restart may take to be answering again, from a snapshot plus
/// the journal of everything the burst did.
const RECOVERY_TARGET: Duration = Duration::from_secs(20);

const TICKERS: [&str; 4] = ["ACME", "NBLA", "HLIO", "PXCO"];

fn options(state_file: Option<PathBuf>) -> Options {
    Options {
        history_days: 0,
        warmup_hours: 0,
        now_ms: Some(NOW_MS),
        state_file,
        // The server, not the allowance: a limiter would measure itself.
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

async fn sign_up(app: &Arc<App>, name: &str) -> (u64, String) {
    let (status, body) = post(
        app,
        None,
        "/api/traders",
        json!({ "name": name, "cash_cents": 100_000_000 }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    (
        body["id"].as_u64().unwrap(),
        body["api_key"].as_str().unwrap().to_owned(),
    )
}

/// The percentile of a sorted slice, by nearest rank.
fn percentile(sorted: &[Duration], pct: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let rank = ((pct / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

/// What a run of the load looked like.
struct Latencies {
    sorted: Vec<Duration>,
    /// Wall time from the first request posted to the last answered.
    elapsed: Duration,
    shed: usize,
    refused: usize,
}

impl Latencies {
    fn report(&self, what: &str) {
        println!(
            "{what}: n={} in {:?} p50={:?} p95={:?} p99={:?} max={:?} shed={} refused={}",
            self.sorted.len(),
            self.elapsed,
            percentile(&self.sorted, 50.0),
            percentile(&self.sorted, 95.0),
            percentile(&self.sorted, 99.0),
            self.sorted.last().copied().unwrap_or_default(),
            self.shed,
            self.refused,
        );
    }
}

/// Run the declared burst against `app` and time every request.
async fn burst(app: &Arc<App>) -> Latencies {
    let mut players = Vec::with_capacity(PLAYERS);
    for i in 0..PLAYERS {
        players.push(sign_up(app, &format!("load-{i}")).await);
    }

    let started = Instant::now();
    let mut tasks = Vec::with_capacity(PLAYERS);
    for (n, (trader_id, key)) in players.into_iter().enumerate() {
        let app = Arc::clone(app);
        tasks.push(tokio::spawn(async move {
            let mut taken = Vec::with_capacity(PER_PLAYER);
            for i in 0..PER_PLAYER {
                let symbol = TICKERS[(n + i) % TICKERS.len()];
                let started = Instant::now();
                let (status, body) = post(
                    &app,
                    Some(&key),
                    &format!("/api/symbols/{symbol}/orders"),
                    json!({
                        "trader_id": trader_id,
                        "side": "buy",
                        "type": "market",
                        "qty": 1,
                    }),
                )
                .await;
                taken.push((started.elapsed(), status, body));
            }
            taken
        }));
    }

    let mut sorted = Vec::with_capacity(PLAYERS * PER_PLAYER);
    let (mut shed, mut refused) = (0, 0);
    for task in tasks {
        for (took, status, body) in task.await.expect("a load task") {
            sorted.push(took);
            match status {
                StatusCode::SERVICE_UNAVAILABLE => shed += 1,
                s if s.is_success() => {}
                _ => {
                    refused += 1;
                    println!("refused: {status} {body}");
                }
            }
        }
    }
    let elapsed = started.elapsed();
    sorted.sort_unstable();
    Latencies {
        sorted,
        elapsed,
        shed,
        refused,
    }
}

// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_declared_load_is_answered_within_the_declared_target() {
    let app = App::new(options(None));
    let taken = burst(&app).await;
    taken.report("burst");

    assert_eq!(
        taken.sorted.len(),
        PLAYERS * PER_PLAYER,
        "every request was answered"
    );
    assert_eq!(taken.refused, 0, "and none of them was refused");
    assert_eq!(
        taken.shed, 0,
        "the declared load is inside `FEHU_MAX_INFLIGHT`, so nothing was shed"
    );

    assert!(
        taken.elapsed < BURST_TARGET,
        "the burst took {:?}, over the declared {BURST_TARGET:?}",
        taken.elapsed
    );
    let p95 = percentile(&taken.sorted, 95.0);
    assert!(
        p95 < P95_TARGET,
        "p95 was {p95:?}, over the declared {P95_TARGET:?}"
    );

    let (_, health) = get(&app, None, "/api/health").await;
    assert_eq!(health["metrics"]["requests_shed"], 0);
    assert_eq!(
        health["requests_in_flight"], 0,
        "and every place was given back"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_full_server_sheds_rather_than_queueing_and_comes_back() {
    // One place, so the second concurrent change has nowhere to go.
    let app = App::new(Options {
        max_inflight: 1,
        ..options(None)
    });
    let taken = burst(&app).await;
    taken.report("saturated");
    assert!(
        taken.shed > 0,
        "with one place and {PLAYERS} players, somebody had to be turned away"
    );
    assert_eq!(taken.refused, 0, "shedding is the only refusal here");

    let (_, health) = get(&app, None, "/api/health").await;
    assert_eq!(
        health["metrics"]["requests_shed"].as_u64().unwrap() as usize,
        taken.shed,
        "and the server says how many it turned away"
    );
    assert_eq!(health["max_in_flight"], 1);
    assert_eq!(
        health["requests_in_flight"], 0,
        "the bound is on work in flight, not a leak: it drained"
    );

    // The moment the burst is over the server takes changes again. A shed
    // request is a "later", not a broken server.
    let (trader_id, key) = sign_up(&app, "after").await;
    let (status, body) = post(
        &app,
        Some(&key),
        "/api/symbols/ACME/orders",
        json!({ "trader_id": trader_id, "side": "buy", "type": "market", "qty": 1 }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

#[tokio::test]
async fn a_shed_request_says_overloaded_and_when_to_come_back() {
    let app = App::new(Options {
        max_inflight: 1,
        ..options(None)
    });
    let (trader_id, key) = sign_up(&app, "one").await;
    // Hold the only place by hand, so the refusal is deterministic rather
    // than a race between tasks.
    let held = app.admission.mutation().expect("the one place");
    let response = router(Arc::clone(&app))
        .oneshot(
            Request::post("/api/symbols/ACME/orders")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::AUTHORIZATION, format!("Bearer {key}"))
                .body(Body::from(
                    json!({ "trader_id": trader_id, "side": "buy", "type": "market", "qty": 1 })
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response
            .headers()
            .get(header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok()),
        Some("1"),
        "a client that obeys the header must not spin"
    );
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["error"]["code"], "overloaded");

    drop(held);
    let (status, body) = post(
        &app,
        Some(&key),
        "/api/symbols/ACME/orders",
        json!({ "trader_id": trader_id, "side": "buy", "type": "market", "qty": 1 }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

#[tokio::test]
async fn a_read_is_never_shed() {
    let app = App::new(Options {
        max_inflight: 1,
        ..options(None)
    });
    let held = app.admission.mutation().expect("the one place");
    // `/api/health` is exactly what is worth reading when the server is
    // full, so it is not gated by how full the server is.
    let (status, health) = get(&app, None, "/api/health").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(health["requests_in_flight"], 1);
    assert_eq!(status, get(&app, None, "/api/supply").await.0);
    drop(held);
}

#[tokio::test]
async fn stream_connections_are_bounded_too() {
    let app = App::new(Options {
        max_streams: 1,
        ..options(None)
    });
    let first = app.admission.stream().expect("the one connection");
    let (status, body) = get(&app, None, "/api/stream").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["code"], "overloaded");
    drop(first);

    let response = router(Arc::clone(&app))
        .oneshot(Request::get("/api/stream").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "a closed connection gives its place back"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_burst_survives_a_restart_and_reconciles() {
    let dir = TempDir::new("fehu-load-restore");
    let path = dir.path().join("state.json");
    let app = App::new(options(Some(path.clone())));
    app.attach_journal(&journal::path_for(&path))
        .await
        .expect("a journal beside the state file");

    // A checkpoint first, so what follows is the journal's to remember.
    save::write(&app, &path).await.unwrap();
    let taken = burst(&app).await;
    taken.report("before the restart");
    assert_eq!(taken.refused, 0);

    let supply_before = get(&app, None, "/api/supply").await.1;
    let outbox_before = get(&app, None, "/api/outbox?after=0").await.1;
    let entries = journal::read(&journal::path_for(&path)).expect("a readable journal");
    assert!(
        entries.len() >= PLAYERS * PER_PLAYER,
        "the burst is on disk: {} entries",
        entries.len()
    );

    let started = Instant::now();
    drop(app);
    let saved = save::read(&path).expect("a readable snapshot");
    let app = App::resume(options(Some(path.clone())), saved, entries).await;
    let recovery = started.elapsed();
    println!("recovery: {recovery:?}");
    assert!(
        recovery < RECOVERY_TARGET,
        "recovery took {recovery:?}, over the declared {RECOVERY_TARGET:?}"
    );

    let (status, report) = get(&app, None, "/api/reconcile").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        report["valid"], true,
        "the restored world does not add up: {}",
        report["issues"]
    );
    assert_eq!(
        get(&app, None, "/api/supply").await.1,
        supply_before,
        "and the currency came back conserved"
    );
    let outbox_after = get(&app, None, "/api/outbox?after=0").await.1;
    assert_eq!(
        outbox_after["latest"], outbox_before["latest"],
        "with the same facts waiting for the game backend"
    );
}

/// The backup and restore drill, end to end.
///
/// Take a backup while the server is running, keep trading after it, then
/// bring a world up from the backup alone and check that it is the world as
/// of the backup — not the one that carried on without it, and not a broken
/// half of either.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_backup_taken_while_running_restores_to_the_world_it_was_taken_from() {
    let dir = TempDir::new("fehu-backup-drill");
    let path = dir.path().join("state.json");
    let app = App::new(options(Some(path.clone())));
    app.attach_journal(&journal::path_for(&path))
        .await
        .expect("a journal beside the state file");

    let (trader_id, key) = sign_up(&app, "keeper").await;
    for _ in 0..4 {
        let (status, body) = post(
            &app,
            Some(&key),
            "/api/symbols/ACME/orders",
            json!({ "trader_id": trader_id, "side": "buy", "type": "market", "qty": 2 }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    let (_, at_backup) = get(&app, Some(&key), &format!("/api/traders/{trader_id}")).await;

    // `curl -H 'Authorization: …' /api/backup > backup.json`, in a test.
    let (status, backup) = get(&app, None, "/api/backup").await;
    assert_eq!(status, StatusCode::OK, "{backup}");
    let backup_path = dir.path().join("backup.json");
    std::fs::write(&backup_path, backup.to_string()).expect("a backup on disk");

    // The world carries on after the backup was taken.
    for _ in 0..4 {
        let (status, body) = post(
            &app,
            Some(&key),
            "/api/symbols/ACME/orders",
            json!({ "trader_id": trader_id, "side": "buy", "type": "market", "qty": 2 }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    let (_, after) = get(&app, Some(&key), &format!("/api/traders/{trader_id}")).await;
    assert_ne!(
        after["positions"][0]["qty"], at_backup["positions"][0]["qty"],
        "the world moved on, so the backup is a real point in the past"
    );
    // Taking a backup does not disturb the live server's own persistence:
    // the journal it needs is still there, whole.
    let entries = journal::read(&journal::path_for(&path)).expect("a readable journal");
    assert!(
        entries.len() >= 9,
        "the live journal still carries everything since its own last snapshot: \
         {} entries",
        entries.len()
    );
    drop(app);

    // A world from the backup alone: no journal, because a snapshot is
    // complete on its own.
    let restored = save::read(&backup_path).expect("the backup reads as a snapshot");
    let restored = App::resume(options(Some(backup_path)), restored, Vec::new()).await;

    let (status, report) = get(&restored, None, "/api/reconcile").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        report["valid"], true,
        "the restored world does not add up: {}",
        report["issues"]
    );
    let (_, back) = get(&restored, Some(&key), &format!("/api/traders/{trader_id}")).await;
    assert_eq!(
        back["positions"][0]["qty"], at_backup["positions"][0]["qty"],
        "and it is the world the backup was taken from, keys and all"
    );
}

#[tokio::test]
async fn a_backup_is_the_operators() {
    let app = App::new(Options {
        admin_key: Some("secret".into()),
        ..options(None)
    });
    let (_, key) = sign_up(&app, "nosy").await;
    assert_eq!(
        get(&app, Some(&key), "/api/backup").await.0,
        StatusCode::UNAUTHORIZED,
        "a snapshot is every key digest and every balance in the world"
    );
    assert_eq!(
        get(&app, Some("secret"), "/api/backup").await.0,
        StatusCode::OK
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
