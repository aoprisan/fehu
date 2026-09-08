//! HTTP surface: JSON endpoints, the SSE stream and the embedded UI.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequestParts, OptionalFromRequestParts, Path, Query, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post};
use fehu::{Candle, Config, Interval, Owner, TraderId};
use serde::{Deserialize, Serialize};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{Stream, StreamExt};
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

use crate::account::{
    Account, AccountCheck, AccountDto, AccountId, AccountStatus, CreateUserRequest, LedgerResponse,
    MoneyError, OpenAccountRequest, StatusRequest, TransferRequest, UserDto, UserId,
};
use fehu::ledger::LedgerError;

use crate::actor::Gone;
use crate::catalog::{CatalogError, CatalogResponse, GoodsError, MAX_NOTE_LEN};
use crate::events::{CatalogEntry, EventRecord, GameEventKind, GameEventRequest, PushEventRequest};
use crate::jobs::{JobError, JobsResponse, MAX_RECIPE_NOTE, RecipesResponse};
use crate::journal::{Command, Listing, Outcome, Principal, seed_from_ticker};
use crate::limit::Decision;
use crate::market::{
    App, AssetKind, Closed, Market, OrderCheck, PayoutError, PlaceError, Quote, Sequenced,
    SnapshotDto, StreamMessage, Subscription, SupplyDto, Symbol, SymbolInfo, SymbolStatus,
    SymbolView, wall_now_ms,
};
use crate::npc::{NpcsResponse, Policy};
use crate::rewards::{BudgetsResponse, RewardError};
use crate::trading::{
    AmendRequest, BookDto, CreateTraderRequest, HolderDto, HoldingDto, MAX_CLIENT_ORDER_ID,
    OpenOrderDto, OrderRecord, OrderRequest, PortfolioDto, PositionDto, Refused, StopOrder,
    StopRequest, TradeDto, TraderSummary, UserHoldingsResponse,
};
use crate::world::WorldResponse;

type AppState = Arc<App>;

/// The UI is built from TypeScript sources in `webapp/ui/` (`just ui`) into
/// `webapp/static/`, and embedded here so the server is a single binary with
/// no runtime asset directory and no Node toolchain. Asset names are fixed
/// rather than content-hashed so they can be named by `include_str!`.
const INDEX_HTML: &str = include_str!("../static/index.html");
const APP_JS: &str = include_str!("../static/assets/app.js");
const APP_CSS: &str = include_str!("../static/assets/app.css");

/// Build the router over a shared [`App`].
pub fn router(app: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/assets/app.js", get(app_js))
        .route("/assets/app.css", get(app_css))
        .route("/api/health", get(health))
        .route("/api/reconcile", get(reconcile))
        .route("/api/symbols", get(list_symbols).post(list_symbol))
        .route("/api/symbols/{symbol}", get(get_symbol))
        .route("/api/symbols/{symbol}/bars", get(get_bars))
        .route(
            "/api/symbols/{symbol}/events",
            get(list_symbol_events).post(push_sim_event),
        )
        .route("/api/symbols/{symbol}/shares", get(get_shares))
        .route("/api/symbols/{symbol}/dividend", post(pay_dividend))
        .route("/api/symbols/{symbol}/delist", post(delist_symbol))
        .route("/api/symbols/{symbol}/status", get(get_status))
        .route("/api/symbols/{symbol}/halt", post(halt_symbol))
        .route("/api/symbols/{symbol}/resume", post(resume_symbol))
        .route("/api/symbols/{symbol}/book", get(get_book))
        .route("/api/symbols/{symbol}/trades", get(get_trades))
        .route(
            "/api/symbols/{symbol}/orders",
            get(list_orders).post(submit_order),
        )
        .route(
            "/api/symbols/{symbol}/orders/{order_id}",
            get(get_order).patch(amend_order).delete(cancel_order),
        )
        .route(
            "/api/symbols/{symbol}/stops",
            get(list_stops).post(submit_stop),
        )
        .route("/api/symbols/{symbol}/stops/{stop_id}", delete(cancel_stop))
        .route("/api/traders", get(list_traders).post(create_trader))
        .route("/api/traders/{trader_id}", get(get_trader))
        .route("/api/traders/{trader_id}/orders", get(list_trader_orders))
        .route("/api/traders/{trader_id}/stops", get(list_trader_stops))
        .route("/api/orders/{order_id}", get(get_order_record))
        .route("/api/traders/{trader_id}/cancel_all", post(cancel_all))
        .route("/api/traders/{trader_id}/deposit", post(trader_deposit))
        .route("/api/users", get(list_users).post(create_user))
        .route("/api/users/{user_id}", get(get_user))
        .route("/api/users/{user_id}/holdings", get(get_holdings))
        .route(
            "/api/users/{user_id}/accounts",
            get(list_user_accounts).post(open_account),
        )
        .route("/api/accounts", get(list_accounts))
        .route("/api/accounts/{account_id}", get(get_account))
        .route("/api/accounts/{account_id}/deposit", post(deposit))
        .route("/api/accounts/{account_id}/withdraw", post(withdraw))
        .route("/api/accounts/{account_id}/status", post(set_status))
        .route("/api/accounts/{account_id}/validate", get(validate_account))
        .route("/api/accounts/{account_id}/ledger", get(get_ledger))
        .route("/api/npcs", get(list_npcs).post(create_npc))
        .route("/api/npcs/{trader_id}/active", post(set_npc_active))
        .route("/api/catalog", get(get_catalog).post(set_catalog_item))
        .route("/api/catalog/{symbol}", delete(remove_catalog_item))
        .route("/api/traders/{trader_id}/purchases", post(purchase))
        .route("/api/traders/{trader_id}/consume", post(consume))
        .route("/api/recipes", get(get_recipes).post(set_recipe))
        .route("/api/recipes/{id}", delete(remove_recipe))
        .route("/api/jobs", get(list_jobs).post(start_job))
        .route("/api/jobs/{job_id}", get(get_job))
        .route("/api/jobs/{job_id}/cancel", post(cancel_job))
        .route("/api/budgets", get(get_budgets).post(create_budget))
        .route("/api/budgets/{wallet_id}/fund", post(fund_budget))
        .route("/api/rewards", post(pay_reward))
        .route("/api/rewards/rules", post(set_reward_rule))
        .route("/api/rewards/rules/{id}", delete(remove_reward_rule))
        .route("/api/transfers", post(transfer))
        .route("/api/wallets/{wallet_id}", get(get_wallet))
        .route(
            "/api/wallets/{wallet_id}/transactions",
            get(get_wallet_transactions),
        )
        .route("/api/traders/{trader_id}/inventory", get(get_inventory))
        .route("/api/world", get(world))
        .route("/api/supply", get(supply))
        .route("/api/commands/{key}", get(get_command))
        // The economy surface the plan names, at the paths it names them at.
        // Every one of these is the same handler as the route above it: one
        // implementation, two spellings, so the game backend can speak the
        // documented economy API and the demo UI can go on speaking the one
        // it was written against.
        .route("/api/v1/economy/players", post(create_trader))
        .route(
            "/api/v1/economy/players/{trader_id}/inventory",
            get(get_inventory),
        )
        .route("/api/v1/economy/wallets/{wallet_id}", get(get_wallet))
        .route(
            "/api/v1/economy/wallets/{wallet_id}/transactions",
            get(get_wallet_transactions),
        )
        .route("/api/v1/economy/transfers", post(transfer))
        .route("/api/v1/economy/rewards", post(pay_reward))
        .route("/api/v1/economy/purchases", post(purchase_body))
        .route("/api/v1/economy/consume", post(consume_body))
        .route("/api/v1/economy/jobs", get(list_jobs).post(start_job))
        .route("/api/v1/economy/jobs/{job_id}", get(get_job))
        .route("/api/v1/economy/jobs/{job_id}/cancel", post(cancel_job))
        .route("/api/v1/economy/recipes", get(get_recipes))
        .route("/api/v1/economy/catalog", get(get_catalog))
        .route("/api/v1/economy/budgets", get(get_budgets))
        .route("/api/v1/economy/supply", get(supply))
        .route("/api/v1/economy/world", get(world))
        .route("/api/v1/economy/commands/{key}", get(get_command))
        .route("/api/v1/economy/reconcile", get(reconcile))
        .route("/api/v1/economy/admin/recipes", post(set_recipe))
        .route("/api/v1/economy/admin/recipes/{id}", delete(remove_recipe))
        .route("/api/v1/economy/admin/catalog", post(set_catalog_item))
        .route(
            "/api/v1/economy/admin/catalog/{symbol}",
            delete(remove_catalog_item),
        )
        .route("/api/v1/economy/admin/budgets", post(create_budget))
        .route(
            "/api/v1/economy/admin/budgets/{wallet_id}/fund",
            post(fund_budget),
        )
        .route("/api/v1/economy/admin/rewards", post(set_reward_rule))
        .route(
            "/api/v1/economy/admin/rewards/{id}",
            delete(remove_reward_rule),
        )
        .route("/api/game/catalog", get(catalog))
        .route("/api/game/events", get(list_events).post(push_game_event))
        .route("/api/events", get(list_events))
        .route("/api/stream", get(stream))
        .layer(middleware::from_fn_with_state(
            Arc::clone(&app),
            rate_limit_writes,
        ))
        .layer(middleware::from_fn_with_state(Arc::clone(&app), count))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(app)
}

/// Time every request and count how it ended.
///
/// Outermost, so it sees what the client saw — the rate limiter's refusals
/// included, which is the point: a server turning requests away is exactly
/// what `/api/health` should be able to say.
async fn count(State(app): State<AppState>, request: Request, next: Next) -> Response {
    let started = std::time::Instant::now();
    let response = next.run(request).await;
    app.metrics
        .request(started.elapsed(), response.status().as_u16());
    response
}

/// Refuse a client that is changing the market faster than
/// `FEHU_RATE_PER_SEC` allows.
///
/// Only unsafe methods are counted. Reads are cheap, idempotent and mostly
/// public; what is worth protecting is the market actor every order goes
/// through, and a client that can open sockets faster than it can be told to
/// stop.
async fn rate_limit_writes(
    State(app): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    if request.method().is_safe() {
        return Ok(next.run(request).await);
    }
    // Whoever the key says, or nobody — an unknown key shares the anonymous
    // bucket, and the handler is left to refuse it properly.
    // The published directory: a request is counted before it waits on
    // anything.
    let who = api_key_of_headers(request.headers()).and_then(|key| app.user_of(&key));
    match app.allow(who).await {
        Decision::Allowed => Ok(next.run(request).await),
        Decision::Limited { retry_after } => Err(ApiError::rate_limited(retry_after)),
    }
}

/// JSON error body: `{"error": {"code": ..., "message": ...}}`.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
    /// Seconds for a `Retry-After` header, when the answer is "later".
    retry_after_secs: Option<u64>,
}

