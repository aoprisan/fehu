//! Milestone 4's acceptance: the scenario in `docs/economy-engine-plan.md`.
//!
//! > Initialise a world with a genesis supply in treasury. Onboard two
//! > players. Pay one a quest reward from a budget wallet. That player buys
//! > ore from an NPC merchant, starts a smelting job, waits for the tick that
//! > completes it, lists the ingot, and the second player buys it. The venue
//! > takes its fee. The second player consumes the ingot.
//!
//! After every step three questions are asked, and they are the same three
//! the earlier milestones ask:
//!
//! * **does the currency add up** — every wallet in the world, added up, is
//!   the genesis supply, and nothing but an operator's mint moves that
//!   number;
//! * **is every unit somewhere** — ore and ingot counts are exactly what has
//!   been issued less what has been consumed, and `/api/reconcile` says so;
//! * **is anything held for nothing** — no reservation without a resting
//!   order behind it, which the same audit checks. A job holds nothing at
//!   all: it takes its inputs and its cost when it starts.
//!
//! And then the whole scenario is run again with every request retried under
//! its original `Idempotency-Key`, and again with the process stopped and
//! restarted at every journal boundary it passed through.
//!
//! The negative cases the plan lists are the tests after it: an empty budget,
//! a merchant out of stock, a recipient at the balance cap, a freeze while an
//! order is resting, and a job completion that must not be delivered twice.

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use fehu::Timestamp;
use fehu_economy::journal::{self, JournalEntry};
use fehu_economy::market::{App, Options};
use fehu_economy::{engine, router, save};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::util::ServiceExt;

/// 2023-11-14T22:13:20Z, as in the other suites.
const NOW_MS: i64 = 1_700_000_000_000;

/// The world's whole supply. Every assertion about conservation is against
/// this number.
const GENESIS_CENTS: i64 = 1_000_000_000;

fn options(state_file: Option<PathBuf>) -> Options {
    Options {
        history_days: 0,
        warmup_hours: 0,
        now_ms: Some(NOW_MS),
        rate_per_sec: 0.0,
        genesis_cents: GENESIS_CENTS,
        starting_cash_cents: 0,
        // Every fill has a funded counterparty: that is what milestone 3
        // built and what this scenario is played on.
        synthetic: false,
        synthetic_float_cents: 0,
        issuer_float_cents: 0,
        // The venue takes its cut, as the scenario says it does.
        taker_fee_bps: 50,
        state_file,
        ..Options::default()
    }
}

fn test_app() -> Arc<App> {
    App::new(options(None))
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
        .get("Fehu-Journal-Seq")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok());
    let replayed = resp
        .headers()
        .get("Fehu-Idempotent-Replay")
        .is_some_and(|v| v == "true");
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

async fn get(app: &Arc<App>, key: Option<&str>, uri: &str) -> Value {
    let r = call(app, key, Request::get(uri).body(Body::empty()).unwrap()).await;
    assert!(
        r.status.is_success(),
        "GET {uri} → {}: {}",
        r.status,
        r.body
    );
    r.body
}

async fn post(
    app: &Arc<App>,
    key: Option<&str>,
    idempotency: Option<&str>,
    uri: &str,
    body: Value,
) -> Response {
    let mut req = Request::post(uri).header(header::CONTENT_TYPE, "application/json");
    if let Some(k) = idempotency {
        req = req.header("Idempotency-Key", k);
    }
    call(app, key, req.body(Body::from(body.to_string())).unwrap()).await
}

/// One request the scenario sent, kept so it can be sent again exactly as
/// it was. The journal holds the *command*, which is not the same thing as
/// the HTTP that carried it — the route, the credential and the body a
/// handler will accept are all here and nowhere else.
struct Sent {
    key: String,
    uri: String,
    body: Value,
    api_key: Option<String>,
}

/// The scenario as it runs: what it sent, and where each mutation landed in
/// the journal.
#[derive(Default)]
struct Script {
    marks: Vec<u64>,
    sent: Vec<Sent>,
}

