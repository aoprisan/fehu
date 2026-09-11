//! The third principal: what the game backend may do, and only that.
//!
//! Before this, every game-backend route was gated by the operator key
//! alone, so a backend that had to pay a quest reward held the credential
//! that can also mint currency, freeze an account and rewrite the catalogue.
//! The question each test here asks is the one least privilege exists to
//! answer: given a key that carries exactly one scope, is everything else
//! still shut.
//!
//! The other half is provisioning. A game already has an id for every
//! player long before they touch the economy, and it should not have to
//! remember a second one; `POST /api/v1/economy/players` maps its id onto a
//! user, an account and a trader, and is idempotent on it, so a backend can
//! call it on every login.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use fehu_economy::market::{App, Options};
use fehu_economy::{router, save};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::util::ServiceExt;

/// 2023-11-14T22:13:20Z, as in the other suites.
const NOW_MS: i64 = 1_700_000_000_000;

/// The operator key every test in this file locks its world with. A service
/// key only means anything on a server that has one — an unlocked server is
/// open to everybody by design.
const OPERATOR: &str = "operator-secret";

fn options() -> Options {
    options_at(None)
}

/// The same world, with a state file so a restart has something to read.
fn options_at(state_file: Option<std::path::PathBuf>) -> Options {
    Options {
        state_file,
        history_days: 0,
        warmup_hours: 0,
        now_ms: Some(NOW_MS),
        rate_per_sec: 0.0,
        admin_key: Some(OPERATOR.into()),
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
    let req = Request::post(uri).header(header::CONTENT_TYPE, "application/json");
    call(app, key, req.body(Body::from(body.to_string())).unwrap()).await
}

async fn delete(app: &Arc<App>, key: Option<&str>, uri: &str) -> (StatusCode, Value) {
    call(app, key, Request::delete(uri).body(Body::empty()).unwrap()).await
}

/// Issue a service credential and hand back its key.
async fn issue(app: &Arc<App>, name: &str, scopes: &[&str]) -> String {
    let (status, body) = post(
        app,
        Some(OPERATOR),
        "/api/v1/economy/admin/services",
        json!({ "name": name, "scopes": scopes }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "issuing {name}: {body}");
    body["api_key"]
        .as_str()
        .unwrap_or_else(|| panic!("a new service is shown its key once: {body}"))
        .to_owned()
}

/// Provision a player through `key` and hand back the response.
async fn provision(app: &Arc<App>, key: &str, external_id: &str) -> (StatusCode, Value) {
    post(
        app,
        Some(key),
        "/api/v1/economy/players",
        json!({ "external_id": external_id, "name": external_id }),
    )
    .await
}

// ---------------------------------------------------------------------------
// Issuing, listing and revoking.

#[tokio::test]
async fn a_service_key_is_shown_once_and_never_again() {
    let app = test_app();
    let key = issue(&app, "quests", &["reward"]).await;

    let (status, body) = get(&app, Some(OPERATOR), "/api/v1/economy/admin/services").await;
    assert_eq!(status, StatusCode::OK);
    let services = body["services"].as_array().unwrap();
    assert_eq!(services.len(), 1);
    assert_eq!(services[0]["name"], "quests");
    assert_eq!(services[0]["scopes"], json!(["reward"]));
    assert_eq!(services[0]["revoked"], json!(false));
    assert_eq!(
        services[0]["api_key"],
        Value::Null,
        "the key is shown in the response that issued it and nowhere else"
    );
    assert!(
        !body.to_string().contains(&key),
        "no listing of services carries a live credential: {body}"
    );
    assert!(
        !body.to_string().contains("digest"),
        "nor the digest it is kept as: {body}"
    );
}

#[tokio::test]
async fn only_the_operator_may_issue_a_service() {
    let app = test_app();
    // A service that carries every scope there is still cannot make another:
    // a credential that could would be an operator key with extra steps.
    let key = issue(
        &app,
        "everything",
        &["provision", "reward", "inventory", "events"],
    )
    .await;
    let (status, body) = post(
        &app,
        Some(&key),
        "/api/v1/economy/admin/services",
        json!({ "name": "wider", "scopes": ["provision"] }),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");

    let (status, _) = delete(&app, Some(&key), "/api/v1/economy/admin/services/1").await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "nor take another service's key away"
    );

    let (status, _) = get(&app, Some(&key), "/api/v1/economy/admin/services").await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "nor read the list of them"
    );
}

#[tokio::test]
async fn a_service_needs_a_name_and_at_least_one_scope() {
    let app = test_app();
    for body in [
        json!({ "name": "", "scopes": ["reward"] }),
        json!({ "name": "  ", "scopes": ["reward"] }),
        json!({ "name": "no scopes", "scopes": [] }),
    ] {
        let (status, answer) = post(
            &app,
            Some(OPERATOR),
            "/api/v1/economy/admin/services",
            body.clone(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body} → {answer}");
        assert_eq!(answer["error"]["code"], "invalid_service", "{answer}");
    }

    let (status, answer) = post(
        &app,
        Some(OPERATOR),
        "/api/v1/economy/admin/services",
        json!({ "name": "typo", "scopes": ["mint"] }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{answer}");
    assert!(
        answer["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no such scope"),
        "an unknown scope names itself: {answer}"
    );
}

#[tokio::test]
async fn a_revoked_key_is_refused_as_revoked() {
    let app = test_app();
    let key = issue(&app, "quests", &["reward"]).await;
    let (status, body) = provision(&app, &key, "p1").await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "the scope it does not carry is shut first: {body}"
    );
    assert_eq!(body["error"]["code"], "missing_scope");

    let (status, body) = delete(&app, Some(OPERATOR), "/api/v1/economy/admin/services/1").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["revoked"], json!(true));
    assert!(
        body["revoked_ms"].as_i64().is_some_and(|ms| ms > 0),
        "when it was revoked is wall-clock, as every other `_ms` on a record is: {body}"
    );

    let (status, body) = post(
        &app,
        Some(&key),
        "/api/v1/economy/rewards",
        json!({ "rule": "quest", "trader_id": 1, "source": "q1" }),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(
        body["error"]["code"], "revoked_api_key",
        "a key that was taken away says so, rather than pretending it was never a key: {body}"
    );

    // The service itself stays, so the journal entries that name it still
    // resolve to the thing that sent them.
    let (_, body) = get(&app, Some(OPERATOR), "/api/v1/economy/admin/services").await;
    assert_eq!(body["services"].as_array().unwrap().len(), 1);

    let (status, _) = delete(&app, Some(OPERATOR), "/api/v1/economy/admin/services/9").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "there is no service 9");
}

// ---------------------------------------------------------------------------
// A scope opens one thing and shuts the rest.

#[tokio::test]
async fn a_scope_opens_its_own_route_and_nothing_else() {
    let app = test_app();
    let provisioner = issue(&app, "accounts", &["provision"]).await;
    let rewarder = issue(&app, "quests", &["reward"]).await;
    let events = issue(&app, "world", &["events"]).await;

    // Provisioning: the provisioner's, nobody else's.
    let (status, body) = provision(&app, &provisioner, "player-1").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    for (key, who) in [(&rewarder, "reward"), (&events, "events")] {
        let (status, body) = provision(&app, key, "player-2").await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{who} cannot provision");
        assert_eq!(body["error"]["code"], "missing_scope");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("provision"),
            "the refusal names the scope that was needed: {body}"
        );
    }

    // Game events: the events service's.
    let event = json!({ "kind": "market_rally", "magnitude": 1.0, "source": "e1" });
    let (status, body) = post(&app, Some(&events), "/api/game/events", event.clone()).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let (status, _) = post(&app, Some(&provisioner), "/api/game/events", event).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Minting is not a scope at all, so no service reaches it.
    for key in [&provisioner, &rewarder, &events] {
        let (status, body) = post(
            &app,
            Some(key),
            "/api/accounts/1/deposit",
            json!({ "amount_cents": 1_000 }),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "no scope reaches the mint: {body}"
        );
    }
}

#[tokio::test]
async fn a_scope_reads_the_side_of_the_world_it_writes() {
    // A backend that just changed something must be able to see what it
    // did without holding a second key. Each read is open to the scopes
    // whose writes it is the read side of, and shut to the rest.
    let app = test_app();
    let provisioner = issue(&app, "accounts", &["provision"]).await;
    let rewarder = issue(&app, "quests", &["reward"]).await;
    let stock = issue(&app, "goods", &["inventory"]).await;
    let events = issue(&app, "world", &["events"]).await;

    let (status, player) = provision(&app, &provisioner, "player-1").await;
    assert_eq!(status, StatusCode::CREATED, "{player}");
    let trader = player["trader_id"].as_u64().unwrap();
    let wallet = player["wallet_id"].as_u64().unwrap();

    // Inventory: the inventory scope's, for any player.
    let uri = format!("/api/v1/economy/players/{trader}/inventory");
    let (status, body) = get(&app, Some(&stock), &uri).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["trader_id"], json!(trader));
    for (key, who) in [(&rewarder, "reward"), (&events, "events")] {
        let (status, body) = get(&app, Some(key), &uri).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{who} cannot read stock");
        assert_eq!(body["error"]["code"], "missing_scope");
    }

    // Budgets: the reward scope's.
    let (status, body) = get(&app, Some(&rewarder), "/api/budgets").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = get(&app, Some(&events), "/api/budgets").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"]["code"], "missing_scope");

    // Wallets: whatever moves money, and not the events scope, which does
    // not. The refusal names every scope that would have done.
    let wallet_uri = format!("/api/wallets/{wallet}");
    for key in [&provisioner, &rewarder, &stock] {
        let (status, body) = get(&app, Some(key), &wallet_uri).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["wallet"], json!(wallet));
        let (status, body) = get(&app, Some(key), &format!("{wallet_uri}/transactions")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let (status, body) = get(&app, Some(&events), &wallet_uri).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"]["code"], "missing_scope");
    let message = body["error"]["message"].as_str().unwrap();
    for scope in ["provision", "reward", "inventory"] {
        assert!(message.contains(scope), "names `{scope}`: {message}");
    }

    // The outbox: any service's, and journaled as that service when it
    // acknowledges. Still no player's.
    let (status, body) = get(&app, Some(&events), "/api/outbox").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = post(
        &app,
        Some(&events),
        "/api/outbox/ack",
        json!({ "through": 0 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = get(&app, player["api_key"].as_str(), "/api/outbox").await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "a player is not the backend"
    );

    // And the owner still reads their own, as before.
    let (status, body) = get(&app, player["api_key"].as_str(), &wallet_uri).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = get(&app, player["api_key"].as_str(), &uri).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn a_service_key_is_never_promoted_to_the_operators() {
    // The order of resolution is the whole safety property: a key the
    // registry knows is judged as that service, whatever else is configured.
    // An unlocked server is open to anyone *without* a service key — it must
    // not also be a way for a narrow one to widen itself.
    let app = App::new(Options {
        admin_key: None,
        ..options()
    });
    let (status, body) = post(
        &app,
        None,
        "/api/v1/economy/admin/services",
        json!({ "name": "quests", "scopes": ["reward"] }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "an unlocked server: {body}");
    let key = body["api_key"].as_str().unwrap().to_owned();

    let (status, body) = provision(&app, &key, "p1").await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a scope it does not carry is shut even here: {body}"
    );
    assert_eq!(body["error"]["code"], "missing_scope");

    // And the server is still open to a request that presents no service key
    // at all, which is what an unset `FEHU_ADMIN_KEY` is documented to mean.
    let (status, body) = provision(&app, "", "p1").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

#[tokio::test]
async fn the_operator_still_reaches_everything_a_scope_opens() {
    // A scope narrows a credential; it does not narrow the operator. A world
    // that never issues a service behaves exactly as it did before there
    // were any.
    let app = test_app();
    let (status, body) = provision(&app, OPERATOR, "player-1").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let (status, body) = post(
        &app,
        Some(OPERATOR),
        "/api/game/events",
        json!({ "kind": "market_rally", "magnitude": 1.0, "source": "e1" }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
}

// ---------------------------------------------------------------------------
// Provisioning.

#[tokio::test]
async fn provisioning_is_idempotent_on_the_games_own_id() {
    let app = test_app();
    let key = issue(&app, "accounts", &["provision"]).await;

    let (status, first) = provision(&app, &key, "steam:42").await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(first["external_id"], "steam:42");
    assert_eq!(first["created"], json!(true));
    let player_key = first["api_key"].as_str().expect("a new player gets a key");

    let (status, again) = provision(&app, &key, "steam:42").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a player who already has a mapping is not created twice"
    );
    assert_eq!(again["created"], json!(false));
    assert_eq!(again["user_id"], first["user_id"]);
    assert_eq!(again["account_id"], first["account_id"]);
    assert_eq!(again["trader_id"], first["trader_id"]);
    assert_eq!(again["wallet_id"], first["wallet_id"]);
    assert_eq!(
        again["api_key"],
        Value::Null,
        "the only copy of the key went out with the response that made it"
    );

    // The first key still works, so a repeat has not quietly rotated it.
    let (status, _) = get(
        &app,
        Some(player_key),
        &format!("/api/users/{}", first["user_id"].as_u64().unwrap()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // A different player is a different mapping.
    let (status, other) = provision(&app, &key, "steam:43").await;
    assert_eq!(status, StatusCode::CREATED);
    assert_ne!(other["trader_id"], first["trader_id"]);
}

#[tokio::test]
async fn a_provisioned_player_arrives_with_nothing() {
    // Arriving in the world creates no currency: minting is the operator's
    // and rewards come out of budgets. This is the ledger invariant the
    // whole economy rests on, checked at the one route that makes people.
    let app = test_app();
    let key = issue(&app, "accounts", &["provision"]).await;
    let (_, before) = get(&app, Some(OPERATOR), "/api/supply").await;

    let (status, player) = provision(&app, &key, "p1").await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, account) = get(
        &app,
        player["api_key"].as_str(),
        &format!("/api/accounts/{}", player["account_id"].as_u64().unwrap()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{account}");
    assert_eq!(account["balance_cents"], json!(0));

    let (_, after) = get(&app, Some(OPERATOR), "/api/supply").await;
    assert_eq!(
        after["outstanding_cents"], before["outstanding_cents"],
        "provisioning a player mints nothing"
    );
    assert_eq!(after["circulating_cents"], before["circulating_cents"]);
}

#[tokio::test]
async fn an_external_id_is_cleaned_or_refused() {
    let app = test_app();
    let key = issue(&app, "accounts", &["provision"]).await;

    let (status, body) = provision(&app, &key, "  steam:7  ").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["external_id"], "steam:7", "it is trimmed");
    let (status, body) = provision(&app, &key, "steam:7").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "and the trimmed id is the one it is keyed on: {body}"
    );

    for bad in ["", "   ", "with\nnewline"] {
        let (status, body) = provision(&app, &key, bad).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad:?} → {body}");
    }
    let long = "x".repeat(129);
    let (status, _) = provision(&app, &key, &long).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn the_roster_answers_whether_a_player_has_been_provisioned() {
    let app = test_app();
    let key = issue(&app, "accounts", &["provision"]).await;
    let rewarder = issue(&app, "quests", &["reward"]).await;
    provision(&app, &key, "a").await;
    provision(&app, &key, "b").await;

    let (status, body) = get(&app, Some(&key), "/api/v1/economy/players").await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<&str> = body["players"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["external_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["a", "b"]);

    let (_, body) = get(&app, Some(&key), "/api/v1/economy/players?external_id=b").await;
    assert_eq!(body["players"].as_array().unwrap().len(), 1);
    assert_eq!(body["players"][0]["external_id"], "b");

    let (status, body) = get(
        &app,
        Some(&key),
        "/api/v1/economy/players?external_id=nobody",
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "asking about a player nobody provisioned is a plain no, not a 404"
    );
    assert!(body["players"].as_array().unwrap().is_empty());

    let (status, _) = get(&app, Some(&rewarder), "/api/v1/economy/players").await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "the roster needs the scope that writes it"
    );
}

// ---------------------------------------------------------------------------
// A service is a principal: journaled, recoverable, and it survives a restart.

#[tokio::test]
async fn a_command_a_service_sent_is_journaled_as_that_service() {
    let app = test_app();
    let key = issue(&app, "accounts", &["provision"]).await;
    let other = issue(&app, "quests", &["reward"]).await;

    let req = Request::post("/api/v1/economy/players")
        .header(header::CONTENT_TYPE, "application/json")
        .header("Idempotency-Key", "prov-1");
    let (status, body) = call(
        &app,
        Some(&key),
        req.body(Body::from(json!({ "external_id": "p1" }).to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // The service that sent it can recover the response it lost.
    let (status, body) = get(&app, Some(&key), "/api/commands/prov-1").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["command"], "provision_player");
    assert!(
        !body.to_string().contains("api_key"),
        "and what is recorded never holds the credential: {body}"
    );

    // Another service cannot: a recovery is not a way to read the log.
    let (status, _) = get(&app, Some(&other), "/api/commands/prov-1").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // The operator can, as it can for anything.
    let (status, _) = get(&app, Some(OPERATOR), "/api/commands/prov-1").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn services_and_players_survive_a_restart() {
    let dir = tempdir_lite::TempDir::new("fehu-service-save");
    let state = dir.path().join("state.json");
    let key;
    let player;
    {
        let app = App::new(options_at(Some(state.clone())));
        key = issue(&app, "accounts", &["provision", "reward"]).await;
        let (_, body) = provision(&app, &key, "steam:1").await;
        player = body;
        save::write(&app, &state)
            .await
            .expect("a snapshot is written");
    }

    let saved = save::read(&state).expect("the snapshot reads back");
    let app = App::restore(options(), saved);

    // The same key still opens the same scopes.
    let (status, again) = provision(&app, &key, "steam:1").await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(
        again["created"],
        json!(false),
        "a restart must not forget a player it had mapped, or it would build them a second one"
    );
    assert_eq!(again["trader_id"], player["trader_id"]);
    assert_eq!(again["user_id"], player["user_id"]);

    // And a scope it never carried is still shut.
    let (status, body) = post(
        &app,
        Some(&key),
        "/api/game/events",
        json!({ "kind": "market_rally", "magnitude": 1.0, "source": "e1" }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // Revoking survives too.
    delete(&app, Some(OPERATOR), "/api/v1/economy/admin/services/1").await;
    save::write(&app, &state).await.unwrap();
    let app = App::restore(options(), save::read(&state).unwrap());
    let (status, body) = provision(&app, &key, "steam:2").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["error"]["code"], "revoked_api_key");
}

#[tokio::test]
async fn a_snapshot_holds_no_service_credential() {
    let dir = tempdir_lite::TempDir::new("fehu-service-creds");
    let state = dir.path().join("state.json");
    let app = App::new(options_at(Some(state.clone())));
    let key = issue(&app, "accounts", &["provision"]).await;
    let (_, player) = provision(&app, &key, "p1").await;
    let player_key = player["api_key"].as_str().unwrap();
    save::write(&app, &state).await.unwrap();

    let written = std::fs::read_to_string(&state).unwrap();
    assert!(
        !written.contains(&key),
        "a save holds the digest of a service key, never the key"
    );
    assert!(
        !written.contains(player_key),
        "nor a provisioned player's key"
    );
    assert!(written.contains("sha256:"), "it holds digests");
}

/// A temporary directory, as in the other suites that restart a server.
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