impl ApiError {
    pub(crate) fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            retry_after_secs: None,
        }
    }

    /// The client is changing things faster than the server will take.
    pub(crate) fn rate_limited(retry_after: std::time::Duration) -> Self {
        // Never advise waiting zero seconds: a client that obeys it would
        // spin. The bucket refills continuously, so a second is honest.
        let secs = retry_after.as_secs_f64().ceil().max(1.0) as u64;
        Self {
            retry_after_secs: Some(secs),
            ..Self::new(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limited",
                format!(
                    "too many requests: this server takes changes at \
                     `FEHU_RATE_PER_SEC`. Try again in {secs}s"
                ),
            )
        }
    }

    pub(crate) fn not_found(symbol: &str) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "unknown_symbol",
            format!("no such symbol: {symbol}"),
        )
    }

    pub(crate) fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", message)
    }

    /// The request is well formed but the market is not in a state to take
    /// it: a ticker already listed, a market with no room for another.
    pub(crate) fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, "conflict", message)
    }

    pub(crate) fn invalid_event(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_event", message)
    }

    pub(crate) fn bad_json(rej: JsonRejection) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_json", rej.body_text())
    }

    pub(crate) fn unknown_trader(id: u64) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "unknown_trader",
            format!("no such trader: {id}"),
        )
    }

    pub(crate) fn unknown_order(id: u64) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "unknown_order",
            format!("no resting order {id} (it may have filled or been cancelled)"),
        )
    }

    pub(crate) fn unknown_stop(id: u64) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "unknown_stop",
            format!("no stop {id} is being held for that trader (it may have fired)"),
        )
    }

    pub(crate) fn invalid_order(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_order", message)
    }

    /// The order would have traded with the trader's own resting order.
    pub(crate) fn self_trade(crossing: &[u64]) -> Self {
        Self::new(
            StatusCode::CONFLICT,
            "self_trade",
            format!(
                "this would trade with your own resting order(s) {crossing:?}: \
                 cancel or reprice them first"
            ),
        )
    }

    /// A post-only order that would have taken liquidity instead of adding it.
    pub(crate) fn would_cross(price_cents: i64, best_cents: i64) -> Self {
        Self::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "post_only_would_cross",
            format!(
                "a post-only order at {price_cents} would trade against {best_cents} \
                 instead of resting"
            ),
        )
    }

    /// The `client_order_id` was used before, for a different order.
    pub(crate) fn duplicate_client_order_id(client_order_id: &str, order_id: u64) -> Self {
        Self::new(
            StatusCode::CONFLICT,
            "duplicate_client_order_id",
            format!(
                "client_order_id {client_order_id:?} was already used for order {order_id}, \
                 which asked for something else"
            ),
        )
    }

    /// A submission the market would not place. `symbol` is the ticker it
    /// was sent to, for the messages that name it.
    pub(crate) fn place(symbol: &str, e: PlaceError) -> Self {
        match e {
            PlaceError::UnknownSymbol => Self::not_found(symbol),
            PlaceError::Check(OrderCheck::Invalid(message)) | PlaceError::Invalid(message) => {
                Self::invalid_order(message)
            }
            PlaceError::Check(OrderCheck::Closed(closed)) => Self::closed(symbol, closed),
            PlaceError::Check(OrderCheck::SelfTrade(ids)) => Self::self_trade(&ids),
            PlaceError::Check(OrderCheck::WouldCross {
                price_cents,
                best_cents,
            }) => Self::would_cross(price_cents, best_cents),
            PlaceError::Check(OrderCheck::UnknownOrder(id)) => Self::unknown_order(id),
            PlaceError::Refused(e) => Self::refused(e),
            PlaceError::UnknownTrader(id) => Self::unknown_trader(id),
            PlaceError::DuplicateClientId {
                client_order_id,
                order_id,
            } => Self::duplicate_client_order_id(&client_order_id, order_id),
            PlaceError::Gone => Self::from(Gone),
        }
    }

    /// An order the trader's account would not fund.
    pub(crate) fn refused(e: Refused) -> Self {
        match e {
            Refused::Account(m) if m.is_forbidden() => {
                Self::new(StatusCode::CONFLICT, "account_not_active", e.to_string())
            }
            _ => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "order_refused",
                e.to_string(),
            ),
        }
    }

    /// No API key on a request that needs one.
    pub(crate) fn unauthenticated() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthenticated",
            "this endpoint needs the API key the user was created with: send it as \
             `Authorization: Bearer <key>` or `X-Api-Key: <key>`",
        )
    }

    /// A key that is not one the server issued.
    pub(crate) fn invalid_api_key() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "invalid_api_key",
            "that API key is not one this server issued",
        )
    }

    /// A valid key, but for somebody else's property.
    pub(crate) fn forbidden(what: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, "forbidden", what)
    }

    /// The market will not take an order for this symbol right now.
    pub(crate) fn closed(symbol: &str, closed: Closed) -> Self {
        let code = match closed {
            Closed::Session { .. } => "market_closed",
            Closed::Halted(_) => "symbol_halted",
        };
        Self::new(
            StatusCode::CONFLICT,
            code,
            format!("{symbol}: {closed}. Orders already resting can still be cancelled."),
        )
    }

    pub(crate) fn unknown_user(id: u64) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "unknown_user",
            format!("no such user: {id}"),
        )
    }

    pub(crate) fn unknown_account(id: u64) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "unknown_account",
            format!("no such account: {id}"),
        )
    }

    /// A refused money movement: the amount, the balance or the account's
    /// status made it impossible.
    pub(crate) fn money(e: MoneyError) -> Self {
        let (status, code) = match e {
            MoneyError::NotPositive { .. } | MoneyError::TooLarge { .. } => {
                (StatusCode::BAD_REQUEST, "invalid_amount")
            }
            MoneyError::NoWallet { .. } => (StatusCode::INTERNAL_SERVER_ERROR, "no_wallet"),
            MoneyError::SameAccount { .. } => (StatusCode::BAD_REQUEST, "same_account"),
            MoneyError::Ledger(e) => match e {
                LedgerError::BalanceCap { .. } => (StatusCode::UNPROCESSABLE_ENTITY, "balance_cap"),
                LedgerError::Insufficient { .. } => {
                    (StatusCode::UNPROCESSABLE_ENTITY, "insufficient_funds")
                }
                LedgerError::Status { .. } | LedgerError::Closed(_) => {
                    (StatusCode::CONFLICT, "account_not_active")
                }
                LedgerError::NotEmpty { .. } => (StatusCode::CONFLICT, "cash_reserved"),
                LedgerError::NotPositive { .. } => (StatusCode::BAD_REQUEST, "invalid_amount"),
                _ => (StatusCode::UNPROCESSABLE_ENTITY, "ledger_refused"),
            },
        };
        Self::new(status, code, e.to_string())
    }

    /// The server could not do its own job. Never the client's fault, and
    /// never something they can fix by changing the request.
    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", message)
    }

    /// A purchase or a consumption the world would not carry out.
    pub(crate) fn goods(e: GoodsError) -> Self {
        match e {
            GoodsError::Money(m) => Self::money(m),
            GoodsError::UnknownTrader(id) => Self::unknown_trader(id),
            GoodsError::Unknown(sym) => Self::not_found(&sym),
            GoodsError::NotAGood(_) => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "not_a_good",
                e.to_string(),
            ),
            GoodsError::Quantity(_) => Self::bad_request(e.to_string()),
            GoodsError::Catalog(CatalogError::Unknown(sym)) => Self::new(
                StatusCode::NOT_FOUND,
                "not_in_catalog",
                CatalogError::Unknown(sym).to_string(),
            ),
            GoodsError::Catalog(CatalogError::Exhausted { .. }) => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "catalog_exhausted",
                e.to_string(),
            ),
            GoodsError::Catalog(_) => Self::bad_request(e.to_string()),
            GoodsError::InsufficientUnits { .. } => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "insufficient_inventory",
                e.to_string(),
            ),
        }
    }

    /// A recipe or a job the world would not run.
    pub(crate) fn job(e: JobError) -> Self {
        match e {
            JobError::Money(m) => Self::money(m),
            JobError::UnknownTrader(id) => Self::unknown_trader(id),
            JobError::Unknown(sym) => Self::not_found(&sym),
            JobError::UnknownRecipe(_) => {
                Self::new(StatusCode::NOT_FOUND, "unknown_recipe", e.to_string())
            }
            JobError::UnknownJob(_) => {
                Self::new(StatusCode::NOT_FOUND, "unknown_job", e.to_string())
            }
            JobError::NotAGood(_) => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "not_a_good",
                e.to_string(),
            ),
            JobError::InsufficientUnits { .. } => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "insufficient_inventory",
                e.to_string(),
            ),
            JobError::NotRunning(_) => {
                Self::new(StatusCode::CONFLICT, "job_finished", e.to_string())
            }
            JobError::Full | JobError::TooManyJobs(_) => {
                Self::new(StatusCode::CONFLICT, "too_many", e.to_string())
            }
            JobError::Recipe(_) | JobError::Quantity(_) => Self::bad_request(e.to_string()),
        }
    }

    /// A budget, a rule or a reward the world would not pay.
    pub(crate) fn reward(e: RewardError) -> Self {
        match e {
            RewardError::Money(m) => Self::money(m),
            RewardError::UnknownTrader(id) => Self::unknown_trader(id),
            RewardError::UnknownBudget(_) => {
                Self::new(StatusCode::NOT_FOUND, "unknown_budget", e.to_string())
            }
            RewardError::UnknownRule(_) => {
                Self::new(StatusCode::NOT_FOUND, "unknown_reward_rule", e.to_string())
            }
            RewardError::Exhausted { .. } => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "budget_exhausted",
                e.to_string(),
            ),
            RewardError::Full(_) => Self::new(StatusCode::CONFLICT, "too_many", e.to_string()),
            RewardError::Invalid(_) => Self::bad_request(e.to_string()),
        }
    }

    /// A corporate payout the issuer could not fund.
    pub(crate) fn payout_not_funded(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, "payout_not_funded", message)
    }

    /// A cancellation the book would not take.
    pub(crate) fn cancel(order_id: u64, e: fehu::CancelError) -> Self {
        match e {
            fehu::CancelError::Unknown => Self::unknown_order(order_id),
            fehu::CancelError::NotOwner => {
                Self::new(StatusCode::FORBIDDEN, "not_owner", e.to_string())
            }
            _ => Self::bad_request(e.to_string()),
        }
    }

    /// The same `Idempotency-Key` over a different request. Answering it
    /// with the first request's result would be answering a question that
    /// was not asked, and applying it would make the key meaningless.
    pub(crate) fn idempotency_conflict(key: &str) -> Self {
        Self::new(
            StatusCode::CONFLICT,
            "idempotency_conflict",
            format!(
                "Idempotency-Key {key:?} was used for a different request. \
                 Use a new key, or resend the original request unchanged"
            ),
        )
    }

    /// The journal stopped taking entries, so the market in memory is ahead
    /// of the disk. Nothing further may change until the process restarts
    /// and replays what did reach it.
    pub(crate) fn journal_broken(why: &str) -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "journal_unavailable",
            format!(
                "this server is not writing its journal ({why}), so it will not \
                 accept changes it cannot promise to keep"
            ),
        )
    }

    /// The message a client would be given, for a log line.
    pub(crate) fn message(&self) -> &str {
        &self.message
    }

    /// A corporate payout the issuer could not fund. Nothing was paid.
    pub(crate) fn payout(e: PayoutError) -> Self {
        match e {
            PayoutError::Gone => Self::from(Gone),
            PayoutError::Unfunded(_) => {
                Self::new(StatusCode::CONFLICT, "payout_not_funded", e.to_string())
            }
        }
    }
}

impl From<Gone> for ApiError {
    /// An actor this request needed has stopped. That is the server's
    /// problem, not the client's, and it is not going to get better by
    /// retrying in a hurry.
    fn from(_: Gone) -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "the market is not running",
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = serde_json::json!({
            "error": { "code": self.code, "message": self.message }
        });
        let mut response = (self.status, Json(body)).into_response();
        if let Some(secs) = self.retry_after_secs
            && let Ok(value) = secs.to_string().parse()
        {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        response
    }
}

/// The API key on a request: `Authorization: Bearer <key>`, or `X-Api-Key`.
fn api_key_of(parts: &Parts) -> Option<String> {
    api_key_of_headers(&parts.headers)
}

/// The API key a request carries, as `Authorization: Bearer <key>` or
/// `X-Api-Key`.
fn api_key_of_headers(headers: &HeaderMap) -> Option<String> {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            let (scheme, key) = v.split_once(' ')?;
            scheme.eq_ignore_ascii_case("bearer").then_some(key)
        });
    let key = bearer.or_else(|| headers.get("x-api-key").and_then(|v| v.to_str().ok()))?;
    let key = key.trim();
    (!key.is_empty()).then(|| key.to_owned())
}

/// The user a request speaks for, proved by their API key.
///
/// Market data — quotes, bars, the book, the tape, the event log — needs no
/// key. Everything that belongs to a user does, and a key only ever speaks
/// for the user it was issued to: one player cannot read another's portfolio,
/// move their money or trade on their account.
#[derive(Clone, Copy, Debug)]
pub struct Caller(pub UserId);

impl FromRequestParts<AppState> for Caller {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, app: &AppState) -> Result<Self, ApiError> {
        let key = api_key_of(parts).ok_or_else(ApiError::unauthenticated)?;
        app.user_of(&key)
            .map(Caller)
            .ok_or_else(ApiError::invalid_api_key)
    }
}

impl OptionalFromRequestParts<AppState> for Caller {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        app: &AppState,
    ) -> Result<Option<Self>, ApiError> {
        let Some(key) = api_key_of(parts) else {
            return Ok(None);
        };
        Ok(app.user_of(&key).map(Caller))
    }
}

/// The game master: whoever may push events into the simulation. Open unless
/// `FEHU_ADMIN_KEY` is set, which is what a single-player game on localhost
/// wants and a shared server does not.
#[derive(Clone, Copy, Debug)]
pub struct Admin;

impl FromRequestParts<AppState> for Admin {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, app: &AppState) -> Result<Self, ApiError> {
        let Some(expected) = app.options.admin_key.as_deref() else {
            return Ok(Self);
        };
        let key = api_key_of(parts).ok_or_else(ApiError::unauthenticated)?;
        if constant_time_eq(&key, expected) {
            Ok(Self)
        } else {
            Err(ApiError::invalid_api_key())
        }
    }
}

impl OptionalFromRequestParts<AppState> for Admin {
    type Rejection = ApiError;

    /// Whether the request carries operator authority, without turning the
    /// lack of it into a refusal.
    ///
    /// For a route where the *same* call means different things depending on
    /// who is asking — freezing an account is the operator's, closing it is
    /// the owner's — the handler has to see both and decide, rather than be
    /// turned away at the door.
    async fn from_request_parts(
        parts: &mut Parts,
        app: &AppState,
    ) -> Result<Option<Self>, ApiError> {
        match <Admin as FromRequestParts<AppState>>::from_request_parts(parts, app).await {
            Ok(admin) => Ok(Some(admin)),
            Err(_) => Ok(None),
        }
    }
}

/// The longest `Idempotency-Key` this server will remember.
const MAX_IDEMPOTENCY_KEY: usize = 128;

/// The `Idempotency-Key` header on a mutating request, if it carries one.
///
/// A key makes a retry free: the first request is applied and its response
/// recorded, and every later request with the same key and the same body is
/// answered with that response instead of doing the work twice. The same key
/// over a *different* body is a mistake on the client's part and is refused.
///
/// It is optional. Without one a retry is simply a second request — which
/// for an order is still covered by `client_order_id`, and for a deposit is
/// a second deposit. That is why anything that moves money should send one.
#[derive(Clone, Debug)]
pub struct Idempotency(pub Option<String>);

impl FromRequestParts<AppState> for Idempotency {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _: &AppState) -> Result<Self, ApiError> {
        let Some(value) = parts.headers.get("idempotency-key") else {
            return Ok(Self(None));
        };
        let key = value
            .to_str()
            .map_err(|_| ApiError::bad_request("`Idempotency-Key` must be printable ASCII"))?
            .trim();
        if key.is_empty() {
            return Ok(Self(None));
        }
        if key.chars().count() > MAX_IDEMPOTENCY_KEY {
            return Err(ApiError::bad_request(format!(
                "`Idempotency-Key` is longer than {MAX_IDEMPOTENCY_KEY} characters"
            )));
        }
        Ok(Self(Some(key.to_owned())))
    }
}

impl From<Caller> for Principal {
    fn from(caller: Caller) -> Self {
        Self::User { id: caller.0.0 }
    }
}

/// A command that was applied and written down, on its way back to the
/// client.
///
/// The body is whatever the command produced; the headers say where it
/// landed in the journal (`Fehu-Journal-Seq`) and whether this request did
/// the work or was answered from an earlier one (`Fehu-Idempotent-Replay`).
pub struct Committed(Outcome);