impl Script {
    /// Send a mutation that has to succeed, and write it down.
    async fn ok(
        &mut self,
        app: &Arc<App>,
        api_key: Option<&str>,
        idempotency: &str,
        uri: &str,
        body: Value,
    ) -> Response {
        let r = ok(app, api_key, idempotency, uri, body.clone()).await;
        if let Some(seq) = r.seq {
            self.marks.push(seq);
        }
        self.sent.push(Sent {
            key: idempotency.to_owned(),
            uri: uri.to_owned(),
            body,
            api_key: api_key.map(str::to_owned),
        });
        r
    }
}

/// A mutation that has to succeed, sent under its own idempotency key.
async fn ok(
    app: &Arc<App>,
    key: Option<&str>,
    idempotency: &str,
    uri: &str,
    body: Value,
) -> Response {
    let r = post(app, key, Some(idempotency), uri, body).await;
    assert!(
        r.status.is_success(),
        "POST {uri} → {}: {}",
        r.status,
        r.body
    );
    r
}

struct Player {
    trader_id: u64,
    account_id: u64,
    key: String,
}

async fn sign_up(app: &Arc<App>, idempotency: &str, name: &str) -> Player {
    sign_up_recorded(&mut Script::default(), app, idempotency, name).await
}

/// [`sign_up`], written into the script so the retry pass can send it again.
async fn sign_up_recorded(
    script: &mut Script,
    app: &Arc<App>,
    idempotency: &str,
    name: &str,
) -> Player {
    let r = script
        .ok(
            app,
            None,
            idempotency,
            "/api/traders",
            json!({ "name": name, "cash_cents": 0 }),
        )
        .await;
    Player {
        trader_id: r.body["id"].as_u64().unwrap(),
        account_id: r.body["account_id"].as_u64().unwrap(),
        key: r.body["api_key"].as_str().unwrap().to_owned(),
    }
}

/// The three questions, asked of the world as it stands.
async fn audit(app: &Arc<App>, what: &str) {
    let supply = get(app, None, "/api/supply").await;
    assert_eq!(supply["balanced"], true, "after {what}: {supply}");
    assert_eq!(
        supply["outstanding_cents"], GENESIS_CENTS,
        "after {what}, the world holds what it was born with: {supply}"
    );
    assert_eq!(
        supply["circulating_cents"], GENESIS_CENTS,
        "after {what}, every wallet added up is that same number: {supply}"
    );
    assert_eq!(
        supply["synthetic_debt_cents"], 0,
        "after {what}, nobody was paid by nobody: {supply}"
    );
    let report = get(app, None, "/api/reconcile").await;
    assert_eq!(report["valid"], true, "after {what}: {report}");
    assert!(
        report["issues"].as_array().unwrap().is_empty(),
        "after {what}: {report}"
    );
}

/// Units of `symbol` issued, consumed and held, from the listing itself.
async fn units(app: &Arc<App>, symbol: &str) -> (u64, u64) {
    let detail = get(app, None, &format!("/api/symbols/{symbol}")).await;
    (
        detail["info"]["asset"]["issued"].as_u64().unwrap(),
        detail["info"]["asset"]["consumed"].as_u64().unwrap(),
    )
}

async fn balance(app: &Arc<App>, player: &Player) -> i64 {
    get(
        app,
        Some(&player.key),
        &format!("/api/accounts/{}", player.account_id),
    )
    .await["balance_cents"]
        .as_i64()
        .unwrap()
}

async fn holding(app: &Arc<App>, player: &Player, symbol: &str) -> u64 {
    let inventory = get(
        app,
        Some(&player.key),
        &format!("/api/v1/economy/players/{}/inventory", player.trader_id),
    )
    .await;
    inventory["inventory"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["symbol"] == symbol)
        .map_or(0, |h| h["qty"].as_u64().unwrap())
}

