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
use fehu::{Candle, Config, Interval, Order, OrderKind, Owner, TraderId};
use serde::{Deserialize, Serialize};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{Stream, StreamExt};
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

use crate::account::{
    Account, AccountCheck, AccountDto, AccountId, CreateUserRequest, LedgerResponse, MoneyError,
    OpenAccountRequest, StatusRequest, TransferRequest, UserDto, UserId,
};
use crate::events::{
    CatalogEntry, EventRecord, GameEventKind, GameEventRequest, MAX_MAGNITUDE, Prepared,
    PushEventRequest, Scope, SimEvent,
};
use crate::limit::Decision;
use crate::market::{
    App, Closed, Market, PlaceError, Quote, Sequenced, SnapshotDto, StreamMessage, Subscription,
    SymbolInfo, SymbolState, SymbolStatus, wall_now_ms,
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
        .route("/api/symbols", get(list_symbols))
        .route("/api/symbols/{symbol}", get(get_symbol))
        .route("/api/symbols/{symbol}/bars", get(get_bars))
        .route(
            "/api/symbols/{symbol}/events",
            get(list_symbol_events).post(push_sim_event),
        )
        .route("/api/symbols/{symbol}/shares", get(get_shares))
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
        .route("/api/game/catalog", get(catalog))
        .route("/api/game/events", get(list_events).post(push_game_event))
        .route("/api/events", get(list_events))
        .route("/api/stream", get(stream))
        .layer(middleware::from_fn_with_state(
            Arc::clone(&app),
            rate_limit_writes,
        ))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(app)
}