impl IntoResponse for Committed {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.0.status).unwrap_or(StatusCode::OK);
        let mut response = (status, Json(self.0.body)).into_response();
        let headers = response.headers_mut();
        if let Ok(seq) = self.0.seq.to_string().parse() {
            headers.insert("fehu-journal-seq", seq);
        }
        if self.0.replayed {
            headers.insert(
                "fehu-idempotent-replay",
                header::HeaderValue::from_static("true"),
            );
        }
        response
    }
}

/// Put the key just generated into the response that created its user.
///
/// The credential is added here rather than by the command, so that neither
/// the journal nor the idempotency index ever holds one. The cost is
/// deliberate: a *replayed* sign-up comes back without a key, because the
/// only copy went out with the first response. A client that loses that
/// response has to create another user.
fn with_key(mut out: Outcome, api_key: String) -> Committed {
    if !out.replayed
        && let Some(map) = out.body.as_object_mut()
    {
        map.insert("api_key".into(), serde_json::Value::String(api_key));
    }
    Committed(out)
}

/// Send a command to the market: apply it, journal it, and answer with what
/// it did — or with what it did the first time, if this is a retry.
///
/// Every mutation in this module goes through here. The simulated instant is
/// read once, here, and travels with the command, so the market applies it
/// at the moment the request arrived whether that happens now or on the next
/// start-up.
async fn run(
    app: &App,
    principal: Principal,
    key: Option<String>,
    command: Command,
) -> Result<Outcome, ApiError> {
    run_at(app, principal, key, app.clock.now(), command).await
}

/// [`run`], for a command whose caller chose the simulated instant: a game
/// event scheduled for a moment other than now.
async fn run_at(
    app: &App,
    principal: Principal,
    key: Option<String>,
    at: fehu::Timestamp,
    command: Command,
) -> Result<Outcome, ApiError> {
    let kind = command.kind();
    let wall_ms = wall_now_ms();
    let outcome = app
        .market
        .call_async(move |m| {
            Box::pin(async move { m.run_command(principal, key, at, wall_ms, command).await })
        })
        .await??;
    tracing::info!(
        command = kind,
        seq = outcome.seq,
        status = outcome.status,
        replayed = outcome.replayed,
        "command"
    );
    Ok(outcome)
}

/// Compare two secrets without giving their common prefix away in the time
/// taken.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The caller owns `trader`. Answered from the published directory, before
/// anything is sent anywhere: a request for somebody else's trader is turned
/// away without waiting on the market.
pub(crate) fn owned_trader(app: &App, caller: Caller, trader: TraderId) -> Result<(), ApiError> {
    let owner = app
        .owner_of(trader)
        .ok_or_else(|| ApiError::unknown_trader(trader.0))?;
    if owner == caller.0 {
        Ok(())
    } else {
        Err(ApiError::forbidden(format!(
            "trader {} belongs to another user",
            trader.0
        )))
    }
}

/// The caller owns `account`.
pub(crate) fn owned_account(
    market: &Market,
    caller: Caller,
    account: AccountId,
) -> Result<(), ApiError> {
    let held = market
        .accounts
        .get(&account)
        .ok_or_else(|| ApiError::unknown_account(account.0))?;
    if held.user_id == caller.0 {
        Ok(())
    } else {
        Err(ApiError::forbidden(format!(
            "account {} belongs to another user",
            account.0
        )))
    }
}

