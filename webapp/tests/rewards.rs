//! Budgets, rewards and transfers: currency moving for reasons outside the
//! market.
//!
//! A reward is the game saying something happened — a quest finished, a boss
//! killed — and the world paying for it. The question every test here asks is
//! the one the ledger exists to answer: did the currency come from somewhere.
//! A budget is a wallet, so the answer is always yes and an empty budget
//! refuses rather than prints.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use fehu_webapp::market::{App, Options};
use fehu_webapp::{router, save};
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
    post_keyed(app, key, uri, body, None).await
}

async fn post_keyed(
    app: &Arc<App>,
    key: Option<&str>,
    uri: &str,
    body: Value,
    idempotency: Option<&str>,
) -> (StatusCode, Value) {
    let mut req = Request::post(uri).header(header::CONTENT_TYPE, "application/json");
    if let Some(k) = idempotency {
        req = req.header("Idempotency-Key", k);
    }
    call(app, key, req.body(Body::from(body.to_string())).unwrap()).await
}

struct Player {
    id: u64,
    account_id: u64,
    key: String,
}

async fn sign_up(app: &Arc<App>, name: &str, cash_cents: i64) -> Player {
    let (status, body) = post(
        app,
        None,
        "/api/traders",
        json!({ "name": name, "cash_cents": cash_cents }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    Player {
        id: body["id"].as_u64().unwrap(),
        account_id: body["account_id"].as_u64().unwrap(),
        key: body["api_key"].as_str().unwrap().to_owned(),
    }
}

async fn supply(app: &Arc<App>) -> Value {
    let (status, body) = get(app, None, "/api/supply").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

async fn reconciles(app: &Arc<App>) {
    reconciles_as(app, None).await;
}

/// The audit is the operator's, so a world with an admin key set needs it.
async fn reconciles_as(app: &Arc<App>, key: Option<&str>) {
    let (status, body) = get(app, key, "/api/reconcile").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["valid"], true, "{body}");
}

async fn balance(app: &Arc<App>, player: &Player) -> i64 {
    let (status, body) = get(
        app,
        Some(&player.key),
        &format!("/api/accounts/{}", player.account_id),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["balance_cents"].as_i64().unwrap()
}

/// Open a budget with `cash_cents` in it and return its wallet id.
async fn open_budget(app: &Arc<App>, name: &str, cash_cents: i64) -> u64 {
    let (status, body) = post(
        app,
        None,
        "/api/budgets",
        json!({ "name": name, "cash_cents": cash_cents }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["wallet"].as_u64().unwrap()
}

/// Write a reward rule paying `amount_cents` out of `budget`.
async fn rule(app: &Arc<App>, id: &str, budget: u64, amount_cents: i64) -> Value {
    let (status, body) = post(
        app,
        None,
        "/api/rewards/rules",
        json!({ "id": id, "budget": budget, "amount_cents": amount_cents }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

async fn pay(app: &Arc<App>, rule: &str, player: &Player, source: &str) -> (StatusCode, Value) {
    post(
        app,
        None,
        "/api/rewards",
        json!({ "rule": rule, "trader_id": player.id, "source": source }),
    )
    .await
}

#[tokio::test]
async fn a_budget_holds_currency_that_already_existed() {
    let app = test_app();
    let before = supply(&app).await;
    let wallet = open_budget(&app, "quests", 250_000).await;

    let after = supply(&app).await;
    assert_eq!(
        after["outstanding_cents"], before["outstanding_cents"],
        "a budget is filled out of treasury, not minted"
    );
    assert_eq!(
        after["treasury_cents"].as_i64().unwrap(),
        before["treasury_cents"].as_i64().unwrap() - 250_000
    );
    assert_eq!(after["budget_cents"], 250_000);

    let (_, budgets) = get(&app, None, "/api/budgets").await;
    assert_eq!(budgets["budgets"][0]["balance_cents"], 250_000);
    assert_eq!(budgets["budgets"][0]["paid_count"], 0);

    // Topping one up is the same movement again.
    let (status, body) = post(
        &app,
        None,
        &format!("/api/budgets/{wallet}/fund"),
        json!({ "amount_cents": 50_000 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["balance_cents"], 300_000);
    assert_eq!(supply(&app).await["budget_cents"], 300_000);
    reconciles(&app).await;
}

#[tokio::test]
async fn a_budget_bigger_than_treasury_is_refused() {
    let app = App::new(Options {
        genesis_cents: 1_000_000,
        starting_cash_cents: 0,
        synthetic_float_cents: 0,
        ..options()
    });
    let (status, body) = post(
        &app,
        None,
        "/api/budgets",
        json!({ "name": "quests", "cash_cents": 2_000_000 }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "insufficient_funds", "{body}");
    assert_eq!(supply(&app).await["budget_cents"], 0);
    reconciles(&app).await;
}

#[tokio::test]
async fn a_reward_moves_currency_from_a_budget_to_a_player() {
    let app = test_app();
    let player = sign_up(&app, "quinn", 100_000).await;
    let budget = open_budget(&app, "quests", 250_000).await;
    rule(&app, "daily", budget, 5_000).await;
    let before = supply(&app).await;

    let (status, receipt) = pay(&app, "daily", &player, "login-2023-11-14").await;
    assert_eq!(status, StatusCode::CREATED, "{receipt}");
    assert_eq!(receipt["rule"], "daily");
    assert_eq!(receipt["amount_cents"], 5_000);
    assert_eq!(receipt["duplicate"], false);
    assert_eq!(receipt["balance_cents"], 105_000);
    assert_eq!(balance(&app, &player).await, 105_000);

    let after = supply(&app).await;
    assert_eq!(
        after["outstanding_cents"], before["outstanding_cents"],
        "a reward is paid, not printed"
    );
    assert_eq!(after["budget_cents"], 245_000);

    let (_, ledger) = get(
        &app,
        Some(&player.key),
        &format!("/api/accounts/{}/ledger", player.account_id),
    )
    .await;
    assert_eq!(ledger["entries"][0]["kind"], "reward");
    assert_eq!(ledger["entries"][0]["amount_cents"], 5_000);

    let (_, budgets) = get(&app, None, "/api/budgets").await;
    assert_eq!(budgets["budgets"][0]["paid_cents"], 5_000);
    assert_eq!(budgets["rules"][0]["paid_count"], 1);
    reconciles(&app).await;
}

#[tokio::test]
async fn the_same_thing_is_never_paid_for_twice() {
    let app = test_app();
    let player = sign_up(&app, "quinn", 0).await;
    let budget = open_budget(&app, "quests", 250_000).await;
    rule(&app, "boss", budget, 5_000).await;

    let (_, first) = pay(&app, "boss", &player, "kill-42").await;
    assert_eq!(first["duplicate"], false);

    // A second request, with its own idempotency key, for the same event.
    // The key protects a request; the source id protects the *event*.
    let (status, again) = post_keyed(
        &app,
        None,
        "/api/rewards",
        json!({ "rule": "boss", "trader_id": player.id, "source": "kill-42" }),
        Some("a-different-key"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again["duplicate"], true);
    assert_eq!(again["tx_id"], first["tx_id"], "the first receipt, again");
    assert_eq!(balance(&app, &player).await, 5_000, "paid once");
    assert_eq!(supply(&app).await["budget_cents"], 245_000);

    // A different kill is a different event.
    let (_, other) = pay(&app, "boss", &player, "kill-43").await;
    assert_eq!(other["duplicate"], false);
    assert_eq!(balance(&app, &player).await, 10_000);
    reconciles(&app).await;
}

#[tokio::test]
async fn an_empty_budget_refuses_the_reward_rather_than_printing_it() {
    let app = test_app();
    let player = sign_up(&app, "quinn", 0).await;
    let budget = open_budget(&app, "quests", 8_000).await;
    rule(&app, "boss", budget, 5_000).await;

    let (status, body) = pay(&app, "boss", &player, "kill-1").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = pay(&app, "boss", &player, "kill-2").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "budget_exhausted", "{body}");
    assert_eq!(balance(&app, &player).await, 5_000);
    assert_eq!(supply(&app).await["budget_cents"], 3_000);

    // A refusal remembers nothing, so the same event pays once the budget is
    // topped up.
    post(
        &app,
        None,
        &format!("/api/budgets/{budget}/fund"),
        json!({ "amount_cents": 10_000 }),
    )
    .await;
    let (status, body) = pay(&app, "boss", &player, "kill-2").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(balance(&app, &player).await, 10_000);
    reconciles(&app).await;
}

#[tokio::test]
async fn a_rule_needs_a_budget_and_a_reward_needs_a_rule() {
    let app = test_app();
    let player = sign_up(&app, "quinn", 0).await;

    let (status, body) = post(
        &app,
        None,
        "/api/rewards/rules",
        json!({ "id": "daily", "budget": 9_999, "amount_cents": 100 }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"]["code"], "unknown_budget", "{body}");

    let (status, body) = pay(&app, "daily", &player, "login-1").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"]["code"], "unknown_reward_rule", "{body}");

    let budget = open_budget(&app, "quests", 10_000).await;
    let (status, body) = post(
        &app,
        None,
        "/api/rewards/rules",
        json!({ "id": "daily", "budget": budget, "amount_cents": 0 }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a rule that pays nothing is not a rule: {body}"
    );
    reconciles(&app).await;
}

#[tokio::test]
async fn a_reward_is_the_games_to_pay_and_not_a_players() {
    let app = App::new(Options {
        admin_key: Some("operator-key".into()),
        ..options()
    });
    let player = sign_up(&app, "quinn", 0).await;
    let (status, body) = post(
        &app,
        Some("operator-key"),
        "/api/budgets",
        json!({ "name": "quests", "cash_cents": 100_000 }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let budget = body["wallet"].as_u64().unwrap();
    post(
        &app,
        Some("operator-key"),
        "/api/rewards/rules",
        json!({ "id": "daily", "budget": budget, "amount_cents": 5_000 }),
    )
    .await;

    // The player's own key pays for nothing, including for themselves.
    let (status, body) = post(
        &app,
        Some(&player.key),
        "/api/rewards",
        json!({ "rule": "daily", "trader_id": player.id, "source": "login-1" }),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(balance(&app, &player).await, 0);

    let (status, body) = post(
        &app,
        Some("operator-key"),
        "/api/rewards",
        json!({ "rule": "daily", "trader_id": player.id, "source": "login-1" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(balance(&app, &player).await, 5_000);
    reconciles_as(&app, Some("operator-key")).await;
}

#[tokio::test]
async fn a_transfer_moves_currency_between_players_and_makes_none() {
    // With an admin key set, an ordinary caller is not the operator, which
    // is what makes "somebody else's account" mean anything.
    let app = App::new(Options {
        admin_key: Some("operator-key".into()),
        ..options()
    });
    let alice = sign_up(&app, "alice", 100_000).await;
    let bob = sign_up(&app, "bob", 1_000).await;
    let before = supply(&app).await;

    let (status, body) = post(
        &app,
        Some(&alice.key),
        "/api/transfers",
        json!({
            "from_account_id": alice.account_id,
            "to_account_id": bob.account_id,
            "amount_cents": 25_000,
            "memo": "for the ore",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["sent"]["kind"], "transfer_out");
    assert_eq!(body["sent"]["amount_cents"], -25_000);
    assert_eq!(body["received"]["kind"], "transfer_in");
    assert_eq!(balance(&app, &alice).await, 75_000);
    assert_eq!(balance(&app, &bob).await, 26_000);
    assert_eq!(
        supply(&app).await["outstanding_cents"],
        before["outstanding_cents"]
    );

    // Only the sender's owner may send it.
    let (status, body) = post(
        &app,
        Some(&bob.key),
        "/api/transfers",
        json!({
            "from_account_id": alice.account_id,
            "to_account_id": bob.account_id,
            "amount_cents": 1_000,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // More than is there, and to yourself, are both refused.
    let (status, body) = post(
        &app,
        Some(&alice.key),
        "/api/transfers",
        json!({
            "from_account_id": alice.account_id,
            "to_account_id": bob.account_id,
            "amount_cents": 10_000_000,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "insufficient_funds", "{body}");
    let (status, body) = post(
        &app,
        Some(&alice.key),
        "/api/transfers",
        json!({
            "from_account_id": alice.account_id,
            "to_account_id": alice.account_id,
            "amount_cents": 1_000,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(balance(&app, &alice).await, 75_000);
    reconciles_as(&app, Some("operator-key")).await;
}

#[tokio::test]
async fn a_frozen_account_is_paid_but_pays_nobody() {
    let app = test_app();
    let player = sign_up(&app, "quinn", 50_000).await;
    let other = sign_up(&app, "rae", 0).await;
    let budget = open_budget(&app, "quests", 100_000).await;
    rule(&app, "daily", budget, 5_000).await;

    let (status, body) = post(
        &app,
        None,
        &format!("/api/accounts/{}/status", player.account_id),
        json!({ "status": "frozen" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // A frozen wallet takes credits: the world can still pay what it owes.
    let (status, body) = pay(&app, "daily", &player, "login-1").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(balance(&app, &player).await, 55_000);

    // It sends nothing.
    let (status, body) = post(
        &app,
        Some(&player.key),
        "/api/transfers",
        json!({
            "from_account_id": player.account_id,
            "to_account_id": other.account_id,
            "amount_cents": 1_000,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "account_not_active", "{body}");
    assert_eq!(balance(&app, &player).await, 55_000);
    reconciles(&app).await;
}

#[tokio::test]
async fn a_world_that_has_paid_rewards_comes_back_remembering_them() {
    let dir = tempdir_lite::TempDir::new("fehu-rewards");
    let path = dir.path().join("state.json");
    let options = || Options {
        state_file: Some(path.clone()),
        ..options()
    };
    let before = App::new(options());
    let player = sign_up(&before, "quinn", 0).await;
    let budget = open_budget(&before, "quests", 100_000).await;
    rule(&before, "boss", budget, 5_000).await;
    pay(&before, "boss", &player, "kill-42").await;
    let (_, budgets) = get(&before, None, "/api/budgets").await;

    save::write(&before, &path).await.expect("state written");
    let after = App::restore(options(), save::read(&path).unwrap());

    let (_, restored) = get(&after, None, "/api/budgets").await;
    assert_eq!(restored, budgets, "the budgets and the rules");
    assert_eq!(balance(&after, &player).await, 5_000);

    // And the kill that was already paid for is still paid for.
    let (status, again) = pay(&after, "boss", &player, "kill-42").await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again["duplicate"], true);
    assert_eq!(balance(&after, &player).await, 5_000);
    reconciles(&after).await;
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
