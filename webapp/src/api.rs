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
use fehu::{Candle, Config, Interval, Order, Owner, TraderId};
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
use crate::events::{
    CatalogEntry, EventRecord, GameEventKind, GameEventRequest, MAX_MAGNITUDE, Prepared,
    PushEventRequest, Scope, SimEvent,
};
use crate::limit::Decision;
use crate::market::{
    Amendment, App, Closed, DelistError, Delisting, Market, OrderCheck, PayoutError, PlaceError,
    PlaceRequest, Placed, Quote, Sequenced, SnapshotDto, StreamMessage, Subscription, SupplyDto,
    Symbol, SymbolInfo, SymbolSpec, SymbolStatus, SymbolView, wall_now_ms,
};
use crate::trading::{
    AmendRequest, AmendResponse, BookDto, CreateTraderRequest, HolderDto, MAX_CLIENT_ORDER_ID,
    OpenOrderDto, OrderRecord, OrderRequest, OrderResponse, PortfolioDto, PositionDto, Refused,
    StopOrder, StopRequest, TradeDto, TraderSummary, UserHoldingsResponse,
};

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
        .route("/api/supply", get(supply))
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
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            retry_after_secs: None,
        }
    }

    /// The client is changing things faster than the server will take.
    fn rate_limited(retry_after: std::time::Duration) -> Self {
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

    fn not_found(symbol: &str) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "unknown_symbol",
            format!("no such symbol: {symbol}"),
        )
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", message)
    }

    /// The request is well formed but the market is not in a state to take
    /// it: a ticker already listed, a market with no room for another.
    fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, "conflict", message)
    }

    fn invalid_event(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_event", message)
    }

    fn bad_json(rej: JsonRejection) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_json", rej.body_text())
    }

    fn unknown_trader(id: u64) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "unknown_trader",
            format!("no such trader: {id}"),
        )
    }

    fn unknown_order(id: u64) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "unknown_order",
            format!("no resting order {id} (it may have filled or been cancelled)"),
        )
    }

    fn unknown_stop(id: u64) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "unknown_stop",
            format!("no stop {id} is being held for that trader (it may have fired)"),
        )
    }

    fn invalid_order(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_order", message)
    }

    /// The order would have traded with the trader's own resting order.
    fn self_trade(crossing: &[u64]) -> Self {
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
    fn would_cross(price_cents: i64, best_cents: i64) -> Self {
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
    fn duplicate_client_order_id(client_order_id: &str, order_id: u64) -> Self {
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
    fn place(symbol: &str, e: PlaceError) -> Self {
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
    fn refused(e: Refused) -> Self {
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
    fn unauthenticated() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthenticated",
            "this endpoint needs the API key the user was created with: send it as \
             `Authorization: Bearer <key>` or `X-Api-Key: <key>`",
        )
    }

    /// A key that is not one the server issued.
    fn invalid_api_key() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "invalid_api_key",
            "that API key is not one this server issued",
        )
    }

    /// A valid key, but for somebody else's property.
    fn forbidden(what: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, "forbidden", what)
    }

    /// The market will not take an order for this symbol right now.
    fn closed(symbol: &str, closed: Closed) -> Self {
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

    fn unknown_user(id: u64) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "unknown_user",
            format!("no such user: {id}"),
        )
    }

    fn unknown_account(id: u64) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "unknown_account",
            format!("no such account: {id}"),
        )
    }

    /// A refused money movement: the amount, the balance or the account's
    /// status made it impossible.
    fn money(e: MoneyError) -> Self {
        let (status, code) = match e {
            MoneyError::NotPositive { .. } | MoneyError::TooLarge { .. } => {
                (StatusCode::BAD_REQUEST, "invalid_amount")
            }
            MoneyError::NoWallet { .. } => (StatusCode::INTERNAL_SERVER_ERROR, "no_wallet"),
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

    /// A corporate payout the issuer could not fund. Nothing was paid.
    fn payout(e: PayoutError) -> Self {
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
fn owned_trader(app: &App, caller: Caller, trader: TraderId) -> Result<(), ApiError> {
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
fn owned_account(market: &Market, caller: Caller, account: AccountId) -> Result<(), ApiError> {
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
fn owned_user(caller: Caller, user: UserId) -> Result<(), ApiError> {
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
    /// Shares in existence.
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
        let (shares_outstanding, bid_shares, price_cents, market_cap_cents) = symbol
            .ask_listed(|s| {
                (
                    s.info.shares_outstanding,
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
    /// Shares in existence. The market never creates or destroys them, so
    /// this is the ceiling on what every trader can hold between them.
    shares_outstanding: u64,
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

/// A new listing, and the event it was recorded as.
#[derive(Serialize)]
struct ListingResponse {
    quote: Quote,
    event: EventRecord,
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
    payload: Result<Json<ListingRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ListingResponse>), ApiError> {
    let Json(req) = payload.map_err(ApiError::bad_json)?;
    let symbol =
        crate::symbols::register(&req.symbol).map_err(|e| ApiError::bad_request(e.to_string()))?;
    if req.shares_outstanding == 0 {
        return Err(ApiError::bad_request(
            "a listing needs shares: shares_outstanding must be positive",
        ));
    }
    let now = app.clock.now();
    // Cheap answer first: warming a year of history up only to find the
    // ticker taken would be a waste of the caller's time and this server's.
    if app.symbol(symbol).is_some() {
        return Err(ApiError::conflict(format!("{symbol} is already listed")));
    }
    let info = SymbolInfo {
        symbol,
        name: clean_text(req.name, 64).unwrap_or_else(|| symbol.to_string()),
        sector: clean_text(req.sector, 64).unwrap_or_else(|| "Uncategorised".into()),
        description: clean_text(req.description, 280).unwrap_or_default(),
        shares_outstanding: req.shares_outstanding,
        seed: req.seed.unwrap_or_else(|| seed_from_ticker(symbol)),
    };
    let spec = SymbolSpec {
        info,
        config: Config {
            start_price_cents: req.start_price_cents,
            drift: req.drift.unwrap_or(Config::default().drift),
            volatility: req.volatility.unwrap_or(Config::default().volatility),
            ..Config::default()
        },
        trading: fehu::TradingParams::default(),
    };
    let state = app
        .prepare_listing(spec, req.history_days.unwrap_or(0), now)
        .map_err(|e| ApiError::invalid_event(e.to_string()))?;
    let (source, note) = (req.source.unwrap_or_else(|| "api".into()), req.note);
    let (quote, record) = app
        .market
        .call(move |m| {
            let quote = m
                .list(state)
                .map_err(|e| ApiError::conflict(e.to_string()))?;
            let record = m.record(EventRecord {
                id: 0,
                received_at_ms: wall_now_ms(),
                at_ms: now.0,
                symbols: vec![symbol],
                kind: "corporate:listing".into(),
                source,
                note,
                magnitude: None,
                effects: Vec::new(),
                summary: vec![format!(
                    "{symbol} listed at {} cents, {} shares outstanding",
                    quote.price_cents, quote.shares_outstanding
                )],
            });
            Ok::<_, ApiError>((quote, record))
        })
        .await??;
    tracing::info!(
        symbol,
        price_cents = quote.price_cents,
        shares_outstanding = quote.shares_outstanding,
        "symbol listed"
    );
    Ok((
        StatusCode::CREATED,
        Json(ListingResponse {
            quote,
            event: record,
        }),
    ))
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

/// What a delisting undid, and the event it was recorded as.
#[derive(Serialize)]
struct DelistResponse {
    delisting: Delisting,
    event: EventRecord,
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
    payload: Result<Json<DelistRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<DelistResponse>), ApiError> {
    let Json(req) = payload.map_err(ApiError::bad_json)?;
    let now = app.clock.now();
    let (source, note, cents_per_share) = (
        req.source.unwrap_or_else(|| "api".into()),
        req.note,
        req.cents_per_share,
    );
    let ticker = symbol.clone();
    let (delisting, record) = app
        .market
        .call_async(move |m| {
            Box::pin(async move {
                let delisting = m
                    .delist(&ticker, cents_per_share, note.clone(), now.0)
                    .await
                    .map_err(|e| match e {
                        DelistError::Unknown => ApiError::not_found(&ticker),
                        DelistError::Price => ApiError::invalid_event(e.to_string()),
                        DelistError::Unfunded(_) => {
                            ApiError::new(StatusCode::CONFLICT, "payout_not_funded", e.to_string())
                        }
                    })?;
                let record = m.record(EventRecord {
                    id: 0,
                    received_at_ms: wall_now_ms(),
                    at_ms: now.0,
                    symbols: vec![delisting.symbol],
                    kind: "corporate:delisting".into(),
                    source,
                    note,
                    magnitude: None,
                    effects: Vec::new(),
                    summary: vec![format!(
                        "{} delisted at {} cents a share: {} shares bought out for {} cents \
                         across {} account(s), {} resting order(s) and {} stop(s) withdrawn",
                        delisting.symbol,
                        delisting.cents_per_share,
                        delisting.shares_bought_out,
                        delisting.total_cents,
                        delisting.accounts_paid,
                        delisting.orders_cancelled,
                        delisting.stops_cancelled,
                    )],
                });
                Ok::<_, ApiError>((delisting, record))
            })
        })
        .await??;
    tracing::info!(
        symbol = delisting.symbol,
        cents_per_share = delisting.cents_per_share,
        shares = delisting.shares_bought_out,
        total_cents = delisting.total_cents,
        "symbol delisted"
    );
    Ok((
        StatusCode::ACCEPTED,
        Json(DelistResponse {
            delisting,
            event: record,
        }),
    ))
}

/// A seed from a ticker, so a listing without one is still reproducible: the
/// same ticker on two servers gives the same company. FNV-1a, which is
/// nothing but a spread of the letters — it is a seed, not a digest.
fn seed_from_ticker(ticker: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in ticker.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
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
) -> Result<Json<SymbolStatus>, ApiError> {
    let handle = app
        .symbol(&symbol)
        .ok_or_else(|| ApiError::not_found(&symbol))?;
    let status = app
        .market
        .call_async(move |m| Box::pin(async move { m.halt(&handle).await }))
        .await?
        .ok_or_else(|| ApiError::not_found(&symbol))?;
    tracing::info!(symbol = status.symbol, "trading halted");
    Ok(Json(status))
}

/// Start trading again, whatever stopped it.
async fn resume_symbol(
    State(app): State<AppState>,
    Path(symbol): Path<String>,
    _admin: Admin,
) -> Result<Json<SymbolStatus>, ApiError> {
    let handle = app
        .symbol(&symbol)
        .ok_or_else(|| ApiError::not_found(&symbol))?;
    let status = app
        .market
        .call_async(move |m| Box::pin(async move { m.resume(&handle).await }))
        .await?
        .ok_or_else(|| ApiError::not_found(&symbol))?;
    tracing::info!(symbol = status.symbol, "trading resumed");
    Ok(Json(status))
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
    payload: Result<Json<PushEventRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<EventRecord>), ApiError> {
    let Json(req) = payload.map_err(ApiError::bad_json)?;
    let prepared = req
        .event
        .prepare()
        .map_err(|e| ApiError::invalid_event(e.to_string()))?;
    let now = app.clock.now();
    let at = req.timing.resolve(now).map_err(ApiError::bad_request)?;

    // A simulator event is the market's to apply: it is a change to a
    // symbol, and every change to a symbol is one job on the market.
    let handle = app
        .symbol(&symbol)
        .ok_or_else(|| ApiError::not_found(&symbol))?;
    let (source, note, event) = (
        req.source.unwrap_or_else(|| "api".into()),
        req.note,
        req.event,
    );
    let record = app
        .market
        .call_async(move |m| {
            Box::pin(async move {
                let ticker = handle.ticker;
                m.apply_events(&handle, vec![prepared], at)
                    .await?
                    .ok_or_else(|| ApiError::not_found(ticker))?
                    .map_err(ApiError::invalid_event)?;
                Ok::<_, ApiError>(m.record(EventRecord {
                    id: 0,
                    received_at_ms: wall_now_ms(),
                    at_ms: at.0,
                    symbols: vec![ticker],
                    kind: format!("sim:{}", sim_event_name(&event)),
                    source,
                    note,
                    magnitude: None,
                    effects: vec![event],
                    summary: vec![event.summary()],
                }))
            })
        })
        .await??;
    tracing::info!(id = record.id, symbol = %symbol, kind = %record.kind, "event accepted");
    Ok((StatusCode::ACCEPTED, Json(record)))
}

fn sim_event_name(e: &SimEvent) -> &'static str {
    match e {
        SimEvent::Jump { .. } => "jump",
        SimEvent::DriftShift { .. } => "drift_shift",
        SimEvent::DriftForTotalMove { .. } => "drift_for_total_move",
        SimEvent::VolShift { .. } => "vol_shift",
        SimEvent::FundamentalShift { .. } => "fundamental_shift",
        SimEvent::FundamentalTarget { .. } => "fundamental_target",
    }
}

async fn push_game_event(
    State(app): State<AppState>,
    _admin: Admin,
    payload: Result<Json<GameEventRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<EventRecord>), ApiError> {
    let Json(req) = payload.map_err(ApiError::bad_json)?;
    let magnitude = req.magnitude.unwrap_or(1.0);
    if !(magnitude.is_finite() && magnitude > 0.0 && magnitude <= MAX_MAGNITUDE) {
        return Err(ApiError::invalid_event(format!(
            "`magnitude` must be in (0, {MAX_MAGNITUDE}]"
        )));
    }
    let effects = req.kind.effects(magnitude);
    let prepared: Vec<Prepared> = effects
        .iter()
        .map(|e| {
            e.prepare()
                .map_err(|e| ApiError::invalid_event(e.to_string()))
        })
        .collect::<Result<_, _>>()?;
    let now = app.clock.now();
    let at = req.timing.resolve(now).map_err(ApiError::bad_request)?;

    // A company event touches one symbol; a market event touches every
    // symbol at the same simulated moment. Either way it is one job on the
    // market, so nothing trades in between.
    let target = match req.kind.scope() {
        Scope::Market => None,
        Scope::Company => {
            let sym = req.symbol.as_deref().ok_or_else(|| {
                ApiError::bad_request("`symbol` is required for a company-scoped event")
            })?;
            Some(app.symbol(sym).ok_or_else(|| ApiError::not_found(sym))?)
        }
    };
    let (kind, source, note) = (
        req.kind,
        req.source.unwrap_or_else(|| "game".into()),
        req.note,
    );
    let record = app
        .market
        .call_async(move |m| {
            Box::pin(async move {
                let symbols = match target {
                    Some(handle) => {
                        m.apply_events(&handle, prepared, at)
                            .await?
                            .ok_or_else(|| ApiError::not_found(handle.ticker))?
                            .map_err(ApiError::invalid_event)?;
                        vec![handle.ticker]
                    }
                    None => m
                        .apply_events_everywhere(prepared, at)
                        .await
                        .map_err(ApiError::invalid_event)?,
                };
                Ok::<_, ApiError>(m.record(EventRecord {
                    id: 0,
                    received_at_ms: wall_now_ms(),
                    at_ms: at.0,
                    symbols,
                    kind: format!("game:{}", game_kind_name(kind)),
                    source,
                    note,
                    magnitude: Some(magnitude),
                    summary: effects.iter().map(SimEvent::summary).collect(),
                    effects,
                }))
            })
        })
        .await??;
    tracing::info!(id = record.id, symbols = ?record.symbols, kind = %record.kind, magnitude, "game event accepted");
    Ok((StatusCode::ACCEPTED, Json(record)))
}

fn game_kind_name(kind: GameEventKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
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
    payload: Option<Json<CreateTraderRequest>>,
) -> Result<(StatusCode, Json<PortfolioDto>), ApiError> {
    let req = payload.map(|Json(r)| r).unwrap_or_default();
    let cash = req.cash_cents.unwrap_or(app.options.starting_cash_cents);
    if cash < 0 {
        return Err(ApiError::bad_request("`cash_cents` must not be negative"));
    }
    let email = check_email(req.email)?;
    let now = wall_now_ms();
    // Joining an existing user is that user's business alone.
    if let Some(user_id) = req.user_id {
        owned_user(
            caller.ok_or_else(ApiError::unauthenticated)?,
            UserId(user_id),
        )?;
    }
    let views = app.views().await;
    let (dto, user_id, account_id) = app
        .market
        .call(move |m| {
            let id = match (req.user_id, req.account_id) {
                (None, None) => m
                    .sign_up(req.name, email, cash, now)
                    .map_err(ApiError::money)?,
                (None, Some(_)) => {
                    return Err(ApiError::bad_request(
                        "`account_id` needs the `user_id` that owns it",
                    ));
                }
                (Some(user_id), account_id) => {
                    let user = UserId(user_id);
                    if !m.users.contains_key(&user) {
                        return Err(ApiError::unknown_user(user_id));
                    }
                    let account = match account_id {
                        Some(account_id) => {
                            let account = AccountId(account_id);
                            let held = m
                                .accounts
                                .get(&account)
                                .ok_or_else(|| ApiError::unknown_account(account_id))?;
                            if held.user_id != user {
                                return Err(ApiError::bad_request(format!(
                                    "account {account_id} belongs to user {}",
                                    held.user_id.0
                                )));
                            }
                            account
                        }
                        None => m
                            .open_account(user, req.name.clone(), cash, now)
                            .map_err(ApiError::money)?,
                    };
                    m.create_trader(user, account, req.name, now)
                }
            };
            let mut dto = portfolio(m, &views, id)?;
            if req.user_id.is_none() {
                // A new user was created for the trader: hand over their
                // key, once.
                dto.api_key = m.take_issued_key(UserId(dto.user_id));
            }
            let (user_id, account_id) = (dto.user_id, dto.account_id);
            Ok((dto, user_id, account_id))
        })
        .await??;
    tracing::info!(
        trader = dto.id,
        user = user_id,
        account = account_id,
        cash = dto.cash_cents,
        "trader created"
    );
    Ok((StatusCode::CREATED, Json(dto)))
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
fn mark_of(views: &[SymbolView], sym: &str) -> i64 {
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

fn portfolio(
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
    payload: Result<Json<OrderRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<OrderResponse>), ApiError> {
    let Json(req) = payload.map_err(ApiError::bad_json)?;
    let trader = TraderId(req.trader_id);
    owned_trader(&app, caller, trader)?;
    let handle = app
        .symbol(&symbol)
        .ok_or_else(|| ApiError::not_found(&symbol))?;
    let request = PlaceRequest {
        order: Order {
            owner: Owner::Trader(trader),
            side: req.side,
            kind: req.kind,
            tif: req.tif,
            qty: req.qty,
        },
        client_order_id: clean_client_order_id(req.client_order_id)?,
        post_only: req.post_only,
        day: req.day,
        expires_at_ms: req.expires_at_ms,
        display_qty: req.display_qty,
    };
    let placed = app
        .market
        .call_async(move |m| Box::pin(async move { m.place(&handle, trader, request).await }))
        .await?
        .map_err(|e| ApiError::place(&symbol, e))?;
    let response = match placed {
        // Same order, second delivery: nothing new happened.
        Placed::Replayed(response) => return Ok((StatusCode::OK, Json(response))),
        Placed::New(response) => response,
    };
    tracing::info!(
        trader = trader.0,
        symbol = response.symbol,
        order = response.order_id,
        side = ?response.side,
        qty = response.qty,
        filled = response.filled,
        status = ?response.status,
        "order"
    );
    Ok((StatusCode::CREATED, Json(response)))
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
    payload: Result<Json<DividendRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<DividendResponse>), ApiError> {
    let Json(req) = payload.map_err(ApiError::bad_json)?;
    let now = app.clock.now();
    let handle = app
        .symbol(&symbol)
        .ok_or_else(|| ApiError::not_found(&symbol))?;
    let (source, note, cents_per_share) = (
        req.source.unwrap_or_else(|| "api".into()),
        req.note,
        req.cents_per_share,
    );
    let (paid, record) = app
        .market
        .call_async(move |m| {
            Box::pin(async move {
                let price = handle
                    .ask_listed(|s| s.price_cents())
                    .await?
                    .ok_or_else(|| ApiError::not_found(handle.ticker))?;
                let (paid, effects) = m
                    .pay_dividend(&handle, cents_per_share, note.clone(), now.0)
                    .await
                    .map_err(ApiError::payout)?
                    .ok_or_else(|| {
                        ApiError::invalid_event(format!(
                            "a dividend must be between 1 and {} cents a share, one less than \
                             the price it is declared against",
                            price - 1
                        ))
                    })?;
                let record = m.record(EventRecord {
                    id: 0,
                    received_at_ms: wall_now_ms(),
                    at_ms: now.0,
                    symbols: vec![paid.symbol],
                    kind: "corporate:dividend".into(),
                    source,
                    note,
                    magnitude: Some(paid.cents_per_share as f64 / 100.0),
                    effects,
                    summary: vec![format!(
                        "dividend of {} cents a share on {} shares, {} cents to {} account(s)",
                        paid.cents_per_share,
                        paid.shares_paid,
                        paid.total_cents,
                        paid.accounts_paid
                    )],
                });
                Ok::<_, ApiError>((paid, record))
            })
        })
        .await??;
    tracing::info!(
        symbol = paid.symbol,
        cents_per_share = paid.cents_per_share,
        total_cents = paid.total_cents,
        accounts = paid.accounts_paid,
        "dividend paid"
    );
    Ok((
        StatusCode::ACCEPTED,
        Json(DividendResponse {
            dividend: paid,
            event: record,
        }),
    ))
}

/// What a dividend paid, and the event it was recorded as.
#[derive(Serialize)]
struct DividendResponse {
    dividend: crate::market::Dividend,
    event: EventRecord,
}

/// Arm a stop: a trigger the engine watches, not an order in the book.
///
/// Unlike an order this is accepted while the symbol is halted or its session
/// is closed — the trigger simply waits, and fires when trading resumes.
async fn submit_stop(
    State(app): State<AppState>,
    Path(symbol): Path<String>,
    caller: Caller,
    payload: Result<Json<StopRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<StopOrder>), ApiError> {
    let Json(req) = payload.map_err(ApiError::bad_json)?;
    let req = StopRequest {
        client_order_id: clean_client_order_id(req.client_order_id)?,
        ..req
    };
    owned_trader(&app, caller, TraderId(req.trader_id))?;
    let handle = app
        .symbol(&symbol)
        .ok_or_else(|| ApiError::not_found(&symbol))?;
    let stop = app
        .market
        .call_async(move |m| Box::pin(async move { m.place_stop(&handle, req).await }))
        .await?
        .map_err(|e| ApiError::place(&symbol, e))?;
    tracing::info!(
        trader = stop.trader_id,
        symbol = stop.symbol,
        stop = stop.stop_id,
        side = ?stop.side,
        qty = stop.qty,
        at = stop.stop_price_cents,
        "stop armed"
    );
    Ok((StatusCode::CREATED, Json(stop)))
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
) -> Result<Json<StopOrder>, ApiError> {
    let trader = TraderId(q.trader_id);
    owned_trader(&app, caller, trader)?;
    let handle = app
        .symbol(&symbol)
        .ok_or_else(|| ApiError::not_found(&symbol))?;
    let stop = app
        .market
        .call_async(move |m| Box::pin(async move { m.cancel_stop(&handle, trader, stop_id).await }))
        .await?
        .ok_or_else(|| ApiError::unknown_stop(stop_id))?;
    tracing::info!(
        trader = stop.trader_id,
        symbol = stop.symbol,
        stop = stop.stop_id,
        "stop cancelled"
    );
    Ok(Json(stop))
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
    payload: Result<Json<AmendRequest>, JsonRejection>,
) -> Result<Json<AmendResponse>, ApiError> {
    let Json(req) = payload.map_err(ApiError::bad_json)?;
    let trader = TraderId(req.trader_id);
    owned_trader(&app, caller, trader)?;
    let client_order_id = clean_client_order_id(req.client_order_id)?;
    let handle = app
        .symbol(&symbol)
        .ok_or_else(|| ApiError::not_found(&symbol))?;
    let amendment = Amendment {
        order_id,
        price_cents: req.price_cents,
        qty: req.qty,
        post_only: req.post_only,
        client_order_id,
    };
    let amended = app
        .market
        .call_async(move |m| Box::pin(async move { m.amend(&handle, trader, amendment).await }))
        .await?
        .map_err(|e| ApiError::place(&symbol, e))?;
    tracing::info!(
        trader = trader.0,
        symbol = amended.order.symbol,
        replaced = order_id,
        order = amended.order.order_id,
        qty = amended.order.qty,
        "order amended"
    );
    Ok(Json(AmendResponse {
        replaced_order_id: amended.replaced_order_id,
        replaced_filled: amended.replaced_filled,
        order: amended.order,
    }))
}

async fn cancel_order(
    State(app): State<AppState>,
    Path((symbol, order_id)): Path<(String, u64)>,
    Query(q): Query<TraderQuery>,
    caller: Caller,
) -> Result<Json<OpenOrderDto>, ApiError> {
    let trader = TraderId(q.trader_id);
    owned_trader(&app, caller, trader)?;
    let handle = app
        .symbol(&symbol)
        .ok_or_else(|| ApiError::not_found(&symbol))?;
    let sym = handle.ticker;
    let cancelled = app
        .market
        .call_async(move |m| Box::pin(async move { m.cancel(&handle, trader, order_id).await }))
        .await?
        .map_err(|e| match e {
            fehu::CancelError::Unknown => ApiError::unknown_order(order_id),
            fehu::CancelError::NotOwner => {
                ApiError::new(StatusCode::FORBIDDEN, "not_owner", e.to_string())
            }
            _ => ApiError::bad_request(e.to_string()),
        })?
        .ok_or_else(|| ApiError::not_found(&symbol))?;
    tracing::info!(
        trader = trader.0,
        symbol = sym,
        order = order_id,
        "order cancelled"
    );
    Ok(Json(OpenOrderDto::from_resting(sym, &cancelled)))
}

async fn cancel_all(
    State(app): State<AppState>,
    Path(trader_id): Path<u64>,
    caller: Caller,
) -> Result<Json<Vec<OpenOrderDto>>, ApiError> {
    let trader = TraderId(trader_id);
    owned_trader(&app, caller, trader)?;
    Ok(Json(
        app.market
            .call_async(move |m| Box::pin(m.cancel_all(trader)))
            .await?,
    ))
}

// ---------------------------------------------------------------------------
// Users and accounts
//
// A user is the person, an account holds their money, and a trader trades on
// exactly one account. Every amount here is an integer number of cents.

async fn create_user(
    State(app): State<AppState>,
    payload: Option<Json<CreateUserRequest>>,
) -> Result<(StatusCode, Json<UserDto>), ApiError> {
    let req = payload.map(|Json(r)| r).unwrap_or_default();
    let email = check_email(req.email)?;
    let views = app.views().await;
    let dto = app
        .market
        .call(move |m| {
            let id = m.create_user(req.name, email, wall_now_ms());
            let mut dto = user_dto(m, &views, id)?;
            // The one and only time the key is handed out.
            dto.api_key = m.take_issued_key(id);
            Ok::<_, ApiError>(dto)
        })
        .await??;
    tracing::info!(user = dto.id, "user created");
    Ok((StatusCode::CREATED, Json(dto)))
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
    payload: Option<Json<OpenAccountRequest>>,
) -> Result<(StatusCode, Json<AccountDto>), ApiError> {
    let req = payload.map(|Json(r)| r).unwrap_or_default();
    let cash = req.cash_cents.unwrap_or(app.options.starting_cash_cents);
    let user = UserId(user_id);
    owned_user(caller, user)?;
    let dto = app
        .market
        .call(move |m| {
            if !m.users.contains_key(&user) {
                return Err(ApiError::unknown_user(user_id));
            }
            let id = m
                .open_account(user, req.name, cash, wall_now_ms())
                .map_err(ApiError::money)?;
            account_dto(m, id)
        })
        .await??;
    tracing::info!(user = user_id, account = dto.id, cash, "account opened");
    Ok((StatusCode::CREATED, Json(dto)))
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
    payload: Result<Json<TransferRequest>, JsonRejection>,
) -> Result<Json<LedgerResponse>, ApiError> {
    let Json(req) = payload.map_err(ApiError::bad_json)?;
    let id = AccountId(account_id);
    let amount_cents = req.amount_cents;
    let response = app
        .market
        .call(move |m| {
            if !m.accounts.contains_key(&id) {
                return Err(ApiError::unknown_account(account_id));
            }
            let entry = m
                .mint_into(id, req.amount_cents, req.memo, wall_now_ms())
                .map_err(ApiError::money)?;
            Ok::<_, ApiError>(LedgerResponse {
                account: account_dto(m, id)?,
                entries: vec![entry],
            })
        })
        .await??;
    tracing::info!(
        account = account_id,
        amount_cents,
        balance_cents = response.entries[0].balance_cents,
        "deposit"
    );
    Ok(Json(response))
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
    payload: Result<Json<TransferRequest>, JsonRejection>,
) -> Result<Json<LedgerResponse>, ApiError> {
    let Json(req) = payload.map_err(ApiError::bad_json)?;
    let id = AccountId(account_id);
    let amount_cents = req.amount_cents;
    let response = app
        .market
        .call(move |m| {
            if !m.accounts.contains_key(&id) {
                return Err(ApiError::unknown_account(account_id));
            }
            let entry = m
                .burn_from(id, req.amount_cents, req.memo, wall_now_ms())
                .map_err(ApiError::money)?;
            Ok::<_, ApiError>(LedgerResponse {
                account: account_dto(m, id)?,
                entries: vec![entry],
            })
        })
        .await??;
    tracing::info!(
        account = account_id,
        amount_cents,
        balance_cents = response.entries[0].balance_cents,
        "withdrawal"
    );
    Ok(Json(response))
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
    payload: Option<Json<StatusRequest>>,
) -> Result<Json<AccountDto>, ApiError> {
    let Json(req) = payload.ok_or_else(|| ApiError::bad_request("a `status` is required"))?;
    let id = AccountId(account_id);
    let status = req.status;
    // Who has to be who depends on which transition this is. An operator
    // freezes an account that is not theirs — that is the whole point of a
    // freeze — so they are not asked to own it; an owner closes theirs, and
    // is not asked to be an operator.
    let owner = match status {
        AccountStatus::Frozen | AccountStatus::Active => {
            if admin.is_none() {
                return Err(ApiError::forbidden(
                    "freezing and unfreezing an account is the operator's to do",
                ));
            }
            None
        }
        AccountStatus::Closed => Some(caller.ok_or_else(ApiError::unauthenticated)?),
    };
    let dto = app
        .market
        .call_async(move |m| {
            Box::pin(async move {
                if let Some(owner) = owner {
                    owned_account(m, owner, id)?;
                } else if !m.accounts.contains_key(&id) {
                    return Err(ApiError::unknown_account(account_id));
                }
                match status {
                    AccountStatus::Frozen => m.freeze_account(id).await,
                    AccountStatus::Active => m.unfreeze_account(id),
                    AccountStatus::Closed => m.close_account(id),
                }
                .map_err(ApiError::money)?;
                account_dto(m, id)
            })
        })
        .await??;
    tracing::info!(account = account_id, status = ?status, "account status");
    Ok(Json(dto))
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
    payload: Result<Json<TransferRequest>, JsonRejection>,
) -> Result<Json<PortfolioDto>, ApiError> {
    let Json(req) = payload.map_err(ApiError::bad_json)?;
    let trader = TraderId(trader_id);
    let views = app.views().await;
    let amount_cents = req.amount_cents;
    let (dto, account_id, balance_cents) = app
        .market
        .call(move |m| {
            let account_id = m
                .traders
                .get(&trader)
                .ok_or_else(|| ApiError::unknown_trader(trader_id))?
                .account_id;
            let entry = m
                .mint_into(account_id, req.amount_cents, req.memo, wall_now_ms())
                .map_err(ApiError::money)?;
            Ok::<_, ApiError>((
                portfolio(m, &views, trader)?,
                account_id,
                entry.balance_cents,
            ))
        })
        .await??;
    tracing::info!(
        trader = trader_id,
        account = account_id.0,
        amount_cents,
        balance_cents,
        "deposit"
    );
    Ok(Json(dto))
}

fn user_dto(market: &Market, views: &[SymbolView], id: UserId) -> Result<UserDto, ApiError> {
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

fn account_dto(market: &Market, id: AccountId) -> Result<AccountDto, ApiError> {
    let account = market
        .accounts
        .get(&id)
        .ok_or_else(|| ApiError::unknown_account(id.0))?;
    Ok(account_view(market, account))
}

fn account_view(market: &Market, account: &Account) -> AccountDto {
    AccountDto::new(
        account,
        &market.ledger,
        market.trader_on(account.id).map(|t| t.id.0),
    )
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