/// The caller is `user` themselves.
pub(crate) fn owned_user(caller: Caller, user: UserId) -> Result<(), ApiError> {
    if caller.0 == user {
        Ok(())
    } else {
        Err(ApiError::forbidden(format!(
            "user {} is somebody else",
            user.0
        )))
    }
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

/// Embedded asset with its content type. The names are stable across builds,
/// so revalidate rather than let a browser cache a stale bundle.
fn asset(content_type: &'static str, body: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

async fn app_js() -> Response {
    asset("text/javascript; charset=utf-8", APP_JS)
}

async fn app_css() -> Response {
    asset("text/css; charset=utf-8", APP_CSS)
}

#[derive(Serialize)]
struct Health {
    status: &'static str,
    uptime_secs: u64,
    sim_now_ms: i64,
    time_scale: f64,
    symbols: usize,
    events_logged: usize,
    ticks_total: u64,
    trades_total: u64,
    users: usize,
    accounts: usize,
    traders: usize,
    /// Every account's balance added up, in cents.
    cash_cents: i64,
    /// Traders' orders resting across every book.
    resting_orders: usize,
    /// Stops held across every symbol, waiting for a price.
    stops_held: usize,
    /// Orders sent to a book since start-up, and submissions turned away
    /// before they got there.
    orders_placed: u64,
    orders_refused: u64,
    /// Fills booked to traders' accounts since start-up.
    fills_booked: u64,
    /// Fills the book made that the ledger then refused to settle. Zero in a
    /// healthy market; anything else means shares moved and money did not,
    /// and the market wants reconciling.
    settlement_failures: u64,
    /// Messages published to the stream since start-up.
    stream_messages: u64,
    /// Streams currently connected.
    stream_subscribers: usize,
    /// Clients whose rate-limit allowance is being tracked.
    tracked_clients: usize,
    /// Request and engine-step timings since start-up.
    metrics: crate::metrics::MetricsDto,
}

async fn reconcile(
    State(app): State<AppState>,
    _admin: Admin,
) -> Json<crate::reconcile::Reconciliation> {
    Json(app.reconcile().await)
}

async fn health(State(app): State<AppState>) -> Result<Json<Health>, ApiError> {
    let market = app.market.call_async(|m| Box::pin(m.health())).await?;
    Ok(Json(Health {
        status: "ok",
        uptime_secs: app.started_at.elapsed().map_or(0, |d| d.as_secs()),
        sim_now_ms: app.clock.now().0,
        time_scale: app.clock.scale,
        symbols: market.symbols,
        events_logged: market.events_logged,
        ticks_total: market.ticks_total,
        trades_total: market.trades_total,
        users: market.users,
        accounts: market.accounts,
        traders: market.traders,
        cash_cents: market.cash_cents,
        resting_orders: market.resting_orders,
        stops_held: market.stops_held,
        orders_placed: market.orders_placed,
        orders_refused: market.orders_refused,
        fills_booked: market.fills_booked,
        settlement_failures: market.settlement_failures,
        stream_messages: app.published().await,
        stream_subscribers: app.stream.subscribers(),
        tracked_clients: app.tracked_clients().await,
        metrics: app.metrics.snapshot(),
    }))
}

#[derive(Serialize)]
struct SymbolsResponse {
    sim_now_ms: i64,
    symbols: Vec<Quote>,
}

async fn list_symbols(State(app): State<AppState>) -> Json<SymbolsResponse> {
    Json(SymbolsResponse {
        sim_now_ms: app.clock.now().0,
        symbols: app.quotes().await,
    })
}

#[derive(Serialize)]
struct SymbolDetail {
    info: SymbolInfo,
    quote: Quote,
    snapshot: SnapshotDto,
    config: Config,
    ticks_total: u64,
    shares: SharesDto,
    status: SymbolStatus,
}

/// `GET /api/symbols/{symbol}/shares`: where the symbol's shares are. The
/// four numbers add up: `outstanding = held + bid_for + available`.
#[derive(Serialize)]
struct SharesDto {
    symbol: &'static str,
    /// `stock` or `good`.
    asset_kind: &'static str,
    /// What one unit of a good is called; `null` for a stock.
    unit: Option<String>,
    /// Units in existence.
    shares_outstanding: u64,
    /// Held by traders.
    held_shares: u64,
    /// Bid for by traders' resting buy orders.
    bid_shares: u64,
    /// Neither held nor bid for: what a buy can still be filled from.
    available_shares: u64,
    price_cents: i64,
    /// `price × shares_outstanding`.
    market_cap_cents: i64,
    /// The caller's own traders holding shares, largest stake first. Who
    /// else holds them is nobody's business: the totals above are public,
    /// the names behind them are not.
    holders: Vec<HolderDto>,
}

impl SharesDto {
    /// The symbol's side of the answer — how many shares exist, how many
    /// the resting bids speak for, the price — and then the market's: who
    /// holds them. Two actors, asked in turn.
    async fn fetch(app: &App, symbol: &Symbol, caller: Option<Caller>) -> Result<Self, ApiError> {
        let sym = symbol.ticker;
        let (asset_kind, unit, shares_outstanding, bid_shares, price_cents, market_cap_cents) =
            symbol
                .ask_listed(|s| {
                    (
                        s.info.asset.label(),
                        s.info.asset.unit().map(str::to_owned),
                        s.info.units_outstanding(),
                        s.bid_shares(),
                        s.price_cents(),
                        s.market_cap_cents(),
                    )
                })
                .await?
                .ok_or_else(|| ApiError::not_found(sym))?;
        let (held_shares, mut holders) = app
            .market
            .call(move |m| {
                let holders: Vec<HolderDto> = m
                    .traders
                    .values()
                    .filter(|t| caller.is_some_and(|c| c.0 == t.user_id))
                    .filter_map(|t| HolderDto::new(t, sym))
                    .collect();
                (m.held_shares(sym), holders)
            })
            .await?;
        holders.sort_by_key(|h| (std::cmp::Reverse(h.qty), h.trader_id));
        Ok(Self {
            symbol: sym,
            asset_kind,
            unit,
            shares_outstanding,
            held_shares,
            bid_shares,
            available_shares: shares_outstanding
                .saturating_sub(held_shares)
                .saturating_sub(bid_shares),
            price_cents,
            market_cap_cents,
            holders,
        })
    }
}

async fn get_symbol(
    State(app): State<AppState>,
    Path(symbol): Path<String>,
    caller: Option<Caller>,
) -> Result<Json<SymbolDetail>, ApiError> {
    let (now, halts) = (app.clock.now(), app.halts);
    let handle = app
        .symbol(&symbol)
        .ok_or_else(|| ApiError::not_found(&symbol))?;
    let (info, quote, snapshot, config, ticks_total, status) = handle
        .ask_listed(move |s| {
            (
                s.info.clone(),
                s.quote(),
                SnapshotDto::from(s.sim().snapshot()),
                s.sim().config().clone(),
                s.ticks_total,
                s.status(now, halts),
            )
        })
        .await?
        .ok_or_else(|| ApiError::not_found(&symbol))?;
    Ok(Json(SymbolDetail {
        info,
        quote,
        snapshot,
        config,
        ticks_total,
        shares: SharesDto::fetch(&app, &handle, caller).await?,
        status,
    }))
}

/// Whether a symbol can be traded right now, and if not, why not.
async fn get_status(
    State(app): State<AppState>,
    Path(symbol): Path<String>,
) -> Result<Json<SymbolStatus>, ApiError> {
    app.status(&symbol)
        .await
        .map(Json)
        .ok_or_else(|| ApiError::not_found(&symbol))
}

/// Body of `POST /api/symbols`.
#[derive(Deserialize)]
struct ListingRequest {
    /// Ticker. Registered as it is read, upper-cased, and refused if another
    /// listing already has it.
    symbol: String,
    name: Option<String>,
    sector: Option<String>,
    description: Option<String>,
    /// `stock`, the default, or `good`.
    kind: Option<String>,
    /// Shares in existence, for a stock. The market never creates or
    /// destroys them, so this is the ceiling on what every trader can hold
    /// between them. A good has no such number: its units are issued and
    /// consumed one command at a time, and it is listed holding none.
    #[serde(default)]
    shares_outstanding: u64,
    /// What one unit of a good is called: `kg`, `crate`, `ingot`. Ignored
    /// for a stock, whose unit is a share.
    unit: Option<String>,
    /// Where the price starts, in cents.
    start_price_cents: i64,
    /// Annual log drift and annualised volatility; the simulator's defaults
    /// if they are not given.
    drift: Option<f64>,
    volatility: Option<f64>,
    /// The RNG seed. Same seed, same prices — so a listing without one takes
    /// a seed derived from its ticker rather than from a clock: listing
    /// `WDGT` twice on two servers gives the same company.
    seed: Option<u64>,
    /// Days of coarse daily bars to generate before now. `0`, the default,
    /// lists a company with no past, which is what a flotation is.
    history_days: Option<usize>,
    note: Option<String>,
    source: Option<String>,
}

/// List a symbol: from this call on it is quoted, it ticks, and orders in it
/// are accepted like any other.
///
/// The work is done in two halves. Warming the simulator up is the expensive
/// part and needs nothing from the market, so it happens here on the request's
/// own task; the market's job is only to check the ticker is still free and
/// give the listing its actor.
async fn list_symbol(
    State(app): State<AppState>,
    _admin: Admin,
    Idempotency(key): Idempotency,
    payload: Result<Json<ListingRequest>, JsonRejection>,
) -> Result<Committed, ApiError> {
    let Json(req) = payload.map_err(ApiError::bad_json)?;
    let symbol =
        crate::symbols::register(&req.symbol).map_err(|e| ApiError::bad_request(e.to_string()))?;
    // Cheap answer first: warming a year of history up only to find the
    // ticker taken would be a waste of the caller's time and this server's.
    if app.symbol(symbol).is_some() {
        return Err(ApiError::conflict(format!("{symbol} is already listed")));
    }
    let asset = match req.kind.as_deref().map(str::trim).unwrap_or("stock") {
        "stock" => AssetKind::stock(req.shares_outstanding),
        "good" => AssetKind::good(
            clean_text(req.unit, crate::symbol::MAX_UNIT_LEN).unwrap_or_else(|| "unit".into()),
        ),
        other => {
            return Err(ApiError::bad_request(format!(
                "unknown asset kind {other:?}: a listing is a \"stock\" or a \"good\""
            )));
        }
    };
    let defaults = Config::default();
    run(
        &app,
        Principal::Operator,
        key,
        Command::ListSymbol {
            listing: Listing {
                symbol: symbol.to_string(),
                name: clean_text(req.name, 64).unwrap_or_else(|| symbol.to_string()),
                sector: clean_text(req.sector, 64).unwrap_or_else(|| "Uncategorised".into()),
                description: clean_text(req.description, 280).unwrap_or_default(),
                asset,
                seed: req.seed.unwrap_or_else(|| seed_from_ticker(symbol)),
                start_price_cents: req.start_price_cents,
                drift: req.drift.unwrap_or(defaults.drift),
                volatility: req.volatility.unwrap_or(defaults.volatility),
                history_days: req.history_days.unwrap_or(0),
                source: req.source.unwrap_or_else(|| "api".into()),
                note: req.note,
            },
        },
    )
    .await
    .map(Committed)
}

// ---------------------------------------------------------------------------
// NPCs: the traders the world runs itself.

/// `GET /api/npcs`: who the world is trading as, and what each has left.
async fn list_npcs(State(app): State<AppState>) -> Result<Json<NpcsResponse>, ApiError> {
    Ok(Json(NpcsResponse {
        npcs: app.market.call(|m| m.npc_views()).await?,
    }))
}

/// Body of `POST /api/npcs`.
#[derive(Deserialize)]
struct NpcRequest {
    /// The symbol it makes a market in.
    symbol: String,
    name: Option<String>,
    /// Currency it is funded with, out of treasury. Nothing is minted.
    cash_cents: i64,
    /// Units it starts holding: shares of a stock nobody held, or units of a
    /// good issued to it.
    #[serde(default)]
    inventory: u64,
    /// How it quotes. The defaults are a 25 bp half-spread over five levels.
    #[serde(default)]
    half_spread_bps: Option<u32>,
    #[serde(default)]
    levels: Option<u32>,
    #[serde(default)]
    level_step_bps: Option<u32>,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    requote_bps: Option<u32>,
}

/// Put a funded trader in the market that the world runs.
async fn create_npc(
    State(app): State<AppState>,
    _admin: Admin,
    Idempotency(key): Idempotency,
    payload: Result<Json<NpcRequest>, JsonRejection>,
) -> Result<Committed, ApiError> {
    let Json(req) = payload.map_err(ApiError::bad_json)?;
    let symbol = crate::symbol::intern(&req.symbol)
        .ok_or_else(|| ApiError::not_found(&req.symbol))?
        .to_string();
    let base = Policy::default();
    let policy = Policy {
        half_spread_bps: req.half_spread_bps.unwrap_or(base.half_spread_bps),
        levels: req.levels.unwrap_or(base.levels),
        level_step_bps: req.level_step_bps.unwrap_or(base.level_step_bps),
        size: req.size.unwrap_or(base.size),
        requote_bps: req.requote_bps.unwrap_or(base.requote_bps),
    };
    run(
        &app,
        Principal::Operator,
        key,
        Command::CreateNpc {
            symbol,
            name: clean_text(req.name, 64),
            policy,
            cash_cents: req.cash_cents,
            inventory: req.inventory,
        },
    )
    .await
    .map(Committed)
}

/// Body of `POST /api/npcs/{trader_id}/active`.
#[derive(Deserialize)]
struct ActiveRequest {
    active: bool,
}

/// Start or stop an NPC quoting. Its money and inventory stay where they are.
async fn set_npc_active(
    State(app): State<AppState>,
    Path(trader_id): Path<u64>,
    _admin: Admin,
    Idempotency(key): Idempotency,
    payload: Result<Json<ActiveRequest>, JsonRejection>,
) -> Result<Committed, ApiError> {
    let Json(req) = payload.map_err(ApiError::bad_json)?;
    run(
        &app,
        Principal::Operator,
        key,
        Command::SetNpcActive {
            trader_id,
            active: req.active,
        },
    )
    .await
    .map(Committed)
}

// ---------------------------------------------------------------------------
// The catalogue: goods, and the two things that are done to them besides
// trading them.

/// `GET /api/catalog`: what the world will make and what it charges.
async fn get_catalog(State(app): State<AppState>) -> Result<Json<CatalogResponse>, ApiError> {
    Ok(Json(CatalogResponse {
        items: app
            .market
            .call(|m| m.catalog.items().cloned().collect())
            .await?,
    }))
}

/// Body of `POST /api/catalog`.
#[derive(Deserialize)]
struct CatalogRequest {
    /// The good this line sells. It has to be listed, and it has to be a
    /// good.
    symbol: String,
    /// What one unit costs, in cents. At least one: a free good would be a
    /// way of making units out of nothing.
    price_cents: i64,
    /// Units this line may still issue. Omit it for a seam that never runs
    /// out, which is what a demo wants and a scarce world does not.
    available: Option<u64>,
    note: Option<String>,
}

/// Write or replace one line of the catalogue.
async fn set_catalog_item(
    State(app): State<AppState>,
    _admin: Admin,
    Idempotency(key): Idempotency,
    payload: Result<Json<CatalogRequest>, JsonRejection>,
) -> Result<Committed, ApiError> {
    let Json(req) = payload.map_err(ApiError::bad_json)?;
    let symbol = crate::symbol::intern(&req.symbol)
        .ok_or_else(|| ApiError::not_found(&req.symbol))?
        .to_string();
    run(
        &app,
        Principal::Operator,
        key,
        Command::SetCatalogItem {
            symbol,
            price_cents: req.price_cents,
            available: req.available,
            note: clean_text(req.note, MAX_NOTE_LEN),
        },
    )
    .await
    .map(Committed)
}

/// Stop making a good. Units already issued off the line stay in the world.
async fn remove_catalog_item(
    State(app): State<AppState>,
    Path(symbol): Path<String>,
    _admin: Admin,
    Idempotency(key): Idempotency,
) -> Result<Committed, ApiError> {
    let symbol = crate::symbol::intern(&symbol)
        .ok_or_else(|| ApiError::not_found(&symbol))?
        .to_string();
    run(
        &app,
        Principal::Operator,
        key,
        Command::RemoveCatalogItem { symbol },
    )
    .await
    .map(Committed)
}

// ---------------------------------------------------------------------------
// Recipes and jobs. See [`crate::jobs`].

/// One line of a recipe, as a request spells it.
#[derive(Deserialize)]
struct RecipeLineRequest {
    /// A listed good. A company is refused: shares are floated, not made.
    symbol: String,
    qty: u64,
}

/// Body of `POST /api/recipes`.
#[derive(Deserialize)]
struct RecipeRequest {
    /// A short name, uppercased on the way in: `SMELT`.
    id: String,
    /// Units consumed when a job starts. May be empty: a mine takes nothing
    /// but time.
    #[serde(default)]
    inputs: Vec<RecipeLineRequest>,
    /// Units issued when it completes. At least one.
    outputs: Vec<RecipeLineRequest>,
    /// What the furnace charges, paid to the venue when the job starts.
    #[serde(default)]
    cost_cents: i64,
    /// How long it takes, in simulated seconds.
    #[serde(default)]
    duration_secs: u64,
    /// What a cancellation gives back, in basis points of the cost.
    #[serde(default)]
    refund_bps: u32,
    note: Option<String>,
}

fn recipe_lines(lines: Vec<RecipeLineRequest>) -> Vec<(String, u64)> {
    lines.into_iter().map(|l| (l.symbol, l.qty)).collect()
}

/// What the world knows how to make.
async fn get_recipes(State(app): State<AppState>) -> Result<Json<RecipesResponse>, ApiError> {
    Ok(Json(RecipesResponse {
        recipes: app
            .market
            .call(|m| m.recipes.recipes().cloned().collect())
            .await?,
    }))
}

/// Write or replace a recipe. Operator authority.
async fn set_recipe(
    State(app): State<AppState>,
    _admin: Admin,
    Idempotency(key): Idempotency,
    payload: Result<Json<RecipeRequest>, JsonRejection>,
) -> Result<Committed, ApiError> {
    let Json(req) = payload.map_err(ApiError::bad_json)?;
    run(
        &app,
        Principal::Operator,
        key,
        Command::SetRecipe {
            id: req.id,
            inputs: recipe_lines(req.inputs),
            outputs: recipe_lines(req.outputs),
            cost_cents: req.cost_cents,
            duration_secs: req.duration_secs,
            refund_bps: req.refund_bps,
            note: clean_text(req.note, MAX_RECIPE_NOTE),
        },
    )
    .await
    .map(Committed)
}

/// Stop making a thing. Jobs already running still deliver what they promised.
async fn remove_recipe(
    State(app): State<AppState>,
    Path(id): Path<String>,
    _admin: Admin,
    Idempotency(key): Idempotency,
) -> Result<Committed, ApiError> {
    run(&app, Principal::Operator, key, Command::RemoveRecipe { id })
        .await
        .map(Committed)
}

/// Body of `POST /api/jobs`.
#[derive(Deserialize)]
struct JobRequest {
    trader_id: u64,
    /// The recipe to run.
    recipe: String,
}

/// Start a job: the inputs and the cost now, the outputs when it is due.
async fn start_job(
    State(app): State<AppState>,
    caller: Caller,
    Idempotency(key): Idempotency,
    payload: Result<Json<JobRequest>, JsonRejection>,
) -> Result<Committed, ApiError> {
    let Json(req) = payload.map_err(ApiError::bad_json)?;
    owned_trader(&app, caller, TraderId(req.trader_id))?;
    run(
        &app,
        caller.into(),
        key,
        Command::StartJob {
            trader_id: req.trader_id,
            recipe: req.recipe,
        },
    )
    .await
    .map(Committed)
}

/// Every job of the caller's traders, oldest first.
async fn list_jobs(
    State(app): State<AppState>,
    caller: Caller,
) -> Result<Json<JobsResponse>, ApiError> {
    let user = caller.0;
    let jobs = app
        .market
        .call(move |m| {
            m.jobs
                .jobs()
                .filter(|job| {
                    m.traders
                        .get(&TraderId(job.trader_id))
                        .is_some_and(|t| t.user_id == user)
                })
                .cloned()
                .collect::<Vec<_>>()
        })
        .await?;
    Ok(Json(JobsResponse { jobs }))
}

/// One job by id. Its owner's, or the operator's, to read.
async fn get_job(
    State(app): State<AppState>,
    Path(job_id): Path<u64>,
    caller: Option<Caller>,
    admin: Option<Admin>,
) -> Result<Json<crate::jobs::Job>, ApiError> {
    let job = app
        .market
        .call(move |m| m.jobs.get(job_id).cloned())
        .await?
        .ok_or_else(|| ApiError::job(JobError::UnknownJob(job_id)))?;
    if admin.is_none() {
        let caller = caller.ok_or_else(ApiError::unauthenticated)?;
        owned_trader(&app, caller, TraderId(job.trader_id))?;
    }
    Ok(Json(job))
}

/// Stop a job before it is due. Its owner's to call.
async fn cancel_job(
    State(app): State<AppState>,
    Path(job_id): Path<u64>,
    caller: Caller,
    Idempotency(key): Idempotency,
) -> Result<Committed, ApiError> {
    let owner = app
        .market
        .call(move |m| m.jobs.get(job_id).map(|j| j.trader_id))
        .await?
        .ok_or_else(|| ApiError::job(JobError::UnknownJob(job_id)))?;
    owned_trader(&app, caller, TraderId(owner))?;
    run(
        &app,
        caller.into(),
        key,
        Command::CancelJob {
            trader_id: owner,
            job_id,
        },
    )
    .await
    .map(Committed)
}

// ---------------------------------------------------------------------------
// Budgets and rewards. See [`crate::rewards`].

/// Body of `POST /api/budgets`.
#[derive(Deserialize)]
struct BudgetRequest {
    name: Option<String>,
    /// What to fund it with out of treasury. A transfer, not a mint.
    #[serde(default)]
    cash_cents: i64,
}

/// Body of `POST /api/budgets/{wallet_id}/fund`.
#[derive(Deserialize)]
struct FundRequest {
    amount_cents: i64,
}

/// What the world has set aside, and what it will pay for. Operator
/// authority: this is the game's own budgeting, not a player's business.
async fn get_budgets(
    State(app): State<AppState>,
    _admin: Admin,
) -> Result<Json<BudgetsResponse>, ApiError> {
    let (budgets, rules) = app
        .market
        .call(|m| (m.budget_views(), m.rewards.rules().cloned().collect()))
        .await?;
    Ok(Json(BudgetsResponse { budgets, rules }))
}

/// Open a budget, funded out of treasury.
async fn create_budget(
    State(app): State<AppState>,
    _admin: Admin,
    Idempotency(key): Idempotency,
    payload: Result<Json<BudgetRequest>, JsonRejection>,
) -> Result<Committed, ApiError> {
    let Json(req) = payload.map_err(ApiError::bad_json)?;
    run(
        &app,
        Principal::Operator,
        key,
        Command::CreateBudget {
            name: clean_text(req.name, 64),
            cash_cents: req.cash_cents,
        },
    )
    .await
    .map(Committed)
}

/// Pay more into a budget.
async fn fund_budget(
    State(app): State<AppState>,
    Path(wallet): Path<u64>,
    _admin: Admin,
    Idempotency(key): Idempotency,
    payload: Result<Json<FundRequest>, JsonRejection>,
) -> Result<Committed, ApiError> {
    let Json(req) = payload.map_err(ApiError::bad_json)?;
    run(
        &app,
        Principal::Operator,
        key,
        Command::FundBudget {
            wallet,
            cash_cents: req.amount_cents,
        },
    )
    .await
    .map(Committed)
}

/// Body of `POST /api/rewards/rules`.
#[derive(Deserialize)]
struct RewardRuleRequest {
    /// A short name: `DAILY`, `boss-kill`.
    id: String,
    /// The budget's wallet id.
    budget: u64,
    /// What one payment is worth, in cents.
    amount_cents: i64,
    note: Option<String>,
}

/// Write or replace what a named reward is worth.
async fn set_reward_rule(
    State(app): State<AppState>,
    _admin: Admin,
    Idempotency(key): Idempotency,
    payload: Result<Json<RewardRuleRequest>, JsonRejection>,
) -> Result<Committed, ApiError> {
    let Json(req) = payload.map_err(ApiError::bad_json)?;
    run(
        &app,
        Principal::Operator,
        key,
        Command::SetRewardRule {
            id: req.id,
            budget: req.budget,
            amount_cents: req.amount_cents,
            note: clean_text(req.note, MAX_RECIPE_NOTE),
        },
    )
    .await
    .map(Committed)
}

/// Take a reward rule away. What it has paid stays paid.
async fn remove_reward_rule(
    State(app): State<AppState>,
    Path(id): Path<String>,
    _admin: Admin,
    Idempotency(key): Idempotency,
) -> Result<Committed, ApiError> {
    run(
        &app,
        Principal::Operator,
        key,
        Command::RemoveRewardRule { id },
    )
    .await
    .map(Committed)
}

/// Body of `POST /api/rewards`.
#[derive(Deserialize)]
struct RewardRequest {
    /// The rule that prices it.
    rule: String,
    /// Who is being paid.
    trader_id: u64,
    /// The game's own id for what happened. Paid at most once, whatever the
    /// `Idempotency-Key` says.
    source: String,
}

/// Pay a reward for something the game says happened.
///
/// The game backend's call, so it carries operator authority: a player
/// cannot decide they have finished a quest. A `source` already paid gets
/// the receipt it produced the first time and moves nothing.
async fn pay_reward(
    State(app): State<AppState>,
    _admin: Admin,
    Idempotency(key): Idempotency,
    payload: Result<Json<RewardRequest>, JsonRejection>,
) -> Result<Committed, ApiError> {
    let Json(req) = payload.map_err(ApiError::bad_json)?;
    run(
        &app,
        Principal::Operator,
        key,
        Command::PayReward {
            rule: req.rule,
            trader_id: req.trader_id,
            source: req.source,
        },
    )
    .await
    .map(Committed)
}

// ---------------------------------------------------------------------------
// Transfers, wallets and the world.

/// Body of `POST /api/transfers`.
#[derive(Deserialize)]
struct TransferBody {
    from_account_id: u64,
    to_account_id: u64,
    amount_cents: i64,
    memo: Option<String>,
}

/// Move currency between two accounts. The sender's owner, or the operator.
async fn transfer(
    State(app): State<AppState>,
    caller: Option<Caller>,
    admin: Option<Admin>,
    Idempotency(key): Idempotency,
    payload: Result<Json<TransferBody>, JsonRejection>,
) -> Result<Committed, ApiError> {
    let Json(req) = payload.map_err(ApiError::bad_json)?;
    let from = AccountId(req.from_account_id);
    let principal = if admin.is_some() {
        Principal::Operator
    } else {
        let caller = caller.ok_or_else(ApiError::unauthenticated)?;
        app.market
            .call(move |m| owned_account(m, caller, from))
            .await??;
        caller.into()
    };
    run(
        &app,
        principal,
        key,
        Command::Transfer {
            from_account: req.from_account_id,
            to_account: req.to_account_id,
            amount_cents: req.amount_cents,
            memo: clean_text(req.memo, 140),
        },
    )
    .await
    .map(Committed)
}

/// One wallet: what it is, and what it holds.
#[derive(Serialize)]
pub struct WalletDto {
    pub wallet: fehu::ledger::WalletId,
    /// `player`, `treasury`, `budget`, `npc`, `issuer`, `venue`, `issuance`
    /// or `synthetic`.
    pub kind: &'static str,
    pub status: &'static str,
    pub balance_cents: i64,
    /// Committed to resting buy orders.
    pub reserved_cents: i64,
    /// `balance − reserved`: what can still be spent.
    pub available_cents: i64,
    /// The account this wallet is the money of, if it is somebody's.
    pub account_id: Option<u64>,
}

/// Whether the caller may look into `wallet`: it is their account's, or they
/// are the operator.
fn may_read_wallet(
    m: &Market,
    wallet: fehu::ledger::WalletId,
    caller: Option<Caller>,
    admin: Option<Admin>,
) -> Result<Option<AccountId>, ApiError> {
    let account = m
        .accounts
        .values()
        .find(|a| a.wallet == wallet)
        .map(|a| (a.id, a.user_id));
    if admin.is_some() {
        return Ok(account.map(|(id, _)| id));
    }
    match (account, caller) {
        (Some((id, owner)), Some(caller)) if owner == caller.0 => Ok(Some(id)),
        (_, None) => Err(ApiError::unauthenticated()),
        _ => Err(ApiError::forbidden(format!(
            "wallet {} is not yours to read",
            wallet.0
        ))),
    }
}

/// One wallet's balance, by wallet id.
async fn get_wallet(
    State(app): State<AppState>,
    Path(wallet_id): Path<u64>,
    caller: Option<Caller>,
    admin: Option<Admin>,
) -> Result<Json<WalletDto>, ApiError> {
    let wallet = fehu::ledger::WalletId(wallet_id);
    app.market
        .call(move |m| {
            let account_id = may_read_wallet(m, wallet, caller, admin)?;
            let held = m.ledger.wallet(wallet).ok_or_else(|| {
                ApiError::new(
                    StatusCode::NOT_FOUND,
                    "unknown_wallet",
                    format!("no wallet {wallet_id}"),
                )
            })?;
            Ok(Json(WalletDto {
                wallet,
                kind: held.kind.label(),
                status: held.status.label(),
                balance_cents: held.balance_cents(),
                reserved_cents: held.reserved_cents(),
                available_cents: held.available_cents(),
                account_id: account_id.map(|a| a.0),
            }))
        })
        .await?
}

/// A wallet's movements: the account's ledger, under the wallet's name.
///
/// The ledger keeps no transaction log of its own — a transaction is
/// balanced and gone, and what is retained is each account's view of the
/// postings that touched it — so this is that view, found by wallet.
async fn get_wallet_transactions(
    State(app): State<AppState>,
    Path(wallet_id): Path<u64>,
    caller: Option<Caller>,
    admin: Option<Admin>,
) -> Result<Json<LedgerResponse>, ApiError> {
    let wallet = fehu::ledger::WalletId(wallet_id);
    app.market
        .call(move |m| {
            let account_id = may_read_wallet(m, wallet, caller, admin)?.ok_or_else(|| {
                ApiError::new(
                    StatusCode::NOT_FOUND,
                    "no_account",
                    format!(
                        "wallet {wallet_id} is the world's, not an account's, so it has no \
                            per-account history: read /api/supply and /api/reconcile instead"
                    ),
                )
            })?;
            let account = m
                .accounts
                .get(&account_id)
                .ok_or_else(|| ApiError::unknown_account(account_id.0))?;
            Ok(Json(LedgerResponse {
                account: account_view(m, account),
                entries: account.ledger(100),
            }))
        })
        .await?
}

/// A trader's inventory: what it holds of the world's goods.
#[derive(Serialize)]
pub struct InventoryResponse {
    pub trader_id: u64,
    /// Goods only. A shareholding is a portfolio, and
    /// `/api/users/{id}/holdings` is where that is.
    pub inventory: Vec<HoldingDto>,
}

async fn get_inventory(
    State(app): State<AppState>,
    Path(trader_id): Path<u64>,
    caller: Caller,
) -> Result<Json<InventoryResponse>, ApiError> {
    let trader = TraderId(trader_id);
    owned_trader(&app, caller, trader)?;
    let views = app.views().await;
    let inventory = app
        .market
        .call(move |m| m.trader_holdings(trader, &views, true))
        .await?;
    Ok(Json(InventoryResponse {
        trader_id,
        inventory,
    }))
}

/// What game events are doing to production and demand right now.
async fn world(State(app): State<AppState>) -> Result<Json<WorldResponse>, ApiError> {
    let at = app.clock.now();
    let (modifiers, symbols) = app
        .market
        .call(move |m| {
            (
                m.world.active(at.0).cloned().collect::<Vec<_>>(),
                m.world_view(at),
            )
        })
        .await?;
    Ok(Json(WorldResponse {
        at_ms: at.0,
        modifiers,
        symbols,
    }))
}

/// Body of `POST /api/v1/economy/purchases` and `.../consume`: the same two
/// commands as the trader-addressed routes, with the trader in the body
/// because that is the shape the plan's economy surface names.
#[derive(Deserialize)]
struct GoodsBody {
    trader_id: u64,
    symbol: String,
    qty: u64,
}

async fn purchase_body(
    State(app): State<AppState>,
    caller: Caller,
    Idempotency(key): Idempotency,
    payload: Result<Json<GoodsBody>, JsonRejection>,
) -> Result<Committed, ApiError> {
    let Json(req) = payload.map_err(ApiError::bad_json)?;
    owned_trader(&app, caller, TraderId(req.trader_id))?;
    let symbol = crate::symbol::intern(&req.symbol)
        .ok_or_else(|| ApiError::not_found(&req.symbol))?
        .to_string();
    run(
        &app,
        caller.into(),
        key,
        Command::Purchase {
            trader_id: req.trader_id,
            symbol,
            qty: req.qty,
        },
    )
    .await
    .map(Committed)
}

async fn consume_body(
    State(app): State<AppState>,
    caller: Caller,
    Idempotency(key): Idempotency,
    payload: Result<Json<GoodsBody>, JsonRejection>,
) -> Result<Committed, ApiError> {
    let Json(req) = payload.map_err(ApiError::bad_json)?;
    owned_trader(&app, caller, TraderId(req.trader_id))?;
    let symbol = crate::symbol::intern(&req.symbol)
        .ok_or_else(|| ApiError::not_found(&req.symbol))?
        .to_string();
    run(
        &app,
        caller.into(),
        key,
        Command::Consume {
            trader_id: req.trader_id,
            symbol,
            qty: req.qty,
        },
    )
    .await
    .map(Committed)
}

/// Body of `POST /api/traders/{id}/purchases` and `.../consume`.
#[derive(Deserialize)]
struct GoodsRequest {
    symbol: String,
    qty: u64,
}

/// Buy units of a good at the catalogue price: currency to the good's
/// issuer, units to the buyer, in one command.
async fn purchase(
    State(app): State<AppState>,
    Path(trader_id): Path<u64>,
    caller: Caller,
    Idempotency(key): Idempotency,
    payload: Result<Json<GoodsRequest>, JsonRejection>,
) -> Result<Committed, ApiError> {
    let Json(req) = payload.map_err(ApiError::bad_json)?;
    owned_trader(&app, caller, TraderId(trader_id))?;
    let symbol = crate::symbol::intern(&req.symbol)
        .ok_or_else(|| ApiError::not_found(&req.symbol))?
        .to_string();
    run(
        &app,
        caller.into(),
        key,
        Command::Purchase {
            trader_id,
            symbol,
            qty: req.qty,
        },
    )
    .await
    .map(Committed)
}

/// Use units up. They leave the world and nothing comes back.
async fn consume(
    State(app): State<AppState>,
    Path(trader_id): Path<u64>,
    caller: Caller,
    Idempotency(key): Idempotency,
    payload: Result<Json<GoodsRequest>, JsonRejection>,
) -> Result<Committed, ApiError> {
    let Json(req) = payload.map_err(ApiError::bad_json)?;
    owned_trader(&app, caller, TraderId(trader_id))?;
    let symbol = crate::symbol::intern(&req.symbol)
        .ok_or_else(|| ApiError::not_found(&req.symbol))?
        .to_string();
    run(
        &app,
        caller.into(),
        key,
        Command::Consume {
            trader_id,
            symbol,
            qty: req.qty,
        },
    )
    .await
    .map(Committed)
}

/// Body of `POST /api/symbols/{symbol}/delist`.
#[derive(Deserialize)]
struct DelistRequest {
    /// Paid on every share held, in cents; the last traded price if it is not
    /// given, and `0` for a company that turned out to be worth nothing.
    cents_per_share: Option<i64>,
    note: Option<String>,
    source: Option<String>,
}

/// Delist a symbol: withdraw its book, drop its stops, buy every holder out,
/// and take it off the market.
///
/// This is the counterpart of listing and it is not a cancellation of it:
/// what the symbol did is still on the record — the fills, the ledger
/// entries and the order records all still name it — and the money the shares
/// were worth is credited before the listing goes.
async fn delist_symbol(
    State(app): State<AppState>,
    Path(symbol): Path<String>,
    _admin: Admin,
    Idempotency(key): Idempotency,
    payload: Result<Json<DelistRequest>, JsonRejection>,
) -> Result<Committed, ApiError> {
    let Json(req) = payload.map_err(ApiError::bad_json)?;
    run(
        &app,
        Principal::Operator,
        key,
        Command::Delist {
            symbol,
            cents_per_share: req.cents_per_share,
            source: req.source.unwrap_or_else(|| "api".into()),
            note: req.note,
        },
    )
    .await
    .map(Committed)
}

/// Trim a caller's text, cap it at `max` characters, and treat an empty
/// result as absent.
fn clean_text(value: Option<String>, max: usize) -> Option<String> {
    value
        .map(|v| v.trim().chars().take(max).collect::<String>())
        .filter(|v| !v.is_empty())
}

/// Stop trading in a symbol. It stays stopped until it is resumed.
async fn halt_symbol(
    State(app): State<AppState>,
    Path(symbol): Path<String>,
    _admin: Admin,
    Idempotency(key): Idempotency,
) -> Result<Committed, ApiError> {
    run(&app, Principal::Operator, key, Command::Halt { symbol })
        .await
        .map(Committed)
}

/// Start trading again, whatever stopped it.
async fn resume_symbol(
    State(app): State<AppState>,
    Path(symbol): Path<String>,
    _admin: Admin,
    Idempotency(key): Idempotency,
) -> Result<Committed, ApiError> {
    run(&app, Principal::Operator, key, Command::Resume { symbol })
        .await
        .map(Committed)
}

async fn get_shares(
    State(app): State<AppState>,
    Path(symbol): Path<String>,
    caller: Option<Caller>,
) -> Result<Json<SharesDto>, ApiError> {
    let handle = app
        .symbol(&symbol)
        .ok_or_else(|| ApiError::not_found(&symbol))?;
    Ok(Json(SharesDto::fetch(&app, &handle, caller).await?))
}

#[derive(Deserialize)]
struct BarsQuery {
    interval: Option<String>,
    limit: Option<usize>,
}

fn parse_interval(s: &str) -> Option<Interval> {
    match s.to_ascii_uppercase().as_str() {
        "M1" | "1M" => Some(Interval::M1),
        "M5" | "5M" => Some(Interval::M5),
        "H1" | "1H" => Some(Interval::H1),
        "D1" | "1D" => Some(Interval::D1),
        _ => None,
    }
}

#[derive(Serialize)]
struct BarsResponse {
    symbol: &'static str,
    interval: Interval,
    interval_ms: i64,
    sim_now_ms: i64,
    bars: Vec<Candle>,
}

async fn get_bars(
    State(app): State<AppState>,
    Path(symbol): Path<String>,
    Query(q): Query<BarsQuery>,
) -> Result<Json<BarsResponse>, ApiError> {
    let interval = match q.interval.as_deref() {
        None => Interval::M1,
        Some(s) => parse_interval(s).ok_or_else(|| {
            ApiError::bad_request(format!("unknown interval `{s}`; use M1, M5, H1 or D1"))
        })?,
    };
    let limit = q.limit.unwrap_or(500).clamp(1, app.options.max_bars + 1);
    let handle = app
        .symbol(&symbol)
        .ok_or_else(|| ApiError::not_found(&symbol))?;
    let bars = handle
        .ask_listed(move |s| s.bars(interval, limit))
        .await?
        .ok_or_else(|| ApiError::not_found(&symbol))?;
    Ok(Json(BarsResponse {
        symbol: handle.ticker,
        interval,
        interval_ms: interval.millis(),
        sim_now_ms: app.clock.now().0,
        bars,
    }))
}

#[derive(Deserialize)]
struct EventsQuery {
    limit: Option<usize>,
    symbol: Option<String>,
}

#[derive(Serialize)]
struct EventsResponse {
    sim_now_ms: i64,
    /// Newest first.
    events: Vec<EventRecord>,
}

async fn list_events(
    State(app): State<AppState>,
    Query(q): Query<EventsQuery>,
) -> Result<Json<EventsResponse>, ApiError> {
    let limit = q.limit.unwrap_or(100).clamp(1, app.options.event_log);
    let events = app
        .market
        .call(move |m| {
            m.events(limit, |e| match &q.symbol {
                Some(sym) => e.symbols.iter().any(|s| s.eq_ignore_ascii_case(sym)),
                None => true,
            })
        })
        .await?;
    Ok(Json(EventsResponse {
        sim_now_ms: app.clock.now().0,
        events,
    }))
}

async fn list_symbol_events(
    State(app): State<AppState>,
    Path(symbol): Path<String>,
    Query(q): Query<EventsQuery>,
) -> Result<Json<EventsResponse>, ApiError> {
    if app.symbol(&symbol).is_none() {
        return Err(ApiError::not_found(&symbol));
    }
    list_events(
        State(app),
        Query(EventsQuery {
            limit: q.limit,
            symbol: Some(symbol),
        }),
    )
    .await
}

async fn push_sim_event(
    State(app): State<AppState>,
    Path(symbol): Path<String>,
    _admin: Admin,
    Idempotency(key): Idempotency,
    payload: Result<Json<PushEventRequest>, JsonRejection>,
) -> Result<Committed, ApiError> {
    let Json(req) = payload.map_err(ApiError::bad_json)?;
    // Rejected here as well as in the command, so a malformed event is
    // answered without waiting on the market.
    req.event
        .prepare()
        .map_err(|e| ApiError::invalid_event(e.to_string()))?;
    let at = req
        .timing
        .resolve(app.clock.now())
        .map_err(ApiError::bad_request)?;
    run_at(
        &app,
        Principal::Operator,
        key,
        at,
        Command::SimEvent {
            symbol,
            event: req.event,
            source: req.source.unwrap_or_else(|| "api".into()),
            note: req.note,
        },
    )
    .await
    .map(Committed)
}

async fn push_game_event(
    State(app): State<AppState>,
    _admin: Admin,
    Idempotency(key): Idempotency,
    payload: Result<Json<GameEventRequest>, JsonRejection>,
) -> Result<Committed, ApiError> {
    let Json(req) = payload.map_err(ApiError::bad_json)?;
    let at = req
        .timing
        .resolve(app.clock.now())
        .map_err(ApiError::bad_request)?;
    run_at(
        &app,
        Principal::Operator,
        key,
        at,
        Command::GameEvent {
            kind: req.kind,
            magnitude: req.magnitude.unwrap_or(1.0),
            symbol: req.symbol,
            source: req.source.unwrap_or_else(|| "game".into()),
            note: req.note,
        },
    )
    .await
    .map(Committed)
}

// ---------------------------------------------------------------------------
// Trading

#[derive(Deserialize)]
struct DepthQuery {
    depth: Option<usize>,
}

#[derive(Serialize)]
struct BookResponse {
    symbol: &'static str,
    ts_ms: i64,
    reference_cents: i64,
    bid_cents: Option<i64>,
    ask_cents: Option<i64>,
    /// Traders' net flow the next tick will price in.
    pending_flow: i64,
    #[serde(flatten)]
    book: BookDto,
}

async fn get_book(
    State(app): State<AppState>,
    Path(symbol): Path<String>,
    Query(q): Query<DepthQuery>,
) -> Result<Json<BookResponse>, ApiError> {
    let depth = q.depth.unwrap_or(10).clamp(1, 200);
    let handle = app
        .symbol(&symbol)
        .ok_or_else(|| ApiError::not_found(&symbol))?;
    handle
        .ask_listed(move |s| {
            let b = s.exchange.book();
            BookResponse {
                symbol: s.info.symbol,
                ts_ms: s.exchange.clock().0,
                reference_cents: s.exchange.reference_cents(),
                bid_cents: b.best_bid(),
                ask_cents: b.best_ask(),
                pending_flow: s.exchange.pending_flow(),
                book: s.book(depth),
            }
        })
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::not_found(&symbol))
}

#[derive(Deserialize)]
struct LimitQuery {
    limit: Option<usize>,
}

#[derive(Serialize)]
struct TradesResponse {
    symbol: &'static str,
    sim_now_ms: i64,
    /// Newest first.
    trades: Vec<TradeDto>,
}

async fn get_trades(
    State(app): State<AppState>,
    Path(symbol): Path<String>,
    Query(q): Query<LimitQuery>,
) -> Result<Json<TradesResponse>, ApiError> {
    let limit = q.limit.unwrap_or(50).clamp(1, app.options.tape_len.max(1));
    let handle = app
        .symbol(&symbol)
        .ok_or_else(|| ApiError::not_found(&symbol))?;
    let trades = handle
        .ask_listed(move |s| {
            s.tape
                .iter()
                .rev()
                .take(limit)
                .map(TradeDto::from)
                .collect()
        })
        .await?
        .ok_or_else(|| ApiError::not_found(&symbol))?;
    Ok(Json(TradesResponse {
        symbol: handle.ticker,
        sim_now_ms: app.clock.now().0,
        trades,
    }))
}

/// Sign a player up: with no `user_id` this creates a user, opens an account
/// funded with `cash_cents` and gives it a trader. With one, the trader joins
/// that user, on an existing `account_id` or on a fresh account.
async fn create_trader(
    State(app): State<AppState>,
    caller: Option<Caller>,
    Idempotency(key): Idempotency,
    payload: Option<Json<CreateTraderRequest>>,
) -> Result<Committed, ApiError> {
    let req = payload.map(|Json(r)| r).unwrap_or_default();
    let cash_cents = req.cash_cents.unwrap_or(app.options.starting_cash_cents);
    if cash_cents < 0 {
        return Err(ApiError::bad_request("`cash_cents` must not be negative"));
    }
    let email = check_email(req.email)?;
    // Joining an existing user is that user's business alone; a sign-up
    // belongs to nobody yet and needs a key of its own.
    let (owner, api_key) = match req.user_id {
        Some(user_id) => {
            let caller = caller.ok_or_else(ApiError::unauthenticated)?;
            owned_user(caller, UserId(user_id))?;
            (Some(caller.0.0), None)
        }
        None => (None, Some(crate::auth::new_api_key())),
    };
    let out = run(
        &app,
        owner.map_or(Principal::Anonymous, |id| Principal::User { id }),
        key,
        Command::CreateTrader {
            caller: owner,
            user_id: req.user_id,
            account_id: req.account_id,
            name: req.name,
            email,
            cash_cents,
            key_digest: api_key.as_deref().map(crate::auth::key_digest),
        },
    )
    .await?;
    Ok(match api_key {
        Some(api_key) => with_key(out, api_key),
        None => Committed(out),
    })
}

/// An email is optional, but if given it has to look like one.
fn check_email(email: Option<String>) -> Result<Option<String>, ApiError> {
    let Some(email) = email else { return Ok(None) };
    let email = email.trim();
    if email.is_empty() {
        return Ok(None);
    }
    let local_and_domain = email.split_once('@');
    match local_and_domain {
        Some((local, domain))
            if !local.is_empty() && domain.contains('.') && !domain.starts_with('.') =>
        {
            Ok(Some(email.to_owned()))
        }
        _ => Err(ApiError::bad_request(format!(
            "`email` does not look like an address: {email}"
        ))),
    }
}

/// The caller's own traders.
async fn list_traders(
    State(app): State<AppState>,
    caller: Caller,
) -> Result<Json<Vec<TraderSummary>>, ApiError> {
    let views = app.views().await;
    let traders = app
        .market
        .call(move |m| {
            m.traders
                .values()
                .filter(|t| t.user_id == caller.0)
                .map(|t| {
                    let account = m.accounts.get(&t.account_id);
                    let cash = account.map_or(0, |a| a.balance_cents(&m.ledger));
                    let equity = cash.saturating_add(
                        t.positions
                            .iter()
                            .map(|(sym, p)| p.market_value_cents(mark_of(&views, sym)))
                            .fold(0i64, i64::saturating_add),
                    );
                    TraderSummary {
                        id: t.id.0,
                        user_id: t.user_id.0,
                        account_id: t.account_id.0,
                        name: t.name.clone(),
                        account_status: account
                            .map_or_else(Default::default, |a| a.status(&m.ledger)),
                        cash_cents: cash,
                        equity_cents: equity,
                        positions: t.positions.len(),
                        open_orders: views
                            .iter()
                            .flat_map(|v| v.open_orders.iter())
                            .filter(|o| o.trader_id == t.id.0)
                            .count(),
                    }
                })
                .collect::<Vec<_>>()
        })
        .await?;
    Ok(Json(traders))
}

async fn get_trader(
    State(app): State<AppState>,
    Path(trader_id): Path<u64>,
    caller: Caller,
) -> Result<Json<PortfolioDto>, ApiError> {
    let trader = TraderId(trader_id);
    owned_trader(&app, caller, trader)?;
    Ok(Json(fetch_portfolio(&app, trader).await?))
}

/// The reference price of `sym` in `views`, or zero for a symbol no longer
/// listed.
pub(crate) fn mark_of(views: &[SymbolView], sym: &str) -> i64 {
    views
        .iter()
        .find(|v| v.symbol.eq_ignore_ascii_case(sym))
        .map_or(0, |v| v.mark_cents)
}

/// A trader's whole position: the symbols' side first (marks, resting
/// orders, stops), then the market's (the trader, the account).
async fn fetch_portfolio(app: &App, id: TraderId) -> Result<PortfolioDto, ApiError> {
    let views = app.views().await;
    app.market.call(move |m| portfolio(m, &views, id)).await?
}

pub(crate) fn portfolio(
    market: &Market,
    views: &[SymbolView],
    id: TraderId,
) -> Result<PortfolioDto, ApiError> {
    let t = market
        .traders
        .get(&id)
        .ok_or_else(|| ApiError::unknown_trader(id.0))?;
    let account = market
        .accounts
        .get(&t.account_id)
        .ok_or_else(|| ApiError::unknown_account(t.account_id.0))?;
    let mut positions = Vec::new();
    let mut value = 0i64;
    let mut unrealised = 0i64;
    let mut realised = 0i64;
    for (sym, p) in &t.positions {
        let mark = mark_of(views, sym);
        let mv = p.market_value_cents(mark);
        let u = p.unrealised_pnl_cents(mark);
        value = value.saturating_add(mv);
        unrealised = unrealised.saturating_add(u);
        realised = realised.saturating_add(p.realised_pnl_cents);
        positions.push(PositionDto {
            symbol: sym,
            qty: p.qty,
            avg_cost_cents: p.avg_cost_cents(),
            mark_cents: mark,
            market_value_cents: mv,
            unrealised_pnl_cents: u,
            realised_pnl_cents: p.realised_pnl_cents,
            reserved_shares: t.reserved_shares.get(sym).copied().unwrap_or(0),
            free_shares: t.free_shares(sym),
        });
    }
    let open_orders = views
        .iter()
        .flat_map(|v| v.open_orders.iter())
        .filter(|o| o.trader_id == id.0)
        .cloned()
        .collect();
    Ok(PortfolioDto {
        id: t.id.0,
        user_id: t.user_id.0,
        account_id: t.account_id.0,
        name: t.name.clone(),
        created_at_ms: t.created_at_ms,
        account_status: account.status(&market.ledger),
        cash_cents: account.balance_cents(&market.ledger),
        reserved_cents: account.reserved_cents(&market.ledger),
        free_cash_cents: account.available_cents(&market.ledger),
        equity_cents: account.balance_cents(&market.ledger).saturating_add(value),
        realised_pnl_cents: realised,
        unrealised_pnl_cents: unrealised,
        positions,
        open_orders,
        stops: views
            .iter()
            .flat_map(|v| v.stops.iter())
            .filter(|s| s.trader_id == id.0)
            .cloned()
            .collect(),
        fills: t.fills.iter().rev().cloned().collect(),
        // Only the response that creates a user carries their key.
        api_key: None,
    })
}

async fn submit_order(
    State(app): State<AppState>,
    Path(symbol): Path<String>,
    caller: Caller,
    Idempotency(key): Idempotency,
    payload: Result<Json<OrderRequest>, JsonRejection>,
) -> Result<Committed, ApiError> {
    let Json(req) = payload.map_err(ApiError::bad_json)?;
    owned_trader(&app, caller, TraderId(req.trader_id))?;
    let order = OrderRequest {
        client_order_id: clean_client_order_id(req.client_order_id)?,
        ..req
    };
    run(
        &app,
        caller.into(),
        key,
        Command::PlaceOrder { symbol, order },
    )
    .await
    .map(Committed)
}

#[derive(Deserialize)]
struct TraderQuery {
    trader_id: u64,
}

/// Body of `POST /api/symbols/{symbol}/dividend`.
#[derive(Deserialize)]
struct DividendRequest {
    /// Paid on every share held, in cents. Must be positive and below the
    /// price: a company cannot pay out more than it is worth.
    cents_per_share: i64,
    note: Option<String>,
    source: Option<String>,
}

/// Declare a dividend: pay every holder, and take the price ex.
///
/// The money and the price move together. Paying without the price falling
/// would be money from nothing — hold over the record, collect, sell — so the
/// reference and the fundamental both drop by the dividend. The shares
/// themselves are untouched: nothing is created or destroyed.
async fn pay_dividend(
    State(app): State<AppState>,
    Path(symbol): Path<String>,
    _admin: Admin,
    Idempotency(key): Idempotency,
    payload: Result<Json<DividendRequest>, JsonRejection>,
) -> Result<Committed, ApiError> {
    let Json(req) = payload.map_err(ApiError::bad_json)?;
    run(
        &app,
        Principal::Operator,
        key,
        Command::Dividend {
            symbol,
            cents_per_share: req.cents_per_share,
            source: req.source.unwrap_or_else(|| "api".into()),
            note: req.note,
        },
    )
    .await
    .map(Committed)
}

/// Arm a stop: a trigger the engine watches, not an order in the book.
///
/// Unlike an order this is accepted while the symbol is halted or its session
/// is closed — the trigger simply waits, and fires when trading resumes.
async fn submit_stop(
    State(app): State<AppState>,
    Path(symbol): Path<String>,
    caller: Caller,
    Idempotency(key): Idempotency,
    payload: Result<Json<StopRequest>, JsonRejection>,
) -> Result<Committed, ApiError> {
    let Json(req) = payload.map_err(ApiError::bad_json)?;
    owned_trader(&app, caller, TraderId(req.trader_id))?;
    let stop = StopRequest {
        client_order_id: clean_client_order_id(req.client_order_id)?,
        ..req
    };
    run(
        &app,
        caller.into(),
        key,
        Command::PlaceStop { symbol, stop },
    )
    .await
    .map(Committed)
}

/// A trader's stops on one symbol. Stops are private: they say what a trader
/// intends to do, which is nobody else's business.
async fn list_stops(
    State(app): State<AppState>,
    Path(symbol): Path<String>,
    Query(q): Query<TraderQuery>,
    caller: Caller,
) -> Result<Json<Vec<StopOrder>>, ApiError> {
    let trader = TraderId(q.trader_id);
    owned_trader(&app, caller, trader)?;
    let handle = app
        .symbol(&symbol)
        .ok_or_else(|| ApiError::not_found(&symbol))?;
    handle
        .ask_listed(move |s| s.stops_of(trader))
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::not_found(&symbol))
}

