//! Recipes and jobs: the world turning things into other things.
//!
//! A job is the third thing that makes a unit of a good, after a catalogue
//! purchase and an NPC endowment, and the only one that destroys units to do
//! it. So every test here asks the same two questions the goods suite asks —
//! does the currency add up, and is every issued unit somewhere — around the
//! things a job can do: take its inputs, take its cost, come due, be
//! cancelled, be retried, and survive a restart while it is still running.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use fehu::Timestamp;
use fehu_economy::market::{App, Options, Resume};
use fehu_economy::{engine, router, save};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::util::ServiceExt;

/// 2023-11-14T22:13:20Z, as in the other suites.
const NOW_MS: i64 = 1_700_000_000_000;

fn test_app() -> Arc<App> {
    App::new(options())
}

fn options() -> Options {
    Options {
        history_days: 0,
        warmup_hours: 0,
        now_ms: Some(NOW_MS),
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
    post_keyed(app, key, uri, body, None).await
}

/// `POST` with an `Idempotency-Key`, so a retry can be told from a second
/// request.
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

async fn delete(app: &Arc<App>, key: Option<&str>, uri: &str) -> (StatusCode, Value) {
    call(
        app,
        key,
        Request::builder()
            .method(Method::DELETE)
            .uri(uri)
            .body(Body::empty())
            .unwrap(),
    )
    .await
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

/// The books add up and every issued unit is somewhere.
async fn reconciles(app: &Arc<App>) {
    let (status, body) = get(app, None, "/api/reconcile").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["valid"], true, "{body}");
}

async fn list_good(app: &Arc<App>, symbol: &str, unit: &str) {
    let (status, body) = post(
        app,
        None,
        "/api/symbols",
        json!({
            "symbol": symbol,
            "kind": "good",
            "name": symbol,
            "unit": unit,
            "start_price_cents": 250,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

/// Put `symbol` on the catalogue and buy `qty` of it for `player`.
async fn stock_up(app: &Arc<App>, player: &Player, symbol: &str, qty: u64, price_cents: i64) {
    let (status, body) = post(
        app,
        None,
        "/api/catalog",
        json!({ "symbol": symbol, "price_cents": price_cents }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = post(
        app,
        Some(&player.key),
        &format!("/api/traders/{}/purchases", player.id),
        json!({ "symbol": symbol, "qty": qty }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

/// The smelting recipe every test here uses: two ore and a little money
/// become one ingot, after a minute.
async fn smelt(app: &Arc<App>, cost_cents: i64, duration_secs: u64, refund_bps: u32) -> Value {
    let (status, body) = post(
        app,
        None,
        "/api/recipes",
        json!({
            "id": "smelt",
            "inputs": [{ "symbol": "ORE", "qty": 2 }],
            "outputs": [{ "symbol": "INGOT", "qty": 1 }],
            "cost_cents": cost_cents,
            "duration_secs": duration_secs,
            "refund_bps": refund_bps,
            "note": "Two ore, one ingot.",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

/// A world with ore and ingots listed and a player holding ore.
async fn forge(cash_cents: i64, ore: u64) -> (Arc<App>, Player) {
    let app = test_app();
    list_good(&app, "ore", "kg").await;
    list_good(&app, "ingot", "bar").await;
    let player = sign_up(&app, "smith", cash_cents).await;
    stock_up(&app, &player, "ORE", ore, 100).await;
    (app, player)
}

async fn start_job(app: &Arc<App>, player: &Player, recipe: &str) -> (StatusCode, Value) {
    post(
        app,
        Some(&player.key),
        "/api/jobs",
        json!({ "trader_id": player.id, "recipe": recipe }),
    )
    .await
}

#[tokio::test]
async fn a_recipe_names_listed_goods_and_nothing_else() {
    let app = test_app();
    list_good(&app, "ore", "kg").await;
    list_good(&app, "ingot", "bar").await;

    // A ticker nobody listed.
    let (status, body) = post(
        &app,
        None,
        "/api/recipes",
        json!({
            "id": "smelt",
            "inputs": [{ "symbol": "SLAG", "qty": 1 }],
            "outputs": [{ "symbol": "INGOT", "qty": 1 }],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    // A company: shares are floated, not made to order.
    let (status, body) = post(
        &app,
        None,
        "/api/recipes",
        json!({
            "id": "smelt",
            "inputs": [{ "symbol": "ORE", "qty": 1 }],
            "outputs": [{ "symbol": "ACME", "qty": 1 }],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "not_a_good", "{body}");

    // A recipe that makes nothing.
    let (status, body) = post(
        &app,
        None,
        "/api/recipes",
        json!({ "id": "smelt", "inputs": [{ "symbol": "ORE", "qty": 1 }], "outputs": [] }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    let recipe = smelt(&app, 500, 60, 0).await;
    assert_eq!(recipe["id"], "SMELT", "the id is cleaned like a ticker");
    assert_eq!(recipe["version"], 1);
    let (_, book) = get(&app, None, "/api/recipes").await;
    assert_eq!(book["recipes"].as_array().unwrap().len(), 1);

    // Rewriting bumps the version rather than adding a second recipe.
    smelt(&app, 700, 60, 0).await;
    let (_, book) = get(&app, None, "/api/recipes").await;
    assert_eq!(book["recipes"].as_array().unwrap().len(), 1);
    assert_eq!(book["recipes"][0]["version"], 2);
    assert_eq!(book["recipes"][0]["cost_cents"], 700);
}

#[tokio::test]
async fn a_job_takes_its_inputs_and_its_cost_at_the_start() {
    let (app, player) = forge(1_000_000, 10).await;
    smelt(&app, 500, 60, 0).await;
    let before = supply(&app).await;

    let (status, job) = start_job(&app, &player, "smelt").await;
    assert_eq!(status, StatusCode::CREATED, "{job}");
    assert_eq!(job["status"], "running");
    assert_eq!(job["outputs"][0]["symbol"], "INGOT");
    assert_eq!(job["outputs"][0]["qty"], 1);
    assert_eq!(job["yield_bps"], 10_000, "a quiet world makes the recipe");
    assert_eq!(
        job["due_at_ms"].as_i64().unwrap() - job["started_at_ms"].as_i64().unwrap(),
        60_000,
        "due a minute of simulated time after it started"
    );

    // The ore is gone — consumed, not reserved — and so is the money.
    let (_, ore) = get(&app, None, "/api/symbols/ore").await;
    assert_eq!(ore["info"]["asset"]["issued"], 10);
    assert_eq!(ore["info"]["asset"]["consumed"], 2);
    let (_, inventory) = get(
        &app,
        Some(&player.key),
        &format!("/api/traders/{}/inventory", player.id),
    )
    .await;
    let ore_row = inventory["inventory"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["symbol"] == "ORE")
        .unwrap()
        .clone();
    assert_eq!(ore_row["qty"], 8);
    assert_eq!(
        ore_row["reserved_shares"], 0,
        "a job holds nothing back: it took what it needed"
    );

    let (_, account) = get(
        &app,
        Some(&player.key),
        &format!("/api/accounts/{}/ledger", player.account_id),
    )
    .await;
    assert_eq!(account["entries"][0]["kind"], "job_cost");
    assert_eq!(account["entries"][0]["amount_cents"], -500);

    let after = supply(&app).await;
    assert_eq!(
        after["outstanding_cents"], before["outstanding_cents"],
        "a job moves currency, it does not make it"
    );
    assert_eq!(
        after["venue_cents"].as_i64().unwrap(),
        before["venue_cents"].as_i64().unwrap() + 500,
        "the furnace charges, and the venue holds it"
    );
    reconciles(&app).await;
}

#[tokio::test]
async fn a_job_the_owner_cannot_pay_for_is_refused_whole() {
    // Enough for the ore, four cents short of the furnace.
    let (app, player) = forge(1_400, 10).await;
    smelt(&app, 500, 60, 0).await;

    let (status, body) = start_job(&app, &player, "smelt").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "insufficient_funds", "{body}");

    let (_, ore) = get(&app, None, "/api/symbols/ore").await;
    assert_eq!(
        ore["info"]["asset"]["consumed"], 0,
        "a refused job burns nothing"
    );
    let (_, jobs) = get(&app, Some(&player.key), "/api/jobs").await;
    assert!(jobs["jobs"].as_array().unwrap().is_empty());
    reconciles(&app).await;
}

#[tokio::test]
async fn a_job_without_the_ore_is_refused_before_the_money_moves() {
    let (app, player) = forge(1_000_000, 1).await;
    smelt(&app, 500, 60, 0).await;

    let (status, body) = start_job(&app, &player, "smelt").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "insufficient_inventory", "{body}");

    let (_, account) = get(
        &app,
        Some(&player.key),
        &format!("/api/accounts/{}", player.account_id),
    )
    .await;
    assert_eq!(
        account["balance_cents"], 999_900,
        "the one ore it bought, and nothing else"
    );
    reconciles(&app).await;
}

#[tokio::test]
async fn units_promised_to_a_resting_sell_cannot_be_smelted() {
    let (app, player) = forge(1_000_000, 4).await;
    smelt(&app, 0, 60, 0).await;
    // Three of the four ore are promised to a resting ask.
    let (status, body) = post(
        &app,
        Some(&player.key),
        "/api/symbols/ORE/orders",
        json!({
            "trader_id": player.id, "side": "sell", "qty": 3,
            "type": "limit", "price_cents": 400
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let (status, body) = start_job(&app, &player, "smelt").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("free of reservations"),
        "{body}"
    );
    reconciles(&app).await;
}

#[tokio::test]
async fn a_job_may_run_its_recipe_several_times_as_one_batch() {
    let (app, player) = forge(1_000_000, 10).await;
    smelt(&app, 500, 60, 0).await;
    let (_, before) = get(
        &app,
        Some(&player.key),
        &format!("/api/traders/{}", player.id),
    )
    .await;
    let cash_before = before["cash_cents"].as_i64().unwrap();

    // Three runs: six ore and 15.00, for three ingots, as one job.
    let (status, job) = post(
        &app,
        Some(&player.key),
        "/api/jobs",
        json!({ "trader_id": player.id, "recipe": "smelt", "runs": 3 }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{job}");
    assert_eq!(job["runs"], 3);
    assert_eq!(job["inputs"], json!([{ "symbol": "ORE", "qty": 6 }]));
    assert_eq!(job["outputs"], json!([{ "symbol": "INGOT", "qty": 3 }]));
    assert_eq!(job["cost_cents"], 1_500);
    let (_, after) = get(
        &app,
        Some(&player.key),
        &format!("/api/traders/{}", player.id),
    )
    .await;
    assert_eq!(after["cash_cents"].as_i64().unwrap(), cash_before - 1_500);

    // More runs than the ore allows is refused whole, and no runs at all is
    // not a job.
    let (status, body) = post(
        &app,
        Some(&player.key),
        "/api/jobs",
        json!({ "trader_id": player.id, "recipe": "smelt", "runs": 3 }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "four ore left: {body}"
    );
    let (_, unchanged) = get(
        &app,
        Some(&player.key),
        &format!("/api/traders/{}", player.id),
    )
    .await;
    assert_eq!(
        unchanged["cash_cents"], after["cash_cents"],
        "nothing moved"
    );
    let (status, body) = post(
        &app,
        Some(&player.key),
        "/api/jobs",
        json!({ "trader_id": player.id, "recipe": "smelt", "runs": 0 }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // One delivery, three ingots, and a unit cost spread over all of them.
    engine::advance_to(&app, Timestamp(NOW_MS + 61_000)).await;
    let (_, ingot) = get(&app, None, "/api/symbols/ingot").await;
    assert_eq!(ingot["info"]["asset"]["issued"], 3);
    let (_, inventory) = get(
        &app,
        Some(&player.key),
        &format!("/api/traders/{}/inventory", player.id),
    )
    .await;
    let ingots = inventory["inventory"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["symbol"] == "INGOT")
        .unwrap()
        .clone();
    assert_eq!(ingots["qty"], 3);
    assert_eq!(
        ingots["cost_cents"], 2_100,
        "15.00 plus six ore at 1.00 is what the three of them cost, 7.00 each"
    );
    reconciles(&app).await;
}

#[tokio::test]
async fn a_restart_pauses_the_world_unless_told_to_catch_up() {
    // The same world twice: a job due in ten minutes, a snapshot, and a
    // restart that finds the server was away for twelve. Paused, the job is
    // still ten minutes out; catching up, it fell due while nobody was
    // looking and the first step delivers it.
    for (policy, delivered) in [(Resume::Pause, false), (Resume::CatchUp, true)] {
        let dir = tempdir_lite::TempDir::new("fehu-resume");
        let path = dir.path().join("state.json");
        let before = App::new(Options {
            state_file: Some(path.clone()),
            ..options()
        });
        list_good(&before, "ore", "kg").await;
        list_good(&before, "ingot", "bar").await;
        let player = sign_up(&before, "smith", 1_000_000).await;
        stock_up(&before, &player, "ORE", 10, 100).await;
        smelt(&before, 500, 600, 0).await;
        let (_, job) = start_job(&before, &player, "smelt").await;
        let job_id = job["id"].as_u64().unwrap();
        save::write(&before, &path).await.expect("state written");

        // Twelve minutes of wall time have passed since the file was written.
        let mut saved = save::read(&path).unwrap();
        saved.saved_at_ms -= 720_000;
        let after = App::resume(
            Options {
                state_file: Some(path.clone()),
                resume: policy,
                ..options()
            },
            saved,
            Vec::new(),
        )
        .await;
        engine::step(&after).await;

        let (_, job) = get(&after, Some(&player.key), &format!("/api/jobs/{job_id}")).await;
        let (_, ingot) = get(&after, None, "/api/symbols/ingot").await;
        if delivered {
            assert_eq!(job["status"], "done", "{policy:?}: {job}");
            assert_eq!(ingot["info"]["asset"]["issued"], 1, "{policy:?}");
            assert!(
                job["finished_at_ms"].as_i64().unwrap() >= NOW_MS + 720_000,
                "delivered at the instant the catch-up step reached: {job}"
            );
        } else {
            assert_eq!(job["status"], "running", "{policy:?}: {job}");
            assert_eq!(ingot["info"]["asset"]["issued"], 0, "{policy:?}");
        }
        reconciles(&after).await;
    }
}

#[tokio::test]
async fn a_job_delivers_on_the_step_that_reaches_it() {
    let (app, player) = forge(1_000_000, 10).await;
    smelt(&app, 500, 60, 0).await;
    let (_, job) = start_job(&app, &player, "smelt").await;
    let job_id = job["id"].as_u64().unwrap();

    engine::advance_to(&app, Timestamp(NOW_MS + 30_000)).await;
    let (_, running) = get(&app, Some(&player.key), &format!("/api/jobs/{job_id}")).await;
    assert_eq!(running["status"], "running", "it is not due yet");
    let (_, ingot) = get(&app, None, "/api/symbols/ingot").await;
    assert_eq!(ingot["info"]["asset"]["issued"], 0);

    engine::advance_to(&app, Timestamp(NOW_MS + 61_000)).await;
    let (_, done) = get(&app, Some(&player.key), &format!("/api/jobs/{job_id}")).await;
    assert_eq!(done["status"], "done");
    assert_eq!(done["finished_at_ms"], NOW_MS + 61_000);

    let (_, ingot) = get(&app, None, "/api/symbols/ingot").await;
    assert_eq!(ingot["info"]["asset"]["issued"], 1, "one ingot, made once");
    let (_, inventory) = get(
        &app,
        Some(&player.key),
        &format!("/api/traders/{}/inventory", player.id),
    )
    .await;
    let ingots = inventory["inventory"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["symbol"] == "INGOT")
        .unwrap()
        .clone();
    assert_eq!(ingots["qty"], 1);
    assert_eq!(
        ingots["cost_cents"], 700,
        "the furnace's 5.00 and the two ore at 1.00: what went in is what it cost"
    );
    let ore = inventory["inventory"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["symbol"] == "ORE")
        .unwrap()
        .clone();
    assert_eq!(
        ore["realised_pnl_cents"], 0,
        "and the ore was not written off on the way in"
    );

    // And it is delivered once, however many steps go past it.
    engine::advance_to(&app, Timestamp(NOW_MS + 300_000)).await;
    let (_, ingot) = get(&app, None, "/api/symbols/ingot").await;
    assert_eq!(ingot["info"]["asset"]["issued"], 1);
    reconciles(&app).await;
}

#[tokio::test]
async fn a_cancelled_job_gives_back_what_the_recipe_says_and_no_more() {
    // Half the cost back, and the ore stays in the crucible.
    let (app, player) = forge(1_000_000, 10).await;
    smelt(&app, 1_000, 600, 5_000).await;
    let (_, job) = start_job(&app, &player, "smelt").await;
    let job_id = job["id"].as_u64().unwrap();

    let (status, cancelled) = post(
        &app,
        Some(&player.key),
        &format!("/api/jobs/{job_id}/cancel"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{cancelled}");
    assert_eq!(cancelled["status"], "cancelled");
    assert_eq!(cancelled["refunded_cents"], 500);

    let (_, ore) = get(&app, None, "/api/symbols/ore").await;
    assert_eq!(
        ore["info"]["asset"]["consumed"], 2,
        "the ore went into the crucible and stays there"
    );

    // Cancelling twice is refused rather than paid twice.
    let (status, body) = post(
        &app,
        Some(&player.key),
        &format!("/api/jobs/{job_id}/cancel"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    // And a cancelled job delivers nothing when its due time passes.
    engine::advance_to(&app, Timestamp(NOW_MS + 700_000)).await;
    let (_, ingot) = get(&app, None, "/api/symbols/ingot").await;
    assert_eq!(ingot["info"]["asset"]["issued"], 0);
    reconciles(&app).await;
}

#[tokio::test]
async fn a_job_is_its_owners_to_start_and_to_stop() {
    let (app, player) = forge(1_000_000, 10).await;
    let other = sign_up(&app, "stranger", 1_000_000).await;
    smelt(&app, 0, 600, 0).await;

    let (status, body) = post(
        &app,
        Some(&other.key),
        "/api/jobs",
        json!({ "trader_id": player.id, "recipe": "smelt" }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    let (_, job) = start_job(&app, &player, "smelt").await;
    let job_id = job["id"].as_u64().unwrap();
    let (status, body) = post(
        &app,
        Some(&other.key),
        &format!("/api/jobs/{job_id}/cancel"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // Reading one is its owner's or the operator's. This world has no
    // `FEHU_ADMIN_KEY`, so every caller is the operator — as they are for
    // every other operator route in the demo — and the useful assertion is
    // that the owner is not shut out of their own job.
    let (status, body) = get(&app, Some(&player.key), &format!("/api/jobs/{job_id}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, mine) = get(&app, Some(&player.key), "/api/jobs").await;
    assert_eq!(mine["jobs"].as_array().unwrap().len(), 1);
    let (_, theirs) = get(&app, Some(&other.key), "/api/jobs").await;
    assert!(
        theirs["jobs"].as_array().unwrap().is_empty(),
        "and that the list is of your own"
    );
}

#[tokio::test]
async fn a_retried_job_is_not_started_twice() {
    let (app, player) = forge(1_000_000, 10).await;
    smelt(&app, 500, 600, 0).await;
    let body = json!({ "trader_id": player.id, "recipe": "smelt" });

    let (first, one) = post_keyed(
        &app,
        Some(&player.key),
        "/api/jobs",
        body.clone(),
        Some("job-1"),
    )
    .await;
    let (second, two) = post_keyed(&app, Some(&player.key), "/api/jobs", body, Some("job-1")).await;
    assert_eq!(first, StatusCode::CREATED, "{one}");
    assert_eq!(second, StatusCode::CREATED, "{two}");
    assert_eq!(one["id"], two["id"], "the same job, answered twice");

    let (_, ore) = get(&app, None, "/api/symbols/ore").await;
    assert_eq!(
        ore["info"]["asset"]["consumed"], 2,
        "one job, one set of inputs"
    );
    let (_, jobs) = get(&app, Some(&player.key), "/api/jobs").await;
    assert_eq!(jobs["jobs"].as_array().unwrap().len(), 1);
    reconciles(&app).await;
}

#[tokio::test]
async fn an_event_moves_the_yield_of_the_next_job_and_not_the_one_running() {
    let (app, player) = forge(10_000_000, 100).await;
    // Ten ingots a job, so a fifth either way is visible.
    let (status, body) = post(
        &app,
        None,
        "/api/recipes",
        json!({
            "id": "smelt",
            "inputs": [{ "symbol": "ORE", "qty": 2 }],
            "outputs": [{ "symbol": "INGOT", "qty": 10 }],
            "duration_secs": 60,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (_, quiet) = start_job(&app, &player, "smelt").await;
    assert_eq!(quiet["outputs"][0]["qty"], 10);
    assert_eq!(quiet["yield_bps"], 10_000);

    // A scandal at the forge: the world makes less of what it makes.
    let (status, body) = post(
        &app,
        None,
        "/api/game/events",
        json!({ "kind": "scandal", "symbol": "INGOT", "magnitude": 1.0, "source": "quest-7" }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");

    let (_, world) = get(&app, None, "/api/world").await;
    let ingot = world["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["symbol"] == "INGOT")
        .unwrap()
        .clone();
    // Read a moment after the event landed, so the ramp has already taken a
    // sliver off the full pull.
    let production = ingot["production_bps"].as_i64().unwrap();
    let demand = ingot["demand_bps"].as_i64().unwrap();
    assert!(
        (8_500..8_520).contains(&production),
        "a scandal is 15 % off production, got {production}"
    );
    assert!(
        (6_000..6_050).contains(&demand),
        "and 40 % off demand, got {demand}"
    );
    assert_eq!(world["modifiers"].as_array().unwrap().len(), 2);

    // Rewritten to take ten minutes, so the two jobs land separately and
    // the first one's delivery can be read on its own.
    let (status, body) = post(
        &app,
        None,
        "/api/recipes",
        json!({
            "id": "smelt",
            "inputs": [{ "symbol": "ORE", "qty": 2 }],
            "outputs": [{ "symbol": "INGOT", "qty": 10 }],
            "duration_secs": 600,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (_, hurt) = start_job(&app, &player, "smelt").await;
    let yielded = hurt["yield_bps"].as_i64().unwrap();
    assert!((8_500..8_520).contains(&yielded), "{hurt}");
    assert_eq!(hurt["outputs"][0]["qty"], 8, "ten, scaled and rounded down");

    // The job that was already running delivers what it promised.
    engine::advance_to(&app, Timestamp(NOW_MS + 61_000)).await;
    let (_, first) = get(
        &app,
        Some(&player.key),
        &format!("/api/jobs/{}", quiet["id"].as_u64().unwrap()),
    )
    .await;
    assert_eq!(first["status"], "done");
    let (_, ingot) = get(&app, None, "/api/symbols/ingot").await;
    assert_eq!(
        ingot["info"]["asset"]["issued"], 10,
        "the promise, not the news"
    );
    reconciles(&app).await;
}

#[tokio::test]
async fn what_an_event_did_ramps_down_and_is_forgotten() {
    let app = test_app();
    list_good(&app, "ore", "kg").await;
    let (status, body) = post(
        &app,
        None,
        "/api/game/events",
        json!({ "kind": "hype", "symbol": "ORE", "magnitude": 1.0, "source": "quest-9" }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");

    let demand = |world: &Value| -> i64 {
        world["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["symbol"] == "ORE")
            .unwrap()["demand_bps"]
            .as_i64()
            .unwrap()
    };
    let (_, now) = get(&app, None, "/api/world").await;
    assert!((14_980..=15_000).contains(&demand(&now)), "{now}");

    // Hype lasts two hours and fades in a straight line. `at_ms` asks what
    // it will be worth later, which is the question a player about to start
    // a ten-minute job actually has.
    let (_, halfway) = get(
        &app,
        None,
        &format!("/api/world?at_ms={}", NOW_MS + 3_600_000),
    )
    .await;
    assert!(
        (12_480..=12_500).contains(&demand(&halfway)),
        "half spent, half the pull: {halfway}"
    );

    engine::advance_to(&app, Timestamp(NOW_MS + 7_260_000)).await;
    let (_, over) = get(
        &app,
        None,
        &format!("/api/world?at_ms={}", NOW_MS + 7_260_000),
    )
    .await;
    assert_eq!(demand(&over), 10_000);
    assert!(
        over["modifiers"].as_array().unwrap().is_empty(),
        "and the step sweeps what is spent"
    );
}

#[tokio::test]
async fn demand_changes_what_a_merchant_quotes() {
    // No synthetic ladder, so the only thing on the book is the merchant.
    let app = App::new(Options {
        synthetic: false,
        ..options()
    });
    let (status, body) = post(
        &app,
        None,
        "/api/npcs",
        json!({
            "symbol": "ACME", "name": "Acme Merchant",
            "cash_cents": 100_000_000, "inventory": 5_000,
            "levels": 1, "size": 100,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    engine::advance_to(&app, Timestamp(NOW_MS + 1_000)).await;
    let (_, npcs) = get(&app, None, "/api/npcs").await;
    assert_eq!(npcs["npcs"][0]["quoted_size"], 100);

    post(
        &app,
        None,
        "/api/game/events",
        json!({ "kind": "hype", "symbol": "ACME", "magnitude": 1.0, "source": "quest-11" }),
    )
    .await;
    engine::advance_to(&app, Timestamp(NOW_MS + 2_000)).await;
    let (_, npcs) = get(&app, None, "/api/npcs").await;
    let quoted = npcs["npcs"][0]["quoted_size"].as_u64().unwrap();
    assert!(
        (145..=150).contains(&quoted),
        "half again as much, less what the ramp has already spent: {quoted}"
    );
    let (_, book) = get(&app, None, "/api/symbols/acme/book").await;
    assert_eq!(
        book["bids"][0]["qty"], quoted,
        "and that is what is on the book"
    );
    reconciles(&app).await;
}

#[tokio::test]
async fn taking_a_recipe_away_leaves_the_job_running_under_it() {
    let (app, player) = forge(1_000_000, 10).await;
    smelt(&app, 0, 60, 0).await;
    let (_, job) = start_job(&app, &player, "smelt").await;

    let (status, body) = delete(&app, None, "/api/recipes/smelt").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, book) = get(&app, None, "/api/recipes").await;
    assert!(book["recipes"].as_array().unwrap().is_empty());
    let (status, body) = start_job(&app, &player, "smelt").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    engine::advance_to(&app, Timestamp(NOW_MS + 61_000)).await;
    let (_, done) = get(
        &app,
        Some(&player.key),
        &format!("/api/jobs/{}", job["id"].as_u64().unwrap()),
    )
    .await;
    assert_eq!(done["status"], "done", "what was promised is delivered");
    let (_, ingot) = get(&app, None, "/api/symbols/ingot").await;
    assert_eq!(ingot["info"]["asset"]["issued"], 1);
    reconciles(&app).await;
}

#[tokio::test]
async fn a_world_with_a_job_in_the_furnace_comes_back_running_it() {
    let dir = tempdir_lite::TempDir::new("fehu-jobs");
    let path = dir.path().join("state.json");
    let options = || Options {
        state_file: Some(path.clone()),
        ..options()
    };
    let before = App::new(options());
    list_good(&before, "ore", "kg").await;
    list_good(&before, "ingot", "bar").await;
    let player = sign_up(&before, "smith", 1_000_000).await;
    stock_up(&before, &player, "ORE", 10, 100).await;
    smelt(&before, 500, 600, 0).await;
    let (_, job) = start_job(&before, &player, "smelt").await;
    let job_id = job["id"].as_u64().unwrap();

    save::write(&before, &path).await.expect("state written");
    let after = App::restore(options(), save::read(&path).unwrap());

    let (_, restored) = get(&after, Some(&player.key), &format!("/api/jobs/{job_id}")).await;
    assert_eq!(restored, job, "the promise came back word for word");
    let (_, recipes) = get(&after, None, "/api/recipes").await;
    assert_eq!(recipes["recipes"][0]["id"], "SMELT");
    reconciles(&after).await;

    // And it is still due when it was due.
    engine::advance_to(&after, Timestamp(NOW_MS + 601_000)).await;
    let (_, done) = get(&after, Some(&player.key), &format!("/api/jobs/{job_id}")).await;
    assert_eq!(done["status"], "done");
    let (_, ingot) = get(&after, None, "/api/symbols/ingot").await;
    assert_eq!(ingot["info"]["asset"]["issued"], 1);
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