/// Everything the scenario does, in order, each step under its own key.
///
/// Returns the journal sequences it passed through, so the same run can be
/// stopped and restarted at each of them.
async fn play(app: &Arc<App>) -> Script {
    let mut script = Script::default();

    // A world with two goods and a merchant behind one of them.
    script
        .ok(
            app,
            None,
            "list-ore",
            "/api/symbols",
            json!({
                "symbol": "ORE", "kind": "good", "unit": "kg",
                "name": "Iron Ore", "start_price_cents": 200,
            }),
        )
        .await;
    script
        .ok(
            app,
            None,
            "list-ingot",
            "/api/symbols",
            json!({
                "symbol": "INGOT", "kind": "good", "unit": "bar",
                "name": "Iron Ingot", "start_price_cents": 900,
            }),
        )
        .await;
    script
        .ok(
            app,
            None,
            "merchant",
            "/api/npcs",
            json!({
                "symbol": "ORE", "name": "Ferrous Trading",
                "cash_cents": 10_000_000, "inventory": 1_000,
                "levels": 1, "size": 200, "half_spread_bps": 100,
            }),
        )
        .await;
    script
        .ok(
            app,
            None,
            "recipe",
            "/api/recipes",
            json!({
                "id": "smelt",
                "inputs": [{ "symbol": "ORE", "qty": 10 }],
                "outputs": [{ "symbol": "INGOT", "qty": 1 }],
                "cost_cents": 2_000,
                "duration_secs": 300,
            }),
        )
        .await;
    audit(app, "the world was made").await;

    // Two players, onboarded with nothing: currency reaches them by being
    // paid, not by being conjured at sign-up.
    let smith = sign_up_recorded(&mut script, app, "player-smith", "smith").await;
    let buyer = sign_up_recorded(&mut script, app, "player-buyer", "buyer").await;
    audit(app, "two players signed up").await;
    assert_eq!(balance(app, &smith).await, 0);

    // A quest budget, and the reward that comes out of it.
    let r = script
        .ok(
            app,
            None,
            "budget",
            "/api/budgets",
            json!({ "name": "quests", "cash_cents": 5_000_000 }),
        )
        .await;
    let budget = r.body["wallet"].as_u64().unwrap();
    script
        .ok(
            app,
            None,
            "rule",
            "/api/rewards/rules",
            json!({ "id": "first-forge", "budget": budget, "amount_cents": 500_000 }),
        )
        .await;
    script
        .ok(
            app,
            None,
            "reward",
            "/api/v1/economy/rewards",
            json!({
                "rule": "first-forge",
                "trader_id": smith.trader_id,
                "source": "quest:first-forge:smith",
            }),
        )
        .await;
    assert_eq!(balance(app, &smith).await, 500_000);
    audit(app, "a quest was paid for").await;

    // The merchant puts its ore on the book, and the smith buys ten of it.
    engine::advance_to(app, Timestamp(NOW_MS + 1_000)).await;
    let ask = get(app, None, "/api/symbols/ORE/book").await["asks"][0]["price_cents"]
        .as_i64()
        .expect("the merchant is quoting");
    let r = script
        .ok(
            app,
            Some(&smith.key),
            "buy-ore",
            "/api/symbols/ORE/orders",
            json!({
                "trader_id": smith.trader_id, "side": "buy", "qty": 10,
                "type": "limit", "price_cents": ask,
            }),
        )
        .await;
    assert_eq!(r.body["status"], "filled", "{}", r.body);
    assert_eq!(holding(app, &smith, "ORE").await, 10);
    audit(app, "the smith bought ore from a merchant").await;

    // Into the furnace. The ore is consumed and the cost is paid now.
    let r = script
        .ok(
            app,
            Some(&smith.key),
            "job",
            "/api/v1/economy/jobs",
            json!({ "trader_id": smith.trader_id, "recipe": "smelt" }),
        )
        .await;
    let job_id = r.body["id"].as_u64().unwrap();
    assert_eq!(holding(app, &smith, "ORE").await, 0, "all ten went in");
    assert_eq!(units(app, "ORE").await.1, 10, "and were consumed");
    audit(app, "a job was started").await;

    // Wait for the tick that completes it.
    engine::advance_to(app, Timestamp(NOW_MS + 301_000)).await;
    let job = get(
        app,
        Some(&smith.key),
        &format!("/api/v1/economy/jobs/{job_id}"),
    )
    .await;
    assert_eq!(job["status"], "done", "{job}");
    assert_eq!(holding(app, &smith, "INGOT").await, 1);
    assert_eq!(units(app, "INGOT").await, (1, 0));
    audit(app, "the job delivered").await;

    // The smith lists the ingot; the second player buys it, and the venue
    // takes its cut of the fill.
    let r = script
        .ok(
            app,
            Some(&smith.key),
            "sell-ingot",
            "/api/symbols/INGOT/orders",
            json!({
                "trader_id": smith.trader_id, "side": "sell", "qty": 1,
                "type": "limit", "price_cents": 100_000,
            }),
        )
        .await;
    assert_eq!(r.body["status"], "resting", "{}", r.body);
    audit(app, "the ingot was offered").await;

    script
        .ok(
            app,
            None,
            "fund-buyer",
            "/api/rewards/rules",
            json!({ "id": "welcome", "budget": budget, "amount_cents": 200_000 }),
        )
        .await;
    script
        .ok(
            app,
            None,
            "pay-buyer",
            "/api/rewards",
            json!({
                "rule": "welcome",
                "trader_id": buyer.trader_id,
                "source": "quest:welcome:buyer",
            }),
        )
        .await;

    let venue_before = get(app, None, "/api/supply").await["venue_cents"]
        .as_i64()
        .unwrap();
    let r = script
        .ok(
            app,
            Some(&buyer.key),
            "buy-ingot",
            "/api/symbols/INGOT/orders",
            json!({
                "trader_id": buyer.trader_id, "side": "buy", "qty": 1,
                "type": "limit", "price_cents": 100_000,
            }),
        )
        .await;
    assert_eq!(r.body["status"], "filled", "{}", r.body);
    assert_eq!(holding(app, &buyer, "INGOT").await, 1);
    let venue_after = get(app, None, "/api/supply").await["venue_cents"]
        .as_i64()
        .unwrap();
    assert_eq!(
        venue_after - venue_before,
        500,
        "50 bp of a 1000.00 fill, and it stays in the world"
    );
    audit(app, "the ingot changed hands").await;

    // And it is used up: the units leave, no currency moves.
    let before = get(app, None, "/api/supply").await;
    script
        .ok(
            app,
            Some(&buyer.key),
            "consume-ingot",
            "/api/v1/economy/consume",
            json!({ "trader_id": buyer.trader_id, "symbol": "INGOT", "qty": 1 }),
        )
        .await;
    assert_eq!(units(app, "INGOT").await, (1, 1), "issued once, used once");
    assert_eq!(holding(app, &buyer, "INGOT").await, 0);
    assert_eq!(
        get(app, None, "/api/supply").await["circulating_cents"],
        before["circulating_cents"],
        "a thing used up is not a thing sold"
    );
    audit(app, "the ingot was consumed").await;

    script
}