/// Every stop a trader holds, across all symbols.
async fn list_trader_stops(
    State(app): State<AppState>,
    Path(trader_id): Path<u64>,
    caller: Caller,
) -> Result<Json<Vec<StopOrder>>, ApiError> {
    let trader = TraderId(trader_id);
    owned_trader(&app, caller, trader)?;
    Ok(Json(app.stops_of(trader).await))
}

/// Withdraw a stop before it fires. The stop is looked for on the symbol
/// named, which is the one it was armed on.
async fn cancel_stop(
    State(app): State<AppState>,
    Path((symbol, stop_id)): Path<(String, u64)>,
    Query(q): Query<TraderQuery>,
    caller: Caller,
    Idempotency(key): Idempotency,
) -> Result<Committed, ApiError> {
    let trader = TraderId(q.trader_id);
    owned_trader(&app, caller, trader)?;
    run(
        &app,
        caller.into(),
        key,
        Command::CancelStop {
            symbol,
            trader_id: trader.0,
            stop_id,
        },
    )
    .await
    .map(Committed)
}

/// Trim a caller-supplied `client_order_id`; an empty one counts as absent.
fn clean_client_order_id(value: Option<String>) -> Result<Option<String>, ApiError> {
    let Some(id) = value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
    else {
        return Ok(None);
    };
    if id.chars().count() > MAX_CLIENT_ORDER_ID {
        return Err(ApiError::bad_request(format!(
            "client_order_id is longer than {MAX_CLIENT_ORDER_ID} characters"
        )));
    }
    Ok(Some(id))
}