/// Refuse a client that is changing the market faster than
/// `FEHU_RATE_PER_SEC` allows.
///
/// Only unsafe methods are counted. Reads are cheap, idempotent and mostly
/// public; what is worth protecting is the one market lock every order goes
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
    let who = api_key_of_headers(request.headers()).and_then(|key| app.market().keys.user_of(&key));
    match app.allow(who) {
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

    /// A submission the market would not place.
    fn place(e: PlaceError) -> Self {
        match e {
            PlaceError::Refused(e) => Self::refused(e),
            PlaceError::UnknownTrader(id) => Self::unknown_trader(id),
            PlaceError::Invalid(message) => Self::invalid_order(message),
        }
    }

    /// An order the trader's account would not fund.
    fn refused(e: Refused) -> Self {
        match e {
            Refused::Account(MoneyError::Status { .. }) => {
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
            MoneyError::BalanceCap { .. } => (StatusCode::UNPROCESSABLE_ENTITY, "balance_cap"),
            MoneyError::Insufficient { .. } => {
                (StatusCode::UNPROCESSABLE_ENTITY, "insufficient_funds")
            }
            MoneyError::Status { .. } => (StatusCode::CONFLICT, "account_not_active"),
            MoneyError::Reserved { .. } => (StatusCode::CONFLICT, "cash_reserved"),
        };
        Self::new(status, code, e.to_string())
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
        app.market()
            .keys
            .user_of(&key)
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
        Ok(app.market().keys.user_of(&key).map(Caller))
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

/// Compare two secrets without giving their common prefix away in the time
/// taken.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The caller owns `trader`.
fn owned_trader(market: &Market, caller: Caller, trader: TraderId) -> Result<(), ApiError> {
    let held = market
        .traders
        .get(&trader)
        .ok_or_else(|| ApiError::unknown_trader(trader.0))?;
    if held.user_id == caller.0 {
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
}

async fn reconcile(
    State(app): State<AppState>,
    _admin: Admin,
) -> Json<crate::reconcile::Reconciliation> {
    Json(app.market().reconcile())
}

async fn health(State(app): State<AppState>) -> Json<Health> {
    let market = app.market();
    Json(Health {
        status: "ok",
        uptime_secs: app.started_at.elapsed().map_or(0, |d| d.as_secs()),
        sim_now_ms: app.clock.now().0,
        time_scale: app.clock.scale,
        symbols: market.symbols.len(),
        events_logged: market.events.len(),
        ticks_total: market.symbols.iter().map(|s| s.ticks_total).sum(),
        trades_total: market.symbols.iter().map(|s| s.trades_total).sum(),
        users: market.users.len(),
        accounts: market.accounts.len(),
        traders: market.traders.len(),
        cash_cents: market
            .accounts
            .values()
            .map(Account::balance_cents)
            .fold(0i64, i64::saturating_add),
        resting_orders: market
            .symbols
            .iter()
            .map(|s| {
                s.exchange
                    .book()
                    .orders()
                    .filter(|o| o.owner != Owner::Synthetic)
                    .count()
            })
            .sum(),
    })
}

#[derive(Serialize)]
struct SymbolsResponse {
    sim_now_ms: i64,
    symbols: Vec<Quote>,
}

async fn list_symbols(State(app): State<AppState>) -> Json<SymbolsResponse> {
    let market = app.market();
    Json(SymbolsResponse {
        sim_now_ms: app.clock.now().0,
        symbols: market.symbols.iter().map(SymbolState::quote).collect(),
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
    fn new(market: &Market, s: &SymbolState, caller: Option<Caller>) -> Self {
        let sym = s.info.symbol;
        let mut holders: Vec<HolderDto> = market
            .traders
            .values()
            .filter(|t| caller.is_some_and(|c| c.0 == t.user_id))
            .filter_map(|t| HolderDto::new(t, sym))
            .collect();
        holders.sort_by_key(|h| (std::cmp::Reverse(h.qty), h.trader_id));
        Self {
            symbol: sym,
            shares_outstanding: s.info.shares_outstanding,
            held_shares: market.held_shares(sym),
            bid_shares: market.bid_shares(sym),
            available_shares: market.available_shares(sym),
            price_cents: s.price_cents(),
            market_cap_cents: s.market_cap_cents(),
            holders,
        }
    }
}

async fn get_symbol(
    State(app): State<AppState>,
    Path(symbol): Path<String>,
    caller: Option<Caller>,
) -> Result<Json<SymbolDetail>, ApiError> {
    let market = app.market();
    let idx = market
        .symbol_index(&symbol)
        .ok_or_else(|| ApiError::not_found(&symbol))?;
    let s = &market.symbols[idx];
    Ok(Json(SymbolDetail {
        info: s.info,
        quote: s.quote(),
        snapshot: s.sim().snapshot().into(),
        config: s.sim().config().clone(),
        ticks_total: s.ticks_total,
        shares: SharesDto::new(&market, s, caller),
        status: market
            .status(idx, app.clock.now())
            .ok_or_else(|| ApiError::not_found(&symbol))?,
    }))
}

/// Whether a symbol can be traded right now, and if not, why not.
async fn get_status(
    State(app): State<AppState>,
    Path(symbol): Path<String>,
) -> Result<Json<SymbolStatus>, ApiError> {
    let market = app.market();
    let idx = market
        .symbol_index(&symbol)
        .ok_or_else(|| ApiError::not_found(&symbol))?;
    market
        .status(idx, app.clock.now())
        .map(Json)
        .ok_or_else(|| ApiError::not_found(&symbol))
}

/// Stop trading in a symbol. It stays stopped until it is resumed.
async fn halt_symbol(
    State(app): State<AppState>,
    Path(symbol): Path<String>,
    _admin: Admin,
) -> Result<Json<SymbolStatus>, ApiError> {
    let now = app.clock.now();
    let mut market = app.market();
    let idx = market
        .symbol_index(&symbol)
        .ok_or_else(|| ApiError::not_found(&symbol))?;
    let status = market
        .halt(idx, now)
        .ok_or_else(|| ApiError::not_found(&symbol))?;
    tracing::info!(symbol = status.symbol, "trading halted");
    app.publish(StreamMessage::Status(status));
    Ok(Json(status))
}

/// Start trading again, whatever stopped it.
async fn resume_symbol(
    State(app): State<AppState>,
    Path(symbol): Path<String>,
    _admin: Admin,
) -> Result<Json<SymbolStatus>, ApiError> {
    let now = app.clock.now();
    let mut market = app.market();
    let idx = market
        .symbol_index(&symbol)
        .ok_or_else(|| ApiError::not_found(&symbol))?;
    let (status, fills) = market
        .resume(idx, now)
        .ok_or_else(|| ApiError::not_found(&symbol))?;
    tracing::info!(symbol = status.symbol, "trading resumed");
    app.publish(StreamMessage::Status(status));
    for fill in fills {
        app.publish(StreamMessage::Fill {
            trader_id: fill.trader_id,
            fill,
        });
    }
    Ok(Json(status))
}

async fn get_shares(
    State(app): State<AppState>,
    Path(symbol): Path<String>,
    caller: Option<Caller>,
) -> Result<Json<SharesDto>, ApiError> {
    let market = app.market();
    let s = market
        .symbol(&symbol)
        .ok_or_else(|| ApiError::not_found(&symbol))?;
    Ok(Json(SharesDto::new(&market, s, caller)))
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
    let market = app.market();
    let s = market
        .symbol(&symbol)
        .ok_or_else(|| ApiError::not_found(&symbol))?;
    Ok(Json(BarsResponse {
        symbol: s.info.symbol,
        interval,
        interval_ms: interval.millis(),
        sim_now_ms: app.clock.now().0,
        bars: s.bars(interval, limit),
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
) -> Json<EventsResponse> {
    let limit = q.limit.unwrap_or(100).clamp(1, app.options.event_log);
    let market = app.market();
    let events = market
        .events
        .iter()
        .rev()
        .filter(|e| match &q.symbol {
            Some(sym) => e.symbols.iter().any(|s| s.eq_ignore_ascii_case(sym)),
            None => true,
        })
        .take(limit)
        .cloned()
        .collect();
    Json(EventsResponse {
        sim_now_ms: app.clock.now().0,
        events,
    })
}

async fn list_symbol_events(
    State(app): State<AppState>,
    Path(symbol): Path<String>,
    Query(q): Query<EventsQuery>,
) -> Result<Json<EventsResponse>, ApiError> {
    if app.market().symbol(&symbol).is_none() {
        return Err(ApiError::not_found(&symbol));
    }
    Ok(list_events(
        State(app),
        Query(EventsQuery {
            limit: q.limit,
            symbol: Some(symbol),
        }),
    )
    .await)
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

    let record = {
        let mut market = app.market();
        let s = market
            .symbol_mut(&symbol)
            .ok_or_else(|| ApiError::not_found(&symbol))?;
        prepared
            .apply(s.exchange.simulator_mut(), at)
            .map_err(|e| ApiError::invalid_event(e.to_string()))?;
        let ticker = s.info.symbol;
        market.record(EventRecord {
            id: 0,
            received_at_ms: wall_now_ms(),
            at_ms: at.0,
            symbols: vec![ticker],
            kind: format!("sim:{}", sim_event_name(&req.event)),
            source: req.source.unwrap_or_else(|| "api".into()),
            note: req.note,
            magnitude: None,
            effects: vec![req.event],
            summary: vec![req.event.summary()],
        })
    };
    tracing::info!(id = record.id, symbol = %symbol, kind = %record.kind, "event accepted");
    app.publish(StreamMessage::Event(record.clone()));
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

    let record = {
        let mut market = app.market();
        let targets: Vec<usize> = match req.kind.scope() {
            Scope::Market => (0..market.symbols.len()).collect(),
            Scope::Company => {
                let sym = req.symbol.as_deref().ok_or_else(|| {
                    ApiError::bad_request("`symbol` is required for a company-scoped event")
                })?;
                let idx = market
                    .symbols
                    .iter()
                    .position(|s| s.info.symbol.eq_ignore_ascii_case(sym))
                    .ok_or_else(|| ApiError::not_found(sym))?;
                vec![idx]
            }
        };
        for &i in &targets {
            let sim = market.symbols[i].exchange.simulator_mut();
            for p in &prepared {
                p.apply(sim, at)
                    .map_err(|e| ApiError::invalid_event(e.to_string()))?;
            }
        }
        let symbols = targets
            .iter()
            .map(|&i| market.symbols[i].info.symbol)
            .collect();
        market.record(EventRecord {
            id: 0,
            received_at_ms: wall_now_ms(),
            at_ms: at.0,
            symbols,
            kind: format!("game:{}", game_kind_name(req.kind)),
            source: req.source.unwrap_or_else(|| "game".into()),
            note: req.note,
            magnitude: Some(magnitude),
            summary: effects.iter().map(SimEvent::summary).collect(),
            effects,
        })
    };
    tracing::info!(id = record.id, symbols = ?record.symbols, kind = %record.kind, magnitude, "game event accepted");
    app.publish(StreamMessage::Event(record.clone()));
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
    let market = app.market();
    let s = market
        .symbol(&symbol)
        .ok_or_else(|| ApiError::not_found(&symbol))?;
    let b = s.exchange.book();
    Ok(Json(BookResponse {
        symbol: s.info.symbol,
        ts_ms: s.exchange.clock().0,
        reference_cents: s.exchange.reference_cents(),
        bid_cents: b.best_bid(),
        ask_cents: b.best_ask(),
        pending_flow: s.exchange.pending_flow(),
        book: s.book(depth),
    }))
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
    let market = app.market();
    let s = market
        .symbol(&symbol)
        .ok_or_else(|| ApiError::not_found(&symbol))?;
    Ok(Json(TradesResponse {
        symbol: s.info.symbol,
        sim_now_ms: app.clock.now().0,
        trades: s
            .tape
            .iter()
            .rev()
            .take(limit)
            .map(TradeDto::from)
            .collect(),
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
    let mut market = app.market();
    let id = match (req.user_id, req.account_id) {
        (None, None) => market
            .sign_up(req.name, email, cash, now)
            .map_err(ApiError::money)?,
        (None, Some(_)) => {
            return Err(ApiError::bad_request(
                "`account_id` needs the `user_id` that owns it",
            ));
        }
        (Some(user_id), account_id) => {
            // Joining an existing user is that user's business alone.
            let user = UserId(user_id);
            owned_user(caller.ok_or_else(ApiError::unauthenticated)?, user)?;
            if !market.users.contains_key(&user) {
                return Err(ApiError::unknown_user(user_id));
            }
            let account = match account_id {
                Some(account_id) => {
                    let account = AccountId(account_id);
                    let held = market
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
                None => market
                    .open_account(user, req.name.clone(), cash, now)
                    .map_err(ApiError::money)?,
            };
            market.create_trader(user, account, req.name, now)
        }
    };
    let mut dto = portfolio(&market, id)?;
    if req.user_id.is_none() {
        // A new user was created for the trader: hand over their key, once.
        dto.api_key = market.keys.take_issued_key(UserId(dto.user_id));
    }
    tracing::info!(
        trader = dto.id,
        user = dto.user_id,
        account = dto.account_id,
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
async fn list_traders(State(app): State<AppState>, caller: Caller) -> Json<Vec<TraderSummary>> {
    let market = app.market();
    Json(
        market
            .traders
            .values()
            .filter(|t| t.user_id == caller.0)
            .map(|t| {
                let account = market.accounts.get(&t.account_id);
                let cash = account.map_or(0, Account::balance_cents);
                let equity = cash.saturating_add(
                    t.positions
                        .iter()
                        .map(|(sym, p)| {
                            let mark = market
                                .symbol(sym)
                                .map_or(0, |s| s.exchange.reference_cents());
                            p.market_value_cents(mark)
                        })
                        .fold(0i64, i64::saturating_add),
                );
                TraderSummary {
                    id: t.id.0,
                    user_id: t.user_id.0,
                    account_id: t.account_id.0,
                    name: t.name.clone(),
                    account_status: account.map(|a| a.status).unwrap_or_default(),
                    cash_cents: cash,
                    equity_cents: equity,
                    positions: t.positions.len(),
                    open_orders: market
                        .symbols
                        .iter()
                        .map(|s| s.exchange.book().orders_of(t.owner()).count())
                        .sum(),
                }
            })
            .collect(),
    )
}

async fn get_trader(
    State(app): State<AppState>,
    Path(trader_id): Path<u64>,
    caller: Caller,
) -> Result<Json<PortfolioDto>, ApiError> {
    let market = app.market();
    let trader = TraderId(trader_id);
    owned_trader(&market, caller, trader)?;
    Ok(Json(portfolio(&market, trader)?))
}

fn portfolio(market: &Market, id: TraderId) -> Result<PortfolioDto, ApiError> {
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
        let mark = market
            .symbol(sym)
            .map_or(0, |s| s.exchange.reference_cents());
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
    let open_orders = market
        .symbols
        .iter()
        .flat_map(|s| {
            s.exchange
                .book()
                .orders_of(t.owner())
                .map(|o| OpenOrderDto::from_resting(s.info.symbol, o))
        })
        .collect();
    Ok(PortfolioDto {
        id: t.id.0,
        user_id: t.user_id.0,
        account_id: t.account_id.0,
        name: t.name.clone(),
        created_at_ms: t.created_at_ms,
        account_status: account.status,
        cash_cents: account.balance_cents(),
        reserved_cents: account.reserved_cents(),
        free_cash_cents: account.available_cents(),
        equity_cents: account.balance_cents().saturating_add(value),
        realised_pnl_cents: realised,
        unrealised_pnl_cents: unrealised,
        positions,
        open_orders,
        stops: market.stops_of(t.id).cloned().collect(),
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
    let order = Order {
        owner: Owner::Trader(trader),
        side: req.side,
        kind: req.kind,
        tif: req.tif,
        qty: req.qty,
    };
    fehu::OrderBook::validate(&order).map_err(|e| ApiError::invalid_order(e.to_string()))?;
    let client_order_id = clean_client_order_id(req.client_order_id)?;

    let (response, fills, replayed) = {
        let mut market = app.market();
        let idx = market
            .symbol_index(&symbol)
            .ok_or_else(|| ApiError::not_found(&symbol))?;
        owned_trader(&market, caller, trader)?;
        let sym = market.symbols[idx].info.symbol;
        // A closed session or a halt takes no new orders at all.
        if let Some(closed) = market.symbols[idx].closed(app.clock.now()) {
            return Err(ApiError::closed(sym, closed));
        }
        if req.post_only {
            check_post_only(&market.symbols[idx], &order)?;
        }
        let crossing = self_crossing(&market.symbols[idx], &order);
        if !crossing.is_empty() {
            return Err(ApiError::self_trade(&crossing));
        }
        // The same order sent twice — a retry after a timeout, say — is
        // placed once: the first response is replayed, and a re-used id that
        // asks for something else is refused rather than quietly obeyed.
        if let Some(id) = client_order_id.as_deref()
            && let Some(record) = market.order_by_client_id(trader, id)
        {
            if !record.matches(&order, sym) {
                return Err(ApiError::duplicate_client_order_id(id, record.order_id));
            }
            let Some(accepted) = record.accepted.clone() else {
                return Err(ApiError::duplicate_client_order_id(id, record.order_id));
            };
            (accepted, Vec::new(), true)
        } else {
            let (response, fills) = market
                .place(idx, trader, order, client_order_id)
                .map_err(ApiError::place)?;
            (response, fills, false)
        }
    };
    if replayed {
        // Same order, second delivery: nothing new happened.
        return Ok((StatusCode::OK, Json(response)));
    }
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
    for fill in fills {
        app.publish(StreamMessage::Fill {
            trader_id: fill.trader_id,
            fill,
        });
    }
    Ok((StatusCode::CREATED, Json(response)))
}

#[derive(Deserialize)]
struct TraderQuery {
    trader_id: u64,
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
    let mut market = app.market();
    let idx = market
        .symbol_index(&symbol)
        .ok_or_else(|| ApiError::not_found(&symbol))?;
    owned_trader(&market, caller, TraderId(req.trader_id))?;
    let now = market.symbols[idx].exchange.clock().0;
    let stop = market.place_stop(idx, &req, now).map_err(ApiError::place)?;
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
    let market = app.market();
    owned_trader(&market, caller, TraderId(q.trader_id))?;
    let s = market
        .symbol(&symbol)
        .ok_or_else(|| ApiError::not_found(&symbol))?;
    Ok(Json(
        s.stops
            .iter()
            .filter(|stop| stop.trader_id == q.trader_id)
            .cloned()
            .collect(),
    ))
}

/// Every stop a trader holds, across all symbols.
async fn list_trader_stops(
    State(app): State<AppState>,
    Path(trader_id): Path<u64>,
    caller: Caller,
) -> Result<Json<Vec<StopOrder>>, ApiError> {
    let market = app.market();
    let trader = TraderId(trader_id);
    owned_trader(&market, caller, trader)?;
    Ok(Json(market.stops_of(trader).cloned().collect()))
}

/// Withdraw a stop before it fires.
async fn cancel_stop(
    State(app): State<AppState>,
    Path((symbol, stop_id)): Path<(String, u64)>,
    Query(q): Query<TraderQuery>,
    caller: Caller,
) -> Result<Json<StopOrder>, ApiError> {
    let mut market = app.market();
    market
        .symbol_index(&symbol)
        .ok_or_else(|| ApiError::not_found(&symbol))?;
    let trader = TraderId(q.trader_id);
    owned_trader(&market, caller, trader)?;
    let stop = market
        .cancel_stop(stop_id, trader)
        .ok_or_else(|| ApiError::unknown_stop(stop_id))?;
    tracing::info!(
        trader = stop.trader_id,
        symbol = stop.symbol,
        stop = stop.stop_id,
        "stop cancelled"
    );
    Ok(Json(stop))
}

/// The price an order can reach: its limit, or for a market order the collar
/// the exchange turns it into.
fn reachable_price_cents(s: &SymbolState, order: &Order) -> i64 {
    match order.kind {
        OrderKind::Limit { price_cents } => price_cents,
        OrderKind::Market => {
            let collar = s.exchange.params().liquidity.market_collar;
            let reference = s.exchange.reference_cents() as f64;
            let price = match order.side {
                fehu::Side::Buy => (reference * (1.0 + collar)).ceil(),
                fehu::Side::Sell => (reference * (1.0 - collar)).floor(),
            };
            (price as i64).max(1)
        }
    }
}

/// The trader's own resting orders this one would trade with. Trading with
/// yourself moves no shares and no money but does print on the tape and move
/// the price, so it is refused rather than matched.
///
/// Only orders the incoming one would actually reach count: the book's own
/// preview says how far down the other side it would walk, and anything
/// past that is none of its business.
fn self_crossing(s: &SymbolState, order: &Order) -> Vec<u64> {
    let book = s.exchange.book();
    let limit = reachable_price_cents(s, order);
    let Some(worst) = book
        .preview(order.side, order.qty, Some(limit))
        .worst_price_cents
    else {
        return Vec::new(); // Nothing would trade at all.
    };
    book.orders_of(order.owner)
        .filter(|resting| resting.side != order.side)
        .filter(|resting| match order.side {
            fehu::Side::Buy => resting.price_cents <= worst,
            fehu::Side::Sell => resting.price_cents >= worst,
        })
        .map(|resting| resting.id.0)
        .collect()
}

/// A post-only order must rest. It cannot if it is a market order, if it is
/// not good-till-cancelled, or if its price is already tradable.
fn check_post_only(s: &SymbolState, order: &Order) -> Result<(), ApiError> {
    let OrderKind::Limit { price_cents } = order.kind else {
        return Err(ApiError::invalid_order(
            "a market order cannot be post-only: it exists to take liquidity",
        ));
    };
    if order.tif != fehu::TimeInForce::Gtc {
        return Err(ApiError::invalid_order(
            "a post-only order must be `gtc`: the others are there to trade at once",
        ));
    }
    let book = s.exchange.book();
    let best = match order.side {
        fehu::Side::Buy => book.best_ask().filter(|ask| *ask <= price_cents),
        fehu::Side::Sell => book.best_bid().filter(|bid| *bid >= price_cents),
    };
    match best {
        Some(best) => Err(ApiError::would_cross(price_cents, best)),
        None => Ok(()),
    }
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
    let market = app.market();
    let record = market
        .order(order_id)
        .ok_or_else(|| ApiError::unknown_order(order_id))?;
    owned_trader(&market, caller, TraderId(record.trader_id))?;
    Ok(Json(record.clone()))
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
    let market = app.market();
    owned_trader(&market, caller, trader)?;
    Ok(Json(
        market
            .orders_of(trader)
            .filter(|o| status.is_none_or(|s| o.status == s))
            .take(q.limit.unwrap_or(100).clamp(1, 1_000))
            .cloned()
            .collect(),
    ))
}

async fn list_orders(
    State(app): State<AppState>,
    Path(symbol): Path<String>,
    Query(q): Query<TraderQuery>,
    caller: Caller,
) -> Result<Json<Vec<OpenOrderDto>>, ApiError> {
    let market = app.market();
    owned_trader(&market, caller, TraderId(q.trader_id))?;
    let s = market
        .symbol(&symbol)
        .ok_or_else(|| ApiError::not_found(&symbol))?;
    Ok(Json(
        s.exchange
            .book()
            .orders_of(Owner::Trader(TraderId(q.trader_id)))
            .map(|o| OpenOrderDto::from_resting(s.info.symbol, o))
            .collect(),
    ))
}

async fn get_order(
    State(app): State<AppState>,
    Path((symbol, order_id)): Path<(String, u64)>,
    caller: Caller,
) -> Result<Json<OpenOrderDto>, ApiError> {
    let market = app.market();
    let s = market
        .symbol(&symbol)
        .ok_or_else(|| ApiError::not_found(&symbol))?;
    let o = s
        .exchange
        .book()
        .get(fehu::OrderId(order_id))
        .filter(|o| o.owner != Owner::Synthetic)
        .ok_or_else(|| ApiError::unknown_order(order_id))?;
    let dto = OpenOrderDto::from_resting(s.info.symbol, o);
    owned_trader(&market, caller, TraderId(dto.trader_id))?;
    Ok(Json(dto))
}

/// Replace a resting order with another at a new price or quantity.
///
/// This is a cancel and a fresh order, in that order and under one lock: the
/// replacement goes to the back of the queue at its price, and if it cannot
/// be placed — no cash, no shares, a halt — the old order is already gone.
/// The response says which order was withdrawn and how much of it had filled.
async fn amend_order(
    State(app): State<AppState>,
    Path((symbol, order_id)): Path<(String, u64)>,
    caller: Caller,
    payload: Result<Json<AmendRequest>, JsonRejection>,
) -> Result<Json<AmendResponse>, ApiError> {
    let Json(req) = payload.map_err(ApiError::bad_json)?;
    let trader = TraderId(req.trader_id);
    let client_order_id = clean_client_order_id(req.client_order_id)?;
    let (response, fills, replaced_filled) = {
        let mut market = app.market();
        owned_trader(&market, caller, trader)?;
        let idx = market
            .symbol_index(&symbol)
            .ok_or_else(|| ApiError::not_found(&symbol))?;
        let sym = market.symbols[idx].info.symbol;
        if let Some(closed) = market.symbols[idx].closed(app.clock.now()) {
            return Err(ApiError::closed(sym, closed));
        }
        let resting = *market.symbols[idx]
            .exchange
            .book()
            .get(fehu::OrderId(order_id))
            .filter(|o| o.owner == Owner::Trader(trader))
            .ok_or_else(|| ApiError::unknown_order(order_id))?;
        let order = Order {
            owner: Owner::Trader(trader),
            side: resting.side,
            kind: OrderKind::Limit {
                price_cents: req.price_cents.unwrap_or(resting.price_cents),
            },
            tif: fehu::TimeInForce::Gtc,
            qty: req.qty.unwrap_or(resting.remaining),
        };
        fehu::OrderBook::validate(&order).map_err(|e| ApiError::invalid_order(e.to_string()))?;

        // Withdraw the old one first: it would otherwise be in the way of its
        // own replacement, both as liquidity and as a reservation.
        let cancelled = market.symbols[idx]
            .exchange
            .cancel(fehu::OrderId(order_id), trader)
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
        let now = market.symbols[idx].exchange.clock().0;
        if let Some((t, account)) = market.trader_and_account(trader) {
            t.release(
                account,
                sym,
                cancelled.side,
                cancelled.remaining,
                cancelled.price_cents,
            );
        }
        market.cancel_order_record(sym, order_id, cancelled.remaining, now);

        if req.post_only {
            check_post_only(&market.symbols[idx], &order)?;
        }
        let crossing = self_crossing(&market.symbols[idx], &order);
        if !crossing.is_empty() {
            return Err(ApiError::self_trade(&crossing));
        }
        let (response, fills) = market
            .place(idx, trader, order, client_order_id)
            .map_err(ApiError::place)?;
        (
            response,
            fills,
            cancelled.qty.saturating_sub(cancelled.remaining),
        )
    };
    tracing::info!(
        trader = trader.0,
        symbol = response.symbol,
        replaced = order_id,
        order = response.order_id,
        qty = response.qty,
        "order amended"
    );
    for fill in fills {
        app.publish(StreamMessage::Fill {
            trader_id: fill.trader_id,
            fill,
        });
    }
    Ok(Json(AmendResponse {
        replaced_order_id: order_id,
        replaced_filled,
        order: response,
    }))
}

async fn cancel_order(
    State(app): State<AppState>,
    Path((symbol, order_id)): Path<(String, u64)>,
    Query(q): Query<TraderQuery>,
    caller: Caller,
) -> Result<Json<OpenOrderDto>, ApiError> {
    let trader = TraderId(q.trader_id);
    let mut market = app.market();
    owned_trader(&market, caller, trader)?;
    let idx = market
        .symbol_index(&symbol)
        .ok_or_else(|| ApiError::not_found(&symbol))?;
    let sym = market.symbols[idx].info.symbol;
    let cancelled = market.symbols[idx]
        .exchange
        .cancel(fehu::OrderId(order_id), trader)
        .map_err(|e| match e {
            fehu::CancelError::Unknown => ApiError::unknown_order(order_id),
            fehu::CancelError::NotOwner => {
                ApiError::new(StatusCode::FORBIDDEN, "not_owner", e.to_string())
            }
            _ => ApiError::bad_request(e.to_string()),
        })?;
    if let Some((t, account)) = market.trader_and_account(trader) {
        t.release(
            account,
            sym,
            cancelled.side,
            cancelled.remaining,
            cancelled.price_cents,
        );
    }
    let now = market.symbols[idx].exchange.clock().0;
    market.cancel_order_record(sym, order_id, cancelled.remaining, now);
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
    let mut market = app.market();
    owned_trader(&market, caller, trader)?;
    let mut out = Vec::new();
    for i in 0..market.symbols.len() {
        let sym = market.symbols[i].info.symbol;
        let now = market.symbols[i].exchange.clock().0;
        let cancelled = market.symbols[i].exchange.cancel_all(trader);
        if let Some((t, account)) = market.trader_and_account(trader) {
            for o in &cancelled {
                t.release(account, sym, o.side, o.remaining, o.price_cents);
                out.push(OpenOrderDto::from_resting(sym, o));
            }
        }
        for o in &cancelled {
            market.cancel_order_record(sym, o.id.0, o.remaining, now);
        }
    }
    Ok(Json(out))
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
    let mut market = app.market();
    let id = market.create_user(req.name, email, wall_now_ms());
    tracing::info!(user = id.0, "user created");
    let mut dto = user_dto(&market, id)?;
    // The one and only time the key is handed out.
    dto.api_key = market.keys.take_issued_key(id);
    Ok((StatusCode::CREATED, Json(dto)))
}

/// The caller, as a list of one: a user is not told about the others.
async fn list_users(State(app): State<AppState>, caller: Caller) -> Json<Vec<UserDto>> {
    let market = app.market();
    Json(user_dto(&market, caller.0).into_iter().collect())
}

async fn get_user(
    State(app): State<AppState>,
    Path(user_id): Path<u64>,
    caller: Caller,
) -> Result<Json<UserDto>, ApiError> {
    let user = UserId(user_id);
    owned_user(caller, user)?;
    let market = app.market();
    Ok(Json(user_dto(&market, user)?))
}

/// Every share the user owns, per symbol, across all of their traders.
async fn get_holdings(
    State(app): State<AppState>,
    Path(user_id): Path<u64>,
    caller: Caller,
) -> Result<Json<UserHoldingsResponse>, ApiError> {
    let user = UserId(user_id);
    owned_user(caller, user)?;
    let market = app.market();
    if !market.users.contains_key(&user) {
        return Err(ApiError::unknown_user(user_id));
    }
    Ok(Json(UserHoldingsResponse::new(
        user_id,
        market.user_holdings(user),
    )))
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
    let mut market = app.market();
    if !market.users.contains_key(&user) {
        return Err(ApiError::unknown_user(user_id));
    }
    let id = market
        .open_account(user, req.name, cash, wall_now_ms())
        .map_err(ApiError::money)?;
    tracing::info!(user = user_id, account = id.0, cash, "account opened");
    Ok((StatusCode::CREATED, Json(account_dto(&market, id)?)))
}

async fn list_user_accounts(
    State(app): State<AppState>,
    Path(user_id): Path<u64>,
    caller: Caller,
) -> Result<Json<Vec<AccountDto>>, ApiError> {
    let user = UserId(user_id);
    owned_user(caller, user)?;
    let market = app.market();
    if !market.users.contains_key(&user) {
        return Err(ApiError::unknown_user(user_id));
    }
    Ok(Json(
        market
            .accounts
            .values()
            .filter(|a| a.user_id == user)
            .map(|a| account_view(&market, a))
            .collect(),
    ))
}

/// The caller's own accounts.
async fn list_accounts(State(app): State<AppState>, caller: Caller) -> Json<Vec<AccountDto>> {
    let market = app.market();
    Json(
        market
            .accounts
            .values()
            .filter(|a| a.user_id == caller.0)
            .map(|a| account_view(&market, a))
            .collect(),
    )
}

async fn get_account(
    State(app): State<AppState>,
    Path(account_id): Path<u64>,
    caller: Caller,
) -> Result<Json<AccountDto>, ApiError> {
    let market = app.market();
    let id = AccountId(account_id);
    owned_account(&market, caller, id)?;
    Ok(Json(account_dto(&market, id)?))
}

/// Add money: `{"amount_cents": 500000}`. The amount is a positive integer
/// number of cents.
async fn deposit(
    State(app): State<AppState>,
    Path(account_id): Path<u64>,
    caller: Caller,
    payload: Result<Json<TransferRequest>, JsonRejection>,
) -> Result<Json<LedgerResponse>, ApiError> {
    let Json(req) = payload.map_err(ApiError::bad_json)?;
    let mut market = app.market();
    let id = AccountId(account_id);
    owned_account(&market, caller, id)?;
    let entry = market
        .accounts
        .get_mut(&id)
        .ok_or_else(|| ApiError::unknown_account(account_id))?
        .deposit(req.amount_cents, req.memo, wall_now_ms())
        .map_err(ApiError::money)?;
    tracing::info!(
        account = account_id,
        amount_cents = req.amount_cents,
        balance_cents = entry.balance_cents,
        "deposit"
    );
    Ok(Json(LedgerResponse {
        account: account_dto(&market, id)?,
        entries: vec![entry],
    }))
}

/// Take money out. Only the available balance can leave: cash reserved for
/// resting orders has to be freed by cancelling them first.
async fn withdraw(
    State(app): State<AppState>,
    Path(account_id): Path<u64>,
    caller: Caller,
    payload: Result<Json<TransferRequest>, JsonRejection>,
) -> Result<Json<LedgerResponse>, ApiError> {
    let Json(req) = payload.map_err(ApiError::bad_json)?;
    let mut market = app.market();
    let id = AccountId(account_id);
    owned_account(&market, caller, id)?;
    let entry = market
        .accounts
        .get_mut(&id)
        .ok_or_else(|| ApiError::unknown_account(account_id))?
        .withdraw(req.amount_cents, req.memo, wall_now_ms())
        .map_err(ApiError::money)?;
    tracing::info!(
        account = account_id,
        amount_cents = req.amount_cents,
        balance_cents = entry.balance_cents,
        "withdrawal"
    );
    Ok(Json(LedgerResponse {
        account: account_dto(&market, id)?,
        entries: vec![entry],
    }))
}

/// Freeze, reopen or close an account: `{"status": "frozen"}`.
async fn set_status(
    State(app): State<AppState>,
    Path(account_id): Path<u64>,
    caller: Caller,
    payload: Result<Json<StatusRequest>, JsonRejection>,
) -> Result<Json<AccountDto>, ApiError> {
    let Json(req) = payload.map_err(ApiError::bad_json)?;
    let mut market = app.market();
    let id = AccountId(account_id);
    owned_account(&market, caller, id)?;
    market
        .accounts
        .get_mut(&id)
        .ok_or_else(|| ApiError::unknown_account(account_id))?
        .set_status(req.status)
        .map_err(ApiError::money)?;
    tracing::info!(account = account_id, status = ?req.status, "account status");
    Ok(Json(account_dto(&market, id)?))
}

/// Check an account: its status, what it can do, and any broken invariant.
async fn validate_account(
    State(app): State<AppState>,
    Path(account_id): Path<u64>,
    caller: Caller,
) -> Result<Json<AccountCheck>, ApiError> {
    let market = app.market();
    owned_account(&market, caller, AccountId(account_id))?;
    let account = market
        .accounts
        .get(&AccountId(account_id))
        .ok_or_else(|| ApiError::unknown_account(account_id))?;
    Ok(Json(AccountCheck::new(account)))
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
    let market = app.market();
    let id = AccountId(account_id);
    owned_account(&market, caller, id)?;
    let account = market
        .accounts
        .get(&id)
        .ok_or_else(|| ApiError::unknown_account(account_id))?;
    Ok(Json(LedgerResponse {
        account: account_view(&market, account),
        entries: account.ledger(limit),
    }))
}

/// Add money to the account a trader trades on, without having to look its
/// account up first.
async fn trader_deposit(
    State(app): State<AppState>,
    Path(trader_id): Path<u64>,
    caller: Caller,
    payload: Result<Json<TransferRequest>, JsonRejection>,
) -> Result<Json<PortfolioDto>, ApiError> {
    let Json(req) = payload.map_err(ApiError::bad_json)?;
    let trader = TraderId(trader_id);
    let mut market = app.market();
    owned_trader(&market, caller, trader)?;
    let account_id = market
        .traders
        .get(&trader)
        .ok_or_else(|| ApiError::unknown_trader(trader_id))?
        .account_id;
    let entry = market
        .accounts
        .get_mut(&account_id)
        .ok_or_else(|| ApiError::unknown_account(account_id.0))?
        .deposit(req.amount_cents, req.memo, wall_now_ms())
        .map_err(ApiError::money)?;
    tracing::info!(
        trader = trader_id,
        account = account_id.0,
        amount_cents = req.amount_cents,
        balance_cents = entry.balance_cents,
        "deposit"
    );
    Ok(Json(portfolio(&market, trader)?))
}

fn user_dto(market: &Market, id: UserId) -> Result<UserDto, ApiError> {
    let user = market
        .users
        .get(&id)
        .ok_or_else(|| ApiError::unknown_user(id.0))?;
    let holdings = market.user_holdings(id);
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
            .map(Account::balance_cents)
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
    AccountDto::new(account, market.trader_on(account.id).map(|t| t.id.0))
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
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let Subscription {
        rx,
        seq,
        oldest_seq,
        replay,
        gap,
    } = app.subscribe(q.since);
    let (hello, viewer) = {
        let market = app.market();
        (
            StreamMessage::Hello {
                sim_now_ms: app.clock.now().0,
                time_scale: app.clock.scale,
                quotes: market.symbols.iter().map(SymbolState::quote).collect(),
                oldest_seq,
                gap,
            },
            q.api_key.as_deref().and_then(|k| market.keys.user_of(k)),
        )
    };
    // Ticks and events are public; a fill belongs to the trader that made it,
    // so it goes only to a stream that proved it speaks for that trader. The
    // replay buffer holds everybody's, so the same rule applies to it.
    let owner = Arc::clone(&app);
    let visible = move |m: &StreamMessage| match m {
        StreamMessage::Fill { trader_id, .. } | StreamMessage::StopTriggered { trader_id, .. } => {
            viewer.is_some_and(|user| {
                owner
                    .market()
                    .traders
                    .get(&TraderId(*trader_id))
                    .is_some_and(|t| t.user_id == user)
            })
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
    Sse::new(all).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}