#[tokio::test]
async fn the_acceptance_scenario() {
    let app = test_app();
    play(&app).await;
}

/// The audit for a world an operator has minted into: the sum still has to
/// add up, it is simply no longer the genesis number.
async fn audit_balanced(app: &Arc<App>, what: &str) {
    let supply = get(app, None, "/api/supply").await;
    assert_eq!(supply["balanced"], true, "after {what}: {supply}");
    assert_eq!(
        supply["circulating_cents"], supply["outstanding_cents"],
        "after {what}: {supply}"
    );
    assert_eq!(supply["synthetic_debt_cents"], 0, "after {what}: {supply}");
    let report = get(app, None, "/api/reconcile").await;
    assert_eq!(report["valid"], true, "after {what}: {report}");
}

#[tokio::test]
async fn replaying_every_request_under_its_key_changes_nothing() {
    let app = test_app();
    let script = play(&app).await;

    let before = get(&app, None, "/api/supply").await;
    let ore = units(&app, "ORE").await;
    let ingot = units(&app, "INGOT").await;

    // Every mutation the scenario made, sent again exactly as it was sent:
    // same route, same body, same credential, same key.
    for sent in &script.sent {
        let r = post(
            &app,
            sent.api_key.as_deref(),
            Some(&sent.key),
            &sent.uri,
            sent.body.clone(),
        )
        .await;
        assert!(
            r.status.is_success(),
            "retry of {} → {}: {}",
            sent.key,
            r.status,
            r.body
        );
        assert!(
            r.replayed,
            "retry of {} was applied again rather than answered: {}",
            sent.key, r.body
        );
    }
    assert!(
        script.sent.len() >= 12,
        "the scenario made {} keyed mutations",
        script.sent.len()
    );

    assert_eq!(
        get(&app, None, "/api/supply").await,
        before,
        "not a cent moved for any of it"
    );
    assert_eq!(units(&app, "ORE").await, ore);
    assert_eq!(units(&app, "INGOT").await, ingot);
    audit(&app, "every request was replayed").await;
}