#[derive(Deserialize)]
struct OrderHistoryQuery {
    /// `resting`, `filled` or `cancelled`; omit for every order.
    status: Option<String>,
    limit: Option<usize>,
}

/// One order by id, whatever became of it — filled and cancelled orders
/// included, for as long as the order log holds them.
async fn get_order_record(
    State(app): State<AppState>,
    Path(order_id): Path<u64>,
    caller: Caller,
) -> Result<Json<OrderRecord>, ApiError> {
    let record = app
        .market
        .call(move |m| m.order(order_id).cloned())
        .await?
        .ok_or_else(|| ApiError::unknown_order(order_id))?;
    owned_trader(&app, caller, TraderId(record.trader_id))?;
    Ok(Json(record))
}

/// A trader's orders, newest first.
async fn list_trader_orders(
    State(app): State<AppState>,
    Path(trader_id): Path<u64>,
    Query(q): Query<OrderHistoryQuery>,
    caller: Caller,
) -> Result<Json<Vec<OrderRecord>>, ApiError> {
    let trader = TraderId(trader_id);
    let status = match q.status.as_deref() {
        None => None,
        Some("resting" | "open" | "live") => Some(fehu::OrderStatus::Resting),
        Some("filled") => Some(fehu::OrderStatus::Filled),
        Some("cancelled" | "canceled") => Some(fehu::OrderStatus::Cancelled),
        Some(other) => {
            return Err(ApiError::bad_request(format!(
                "unknown status {other:?}: use resting, filled or cancelled"
            )));
        }
    };
    owned_trader(&app, caller, trader)?;
    let limit = q.limit.unwrap_or(100).clamp(1, 1_000);
    Ok(Json(
        app.market
            .call(move |m| {
                m.orders_of(trader)
                    .filter(|o| status.is_none_or(|s| o.status == s))
                    .take(limit)
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .await?,
    ))
}

async fn list_orders(
    State(app): State<AppState>,
    Path(symbol): Path<String>,
    Query(q): Query<TraderQuery>,
    caller: Caller,
) -> Result<Json<Vec<OpenOrderDto>>, ApiError> {
    let trader = TraderId(q.trader_id);
    owned_trader(&app, caller, trader)?;
    let handle = app
        .symbol(&symbol)
        .ok_or_else(|| ApiError::not_found(&symbol))?;
    handle
        .ask_listed(move |s| {
            s.exchange
                .book()
                .orders_of(Owner::Trader(trader))
                .map(|o| OpenOrderDto::from_resting(s.info.symbol, o))
                .collect::<Vec<_>>()
        })
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::not_found(&symbol))
}

async fn get_order(
    State(app): State<AppState>,
    Path((symbol, order_id)): Path<(String, u64)>,
    caller: Caller,
) -> Result<Json<OpenOrderDto>, ApiError> {
    let handle = app
        .symbol(&symbol)
        .ok_or_else(|| ApiError::not_found(&symbol))?;
    let dto = handle
        .ask_listed(move |s| {
            s.exchange
                .book()
                .get(fehu::OrderId(order_id))
                .filter(|o| o.owner != Owner::Synthetic)
                .map(|o| OpenOrderDto::from_resting(s.info.symbol, o))
        })
        .await?
        .ok_or_else(|| ApiError::not_found(&symbol))?
        .ok_or_else(|| ApiError::unknown_order(order_id))?;
    owned_trader(&app, caller, TraderId(dto.trader_id))?;
    Ok(Json(dto))
}

/// Replace a resting order with another at a new price or quantity.
///
/// This is a cancel and a fresh order, in that order and in one job on the
/// market: the replacement goes to the back of the queue at its price, and if
/// it cannot be placed — no cash, no shares, a halt — the old order is
/// already gone.
/// The response says which order was withdrawn and how much of it had filled.
async fn amend_order(
    State(app): State<AppState>,
    Path((symbol, order_id)): Path<(String, u64)>,
    caller: Caller,
    Idempotency(key): Idempotency,
    payload: Result<Json<AmendRequest>, JsonRejection>,
) -> Result<Committed, ApiError> {
    let Json(req) = payload.map_err(ApiError::bad_json)?;
    owned_trader(&app, caller, TraderId(req.trader_id))?;
    let amend = AmendRequest {
        client_order_id: clean_client_order_id(req.client_order_id)?,
        ..req
    };
    run(
        &app,
        caller.into(),
        key,
        Command::AmendOrder {
            symbol,
            order_id,
            amend,
        },
    )
    .await
    .map(Committed)
}

async fn cancel_order(
    State(app): State<AppState>,
    Path((symbol, order_id)): Path<(String, u64)>,
    Query(q): Query<TraderQuery>,
    caller: Caller,
    Idempotency(key): Idempotency,
) -> Result<Committed, ApiError> {
    let trader = TraderId(q.trader_id);
    owned_trader(&app, caller, trader)?;
    run(
        &app,
        caller.into(),
        key,
        Command::CancelOrder {
            symbol,
            trader_id: trader.0,
            order_id,
        },
    )
    .await
    .map(Committed)
}

async fn cancel_all(
    State(app): State<AppState>,
    Path(trader_id): Path<u64>,
    caller: Caller,
    Idempotency(key): Idempotency,
) -> Result<Committed, ApiError> {
    owned_trader(&app, caller, TraderId(trader_id))?;
    run(&app, caller.into(), key, Command::CancelAll { trader_id })
        .await
        .map(Committed)
}

// ---------------------------------------------------------------------------
// Users and accounts
//
// A user is the person, an account holds their money, and a trader trades on
// exactly one account. Every amount here is an integer number of cents.

async fn create_user(
    State(app): State<AppState>,
    Idempotency(key): Idempotency,
    payload: Option<Json<CreateUserRequest>>,
) -> Result<Committed, ApiError> {
    let req = payload.map(|Json(r)| r).unwrap_or_default();
    let email = check_email(req.email)?;
    let api_key = crate::auth::new_api_key();
    let out = run(
        &app,
        Principal::Anonymous,
        key,
        Command::CreateUser {
            name: req.name,
            email,
            key_digest: crate::auth::key_digest(&api_key),
        },
    )
    .await?;
    Ok(with_key(out, api_key))
}

/// The caller, as a list of one: a user is not told about the others.
async fn list_users(
    State(app): State<AppState>,
    caller: Caller,
) -> Result<Json<Vec<UserDto>>, ApiError> {
    let views = app.views().await;
    let users = app
        .market
        .call(move |m| {
            user_dto(m, &views, caller.0)
                .into_iter()
                .collect::<Vec<_>>()
        })
        .await?;
    Ok(Json(users))
}

async fn get_user(
    State(app): State<AppState>,
    Path(user_id): Path<u64>,
    caller: Caller,
) -> Result<Json<UserDto>, ApiError> {
    let user = UserId(user_id);
    owned_user(caller, user)?;
    let views = app.views().await;
    Ok(Json(
        app.market
            .call(move |m| user_dto(m, &views, user))
            .await??,
    ))
}

/// Every share the user owns, per symbol, across all of their traders.
async fn get_holdings(
    State(app): State<AppState>,
    Path(user_id): Path<u64>,
    caller: Caller,
) -> Result<Json<UserHoldingsResponse>, ApiError> {
    let user = UserId(user_id);
    owned_user(caller, user)?;
    let views = app.views().await;
    let holdings = app
        .market
        .call(move |m| {
            if !m.users.contains_key(&user) {
                return Err(ApiError::unknown_user(user_id));
            }
            Ok(m.user_holdings(user, &views))
        })
        .await??;
    Ok(Json(UserHoldingsResponse::new(user_id, holdings)))
}

/// Open another account for a user, with `cash_cents` paid in.
async fn open_account(
    State(app): State<AppState>,
    Path(user_id): Path<u64>,
    caller: Caller,
    Idempotency(key): Idempotency,
    payload: Option<Json<OpenAccountRequest>>,
) -> Result<Committed, ApiError> {
    let req = payload.map(|Json(r)| r).unwrap_or_default();
    owned_user(caller, UserId(user_id))?;
    run(
        &app,
        caller.into(),
        key,
        Command::OpenAccount {
            user_id,
            name: req.name,
            cash_cents: req.cash_cents.unwrap_or(app.options.starting_cash_cents),
        },
    )
    .await
    .map(Committed)
}

async fn list_user_accounts(
    State(app): State<AppState>,
    Path(user_id): Path<u64>,
    caller: Caller,
) -> Result<Json<Vec<AccountDto>>, ApiError> {
    let user = UserId(user_id);
    owned_user(caller, user)?;
    Ok(Json(
        app.market
            .call(move |m| {
                if !m.users.contains_key(&user) {
                    return Err(ApiError::unknown_user(user_id));
                }
                Ok(m.accounts
                    .values()
                    .filter(|a| a.user_id == user)
                    .map(|a| account_view(m, a))
                    .collect::<Vec<_>>())
            })
            .await??,
    ))
}

/// The caller's own accounts.
async fn list_accounts(
    State(app): State<AppState>,
    caller: Caller,
) -> Result<Json<Vec<AccountDto>>, ApiError> {
    Ok(Json(
        app.market
            .call(move |m| {
                m.accounts
                    .values()
                    .filter(|a| a.user_id == caller.0)
                    .map(|a| account_view(m, a))
                    .collect::<Vec<_>>()
            })
            .await?,
    ))
}

async fn get_account(
    State(app): State<AppState>,
    Path(account_id): Path<u64>,
    caller: Caller,
) -> Result<Json<AccountDto>, ApiError> {
    let id = AccountId(account_id);
    Ok(Json(
        app.market
            .call(move |m| {
                owned_account(m, caller, id)?;
                account_dto(m, id)
            })
            .await??,
    ))
}

/// Mint money into an account: `{"amount_cents": 500000}`. The amount is a
/// positive integer number of cents.
///
/// **Operator authority.** This is one of the two routes that changes how
/// much currency exists — it creates it — so it is gated the way the
/// game-master routes are. A player cannot reach it, and no route a player
/// can reach moves the supply at all.
async fn deposit(
    State(app): State<AppState>,
    Path(account_id): Path<u64>,
    _admin: Admin,
    Idempotency(key): Idempotency,
    payload: Result<Json<TransferRequest>, JsonRejection>,
) -> Result<Committed, ApiError> {
    let Json(req) = payload.map_err(ApiError::bad_json)?;
    run(
        &app,
        Principal::Operator,
        key,
        Command::Mint {
            account_id,
            amount_cents: req.amount_cents,
            memo: req.memo,
        },
    )
    .await
    .map(Committed)
}

/// Burn money out of an account. Only the available balance can leave: cash
/// reserved for resting orders has to be freed by cancelling them first.
///
/// **Operator authority**, and the mirror of [`deposit`]: this is the only
/// way currency leaves the world.
async fn withdraw(
    State(app): State<AppState>,
    Path(account_id): Path<u64>,
    _admin: Admin,
    Idempotency(key): Idempotency,
    payload: Result<Json<TransferRequest>, JsonRejection>,
) -> Result<Committed, ApiError> {
    let Json(req) = payload.map_err(ApiError::bad_json)?;
    run(
        &app,
        Principal::Operator,
        key,
        Command::Burn {
            account_id,
            amount_cents: req.amount_cents,
            memo: req.memo,
        },
    )
    .await
    .map(Committed)
}

/// Freeze, reopen or close an account: `{"status": "frozen"}`.
///
/// Two transitions with two different authorities, behind one route.
///
/// **Freezing and unfreezing are the operator's**: an account that could
/// unfreeze itself is not frozen. Freezing withdraws the account's resting
/// orders and stops in the same job, because an order that outlived a freeze
/// would fill against a wallet that can no longer pay it.
///
/// **Closing is the owner's**, and needs no authority beyond owning it — but
/// it does need an empty wallet, holding nothing and reserving nothing, so
/// that closing an account can never strand currency out of reach.
async fn set_status(
    State(app): State<AppState>,
    Path(account_id): Path<u64>,
    caller: Option<Caller>,
    admin: Option<Admin>,
    Idempotency(key): Idempotency,
    payload: Option<Json<StatusRequest>>,
) -> Result<Committed, ApiError> {
    let Json(req) = payload.ok_or_else(|| ApiError::bad_request("a `status` is required"))?;
    // Who has to be who depends on which transition this is. An operator
    // freezes an account that is not theirs — that is the whole point of a
    // freeze — so they are not asked to own it; an owner closes theirs, and
    // is not asked to be an operator.
    let (owner, principal) = match req.status {
        AccountStatus::Frozen | AccountStatus::Active => {
            if admin.is_none() {
                return Err(ApiError::forbidden(
                    "freezing and unfreezing an account is the operator's to do",
                ));
            }
            (None, Principal::Operator)
        }
        AccountStatus::Closed => {
            let caller = caller.ok_or_else(ApiError::unauthenticated)?;
            (Some(caller.0.0), caller.into())
        }
    };
    run(
        &app,
        principal,
        key,
        Command::SetAccountStatus {
            account_id,
            status: req.status,
            owner,
        },
    )
    .await
    .map(Committed)
}

/// Check an account: its status, what it can do, and any broken invariant.
async fn validate_account(
    State(app): State<AppState>,
    Path(account_id): Path<u64>,
    caller: Caller,
) -> Result<Json<AccountCheck>, ApiError> {
    let id = AccountId(account_id);
    Ok(Json(
        app.market
            .call(move |m| {
                owned_account(m, caller, id)?;
                let account = m
                    .accounts
                    .get(&id)
                    .ok_or_else(|| ApiError::unknown_account(account_id))?;
                Ok::<_, ApiError>(AccountCheck::new(account, &m.ledger))
            })
            .await??,
    ))
}

/// Every movement of money through an account, newest first.
async fn get_ledger(
    State(app): State<AppState>,
    Path(account_id): Path<u64>,
    Query(q): Query<LimitQuery>,
    caller: Caller,
) -> Result<Json<LedgerResponse>, ApiError> {
    let limit = q
        .limit
        .unwrap_or(100)
        .clamp(1, app.options.ledger_log.max(1));
    let id = AccountId(account_id);
    Ok(Json(
        app.market
            .call(move |m| {
                owned_account(m, caller, id)?;
                let account = m
                    .accounts
                    .get(&id)
                    .ok_or_else(|| ApiError::unknown_account(account_id))?;
                Ok::<_, ApiError>(LedgerResponse {
                    account: account_view(m, account),
                    entries: account.ledger(limit),
                })
            })
            .await??,
    ))
}

/// Add money to the account a trader trades on, without having to look its
/// account up first.
async fn trader_deposit(
    State(app): State<AppState>,
    Path(trader_id): Path<u64>,
    _admin: Admin,
    Idempotency(key): Idempotency,
    payload: Result<Json<TransferRequest>, JsonRejection>,
) -> Result<Committed, ApiError> {
    let Json(req) = payload.map_err(ApiError::bad_json)?;
    run(
        &app,
        Principal::Operator,
        key,
        Command::MintToTrader {
            trader_id,
            amount_cents: req.amount_cents,
            memo: req.memo,
        },
    )
    .await
    .map(Committed)
}

pub(crate) fn user_dto(
    market: &Market,
    views: &[SymbolView],
    id: UserId,
) -> Result<UserDto, ApiError> {
    let user = market
        .users
        .get(&id)
        .ok_or_else(|| ApiError::unknown_user(id.0))?;
    let holdings = market.user_holdings(id, views);
    Ok(UserDto {
        id: user.id.0,
        name: user.name.clone(),
        email: user.email.clone(),
        created_at_ms: user.created_at_ms,
        accounts: user.accounts.iter().map(|a| a.0).collect(),
        traders: market
            .traders
            .values()
            .filter(|t| t.user_id == id)
            .map(|t| t.id.0)
            .collect(),
        balance_cents: user
            .accounts
            .iter()
            .filter_map(|a| market.accounts.get(a))
            .map(|a| a.balance_cents(&market.ledger))
            .fold(0i64, i64::saturating_add),
        shares_owned: holdings
            .iter()
            .map(|h| h.qty)
            .fold(0u64, u64::saturating_add),
        holdings_value_cents: holdings
            .iter()
            .map(|h| h.market_value_cents)
            .fold(0i64, i64::saturating_add),
        // Only the response that creates a user carries their key.
        api_key: None,
    })
}

pub(crate) fn account_dto(market: &Market, id: AccountId) -> Result<AccountDto, ApiError> {
    let account = market
        .accounts
        .get(&id)
        .ok_or_else(|| ApiError::unknown_account(id.0))?;
    Ok(account_view(market, account))
}

pub(crate) fn account_view(market: &Market, account: &Account) -> AccountDto {
    AccountDto::new(
        account,
        &market.ledger,
        market.trader_on(account.id).map(|t| t.id.0),
    )
}

/// `GET /api/commands/{key}`: the answer a command was given, for a client
/// that lost it.
#[derive(Serialize)]
struct CommandDto {
    /// The `Idempotency-Key` it was sent under.
    key: String,
    /// Where it landed in the journal.
    seq: u64,
    /// Which command it was: `place_order`, `mint`, and so on.
    command: String,
    /// The HTTP status it was answered with.
    status: u16,
    received_at_ms: i64,
    /// The response body, as it was sent — except for a credential, which is
    /// shown once and is not kept. A replayed sign-up carries no `api_key`.
    result: serde_json::Value,
}

/// Recover the result of a command whose response was lost.
///
/// Who may ask: the operator, for anything; a player, for their own
/// commands. A command sent by nobody — a sign-up, which has no credential
/// yet — is answered to whoever knows its key, because there is no other
/// way to recover one; the key is the only thing proving it was theirs, so
/// a client that wants that recovery has to choose keys nobody can guess.
async fn get_command(
    State(app): State<AppState>,
    Path(key): Path<String>,
    caller: Option<Caller>,
    admin: Option<Admin>,
) -> Result<Json<CommandDto>, ApiError> {
    let unknown = || {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "unknown_command",
            "no command was recorded under that key (it may never have been sent, \
             may have been refused, or may have been evicted from the log)",
        )
    };
    let record = app
        .market
        .call(move |m| m.commands.get(&key).cloned())
        .await?
        .ok_or_else(unknown)?;
    let mine = match record.principal {
        Principal::Anonymous => true,
        Principal::User { id } => caller.is_some_and(|c| c.0.0 == id),
        Principal::Operator | Principal::Engine => admin.is_some(),
    };
    if !mine {
        // Not "forbidden": whether a key exists is itself the answer to a
        // question the caller has no business asking.
        return Err(unknown());
    }
    Ok(Json(CommandDto {
        key: record.key,
        seq: record.seq,
        command: record.kind,
        status: record.status,
        received_at_ms: record.wall_ms,
        result: record.result,
    }))
}

/// How much currency exists and where it is: `GET /api/supply`.
///
/// The public face of the conservation invariant. `circulating_cents` is
/// what every wallet but issuance actually holds, `outstanding_cents` is
/// minted less burned, and in a healthy world they are the same number —
/// which is exactly what makes the claim checkable by anyone rather than
/// promised by the server.
async fn supply(State(app): State<AppState>) -> Json<SupplyDto> {
    Json(
        app.market
            .call(|m| SupplyDto::of(m))
            .await
            .unwrap_or_default(),
    )
}

async fn catalog() -> Json<Vec<CatalogEntry>> {
    Json(
        GameEventKind::ALL
            .iter()
            .map(|&kind| CatalogEntry {
                kind,
                label: kind.label(),
                scope: kind.scope(),
                description: kind.description(),
                effects: kind.effects(1.0),
                world: kind.world_effects(1.0),
            })
            .collect(),
    )
}

/// Server-sent events: a `hello` with the current quotes, then every tick
/// and every accepted event. Each message is JSON with a `type` field and the
/// `seq` it was published under.
#[derive(Deserialize)]
struct StreamQuery {
    /// `EventSource` cannot set headers, so the stream takes the key in the
    /// query string. Without one the stream carries market data only.
    api_key: Option<String>,
    /// Resume after this sequence number: everything published since, as far
    /// back as the replay buffer still reaches, arrives before the live feed.
    /// `hello` reports `gap: true` when the buffer no longer goes that far.
    since: Option<u64>,
}