#[tokio::test]
async fn stopping_at_every_journal_boundary_and_starting_again() {
    let dir = tempdir_lite::TempDir::new("fehu-m4-restart");
    let path = dir.path().join("state.json");
    let app = App::new(options(Some(path.clone())));
    app.attach_journal(&journal::path_for(&path))
        .await
        .expect("a journal");
    let script = play(&app).await;
    let entries = journal::read(&journal::path_for(&path)).expect("a readable journal");
    drop(app);

    // Every acknowledged point in the scenario, rebuilt from the world as it
    // was before the first command plus the entries up to that point. There
    // is no snapshot in between: what is being tested is that the journal
    // alone gets back to a world that adds up.
    assert!(
        script.marks.len() >= 12,
        "{} boundaries",
        script.marks.len()
    );
    for seq in &script.marks {
        let prefix: Vec<JournalEntry> = entries.iter().filter(|e| e.seq <= *seq).cloned().collect();
        let app = App::resume(options(Some(path.clone())), fresh_save().await, prefix).await;
        audit(&app, &format!("restarting at sequence {seq}")).await;
    }

    // And the whole journal, which is the world as it was left.
    let app = App::resume(options(Some(path.clone())), fresh_save().await, entries).await;
    audit(&app, "restarting at the end").await;
    assert_eq!(
        units(&app, "ORE").await,
        (1_000, 10),
        "the merchant's endowment, ten of which were smelted"
    );
    assert_eq!(units(&app, "INGOT").await, (1, 1));
}

/// The world as it was before the first command: the genesis supply in
/// treasury and the seeded listings, and nothing the scenario did.
///
/// Replay starts from a checkpoint, and this is the only honest one for a
/// journal with no snapshot behind it — the state a freshly started server
/// has. Everything the scenario did is in the entries.
async fn fresh_save() -> save::Save {
    App::new(options(None)).save().await
}

// ---------------------------------------------------------------------------
// The negative cases the plan lists.

#[tokio::test]
async fn an_empty_budget_pays_nothing_and_breaks_nothing() {
    let app = test_app();
    let player = sign_up(&app, "p1", "quinn").await;
    let budget = ok(
        &app,
        None,
        "budget",
        "/api/budgets",
        json!({ "name": "quests", "cash_cents": 1_000 }),
    )
    .await
    .body["wallet"]
        .as_u64()
        .unwrap();
    ok(
        &app,
        None,
        "rule",
        "/api/rewards/rules",
        json!({ "id": "daily", "budget": budget, "amount_cents": 5_000 }),
    )
    .await;

    let r = post(
        &app,
        None,
        Some("pay"),
        "/api/rewards",
        json!({ "rule": "daily", "trader_id": player.trader_id, "source": "login-1" }),
    )
    .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", r.body);
    assert_eq!(r.body["error"]["code"], "budget_exhausted", "{}", r.body);
    assert_eq!(balance(&app, &player).await, 0);
    audit(&app, "a budget ran out").await;
}