async fn stream(
    State(app): State<AppState>,
    Query(q): Query<StreamQuery>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let Subscription {
        rx,
        seq,
        oldest_seq,
        replay,
        gap,
    } = app.subscribe(q.since).await?;
    let hello = StreamMessage::Hello {
        sim_now_ms: app.clock.now().0,
        time_scale: app.clock.scale,
        quotes: app.quotes().await,
        oldest_seq,
        gap,
    };
    let viewer = q.api_key.as_deref().and_then(|k| app.user_of(k));
    // Ticks and events are public; a fill belongs to the trader that made it,
    // so it goes only to a stream that proved it speaks for that trader. The
    // replay buffer holds everybody's, so the same rule applies to it. The
    // published directory says who owns a trader without asking anyone,
    // which is what lets every open stream check every message.
    let owner = Arc::clone(&app);
    let visible = move |m: &StreamMessage| match m {
        StreamMessage::Fill { trader_id, .. }
        | StreamMessage::StopTriggered { trader_id, .. }
        | StreamMessage::OrderExpired { trader_id, .. } => {
            viewer.is_some_and(|user| owner.owner_of(TraderId(*trader_id)) == Some(user))
        }
        _ => true,
    };
    let mine = visible.clone();
    // A gap ends this connection: the client reconnects with `?since=` and
    // picks up where it left off. Never present later messages as an
    // uninterrupted stream.
    let live = BroadcastStream::new(rx)
        .take_while(Result::is_ok)
        .filter_map(Result::ok)
        .filter(move |m| mine(&m.message));
    let missed: Vec<Sequenced> = replay.into_iter().filter(|m| visible(&m.message)).collect();
    // The `hello` carries the sequence the connection joins at, so the first
    // live message a client sees is `seq + 1` — or, after a replay, the
    // number the replay left off at.
    let all = tokio_stream::once(Sequenced {
        seq,
        message: hello,
    })
    .chain(tokio_stream::iter(missed))
    .chain(live)
    .filter_map(|m| Event::default().json_data(&m).ok().map(Ok));
    Ok(Sse::new(all).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}