#[tokio::test]
async fn a_merchant_out_of_stock_simply_has_no_ask() {
    let app = test_app();
    ok(
        &app,
        None,
        "list-ore",
        "/api/symbols",
        json!({
            "symbol": "ORE", "kind": "good", "unit": "kg",
            "name": "Iron Ore", "start_price_cents": 200,
        }),
    )
    .await;
    ok(
        &app,
        None,
        "merchant",
        "/api/npcs",
        json!({
            "symbol": "ORE", "name": "Ferrous Trading",
            "cash_cents": 10_000_000, "inventory": 5,
            "levels": 1, "size": 5, "half_spread_bps": 100,
        }),
    )
    .await;
    let player = sign_up(&app, "p1", "quinn").await;
    let budget = ok(
        &app,
        None,
        "budget",
        "/api/budgets",
        json!({ "name": "quests", "cash_cents": 5_000_000 }),
    )
    .await
    .body["wallet"]
        .as_u64()
        .unwrap();
    ok(
        &app,
        None,
        "rule",
        "/api/rewards/rules",
        json!({ "id": "daily", "budget": budget, "amount_cents": 1_000_000 }),
    )
    .await;
    ok(
        &app,
        None,
        "pay",
        "/api/rewards",
        json!({ "rule": "daily", "trader_id": player.trader_id, "source": "login-1" }),
    )
    .await;

    engine::advance_to(&app, Timestamp(NOW_MS + 1_000)).await;
    let ask = get(&app, None, "/api/symbols/ORE/book").await["asks"][0]["price_cents"]
        .as_i64()
        .expect("the merchant is quoting");
    let r = ok(
        &app,
        Some(&player.key),
        "buy-the-lot",
        "/api/symbols/ORE/orders",
        json!({
            "trader_id": player.trader_id, "side": "buy", "qty": 5,
            "type": "limit", "price_cents": ask,
        }),
    )
    .await;
    assert_eq!(r.body["status"], "filled", "{}", r.body);
    audit(&app, "the merchant sold its last unit").await;

    // It has nothing left to sell, so it quotes nothing to sell.
    engine::advance_to(&app, Timestamp(NOW_MS + 2_000)).await;
    let book = get(&app, None, "/api/symbols/ORE/book").await;
    assert!(
        book["asks"].as_array().unwrap().is_empty(),
        "scarcity is visible in the book: {book}"
    );
    assert!(
        !book["bids"].as_array().unwrap().is_empty(),
        "and it still buys: {book}"
    );
    let npcs = get(&app, None, "/api/npcs").await;
    assert_eq!(npcs["npcs"][0]["inventory"], 0);
    audit(&app, "a merchant ran out").await;
}

#[tokio::test]
async fn a_recipient_at_the_balance_cap_is_refused_rather_than_clipped() {
    let app = test_app();
    let player = sign_up(&app, "p1", "quinn").await;
    // Mint the account up to the cap. Operator authority, and the only
    // thing in the whole server that changes the supply.
    let cap = fehu_economy::account::MAX_BALANCE_CENTS;
    let r = post(
        &app,
        None,
        Some("mint"),
        &format!("/api/accounts/{}/deposit", player.account_id),
        json!({ "amount_cents": cap }),
    )
    .await;
    assert!(r.status.is_success(), "{}", r.body);

    let budget = ok(
        &app,
        None,
        "budget",
        "/api/budgets",
        json!({ "name": "quests", "cash_cents": 5_000 }),
    )
    .await
    .body["wallet"]
        .as_u64()
        .unwrap();
    ok(
        &app,
        None,
        "rule",
        "/api/rewards/rules",
        json!({ "id": "daily", "budget": budget, "amount_cents": 1_000 }),
    )
    .await;

    let r = post(
        &app,
        None,
        Some("pay"),
        "/api/rewards",
        json!({ "rule": "daily", "trader_id": player.trader_id, "source": "login-1" }),
    )
    .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", r.body);
    assert_eq!(r.body["error"]["code"], "balance_cap", "{}", r.body);
    assert_eq!(
        balance(&app, &player).await,
        cap,
        "refused whole, not paid in part"
    );
    let supply = get(&app, None, "/api/supply").await;
    assert_eq!(supply["balanced"], true, "{supply}");
    assert_eq!(supply["budget_cents"], 5_000, "the budget kept its money");
}

#[tokio::test]
async fn a_freeze_while_an_order_is_resting_gives_the_reservation_back() {
    let app = test_app();
    ok(
        &app,
        None,
        "list-ore",
        "/api/symbols",
        json!({
            "symbol": "ORE", "kind": "good", "unit": "kg",
            "name": "Iron Ore", "start_price_cents": 200,
        }),
    )
    .await;
    let player = sign_up(&app, "p1", "quinn").await;
    let r = post(
        &app,
        None,
        Some("mint"),
        &format!("/api/accounts/{}/deposit", player.account_id),
        json!({ "amount_cents": 1_000_000 }),
    )
    .await;
    assert!(r.status.is_success(), "{}", r.body);

    let r = ok(
        &app,
        Some(&player.key),
        "bid",
        "/api/symbols/ORE/orders",
        json!({
            "trader_id": player.trader_id, "side": "buy", "qty": 100,
            "type": "limit", "price_cents": 150,
        }),
    )
    .await;
    assert_eq!(r.body["status"], "resting", "{}", r.body);
    let account = get(
        &app,
        Some(&player.key),
        &format!("/api/accounts/{}", player.account_id),
    )
    .await;
    assert_eq!(account["reserved_cents"], 15_000, "{account}");

    let r = post(
        &app,
        None,
        Some("freeze"),
        &format!("/api/accounts/{}/status", player.account_id),
        json!({ "status": "frozen" }),
    )
    .await;
    assert!(r.status.is_success(), "{}", r.body);

    let account = get(
        &app,
        Some(&player.key),
        &format!("/api/accounts/{}", player.account_id),
    )
    .await;
    assert_eq!(
        account["reserved_cents"], 0,
        "a freeze cancels what was resting and gives back what it held: {account}"
    );
    let book = get(&app, None, "/api/symbols/ORE/book").await;
    assert!(book["bids"].as_array().unwrap().is_empty(), "{book}");
    audit_balanced(&app, "an account was frozen mid-order").await;
}

#[tokio::test]
async fn a_job_is_delivered_once_however_many_steps_pass_over_it() {
    let dir = tempdir_lite::TempDir::new("fehu-m4-once");
    let path = dir.path().join("state.json");
    let app = App::new(options(Some(path.clone())));
    app.attach_journal(&journal::path_for(&path))
        .await
        .expect("a journal");
    ok(
        &app,
        None,
        "list-ore",
        "/api/symbols",
        json!({ "symbol": "ORE", "kind": "good", "unit": "kg", "start_price_cents": 200 }),
    )
    .await;
    ok(
        &app,
        None,
        "list-ingot",
        "/api/symbols",
        json!({ "symbol": "INGOT", "kind": "good", "unit": "bar", "start_price_cents": 900 }),
    )
    .await;
    ok(
        &app,
        None,
        "catalog",
        "/api/catalog",
        json!({ "symbol": "ORE", "price_cents": 100 }),
    )
    .await;
    ok(
        &app,
        None,
        "recipe",
        "/api/recipes",
        json!({
            "id": "smelt",
            "inputs": [{ "symbol": "ORE", "qty": 2 }],
            "outputs": [{ "symbol": "INGOT", "qty": 3 }],
            "duration_secs": 60,
        }),
    )
    .await;
    let player = sign_up(&app, "p1", "quinn").await;
    let r = post(
        &app,
        None,
        Some("mint"),
        &format!("/api/accounts/{}/deposit", player.account_id),
        json!({ "amount_cents": 100_000 }),
    )
    .await;
    assert!(r.status.is_success(), "{}", r.body);
    ok(
        &app,
        Some(&player.key),
        "buy-ore",
        &format!("/api/traders/{}/purchases", player.trader_id),
        json!({ "symbol": "ORE", "qty": 2 }),
    )
    .await;
    ok(
        &app,
        Some(&player.key),
        "job",
        "/api/jobs",
        json!({ "trader_id": player.trader_id, "recipe": "smelt" }),
    )
    .await;

    // Step over the due instant several times.
    for i in 1..=5 {
        engine::advance_to(&app, Timestamp(NOW_MS + 61_000 + i * 1_000)).await;
    }
    assert_eq!(units(&app, "INGOT").await, (3, 0), "one delivery");
    audit_balanced(&app, "the job came due").await;

    // And a restart that replays the steps delivers it once, not again.
    let entries = journal::read(&journal::path_for(&path)).expect("a readable journal");
    drop(app);
    let app = App::resume(options(Some(path.clone())), fresh_save().await, entries).await;
    assert_eq!(
        units(&app, "INGOT").await,
        (3, 0),
        "replay delivers the same one delivery"
    );
    assert_eq!(holding(&app, &player, "INGOT").await, 3);
    audit_balanced(&app, "the world was replayed").await;
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
