//! HTTP surface: JSON endpoints, the SSE stream and the embedded UI.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
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
use crate::market::{
    App, Market, Quote, SnapshotDto, StreamMessage, SymbolInfo, SymbolState, wall_now_ms,
};
use crate::trading::{
    BookDto, CreateTraderRequest, HolderDto, MAX_CLIENT_ORDER_ID, OpenOrderDto, OrderRecord,
    OrderRequest, OrderResponse, PortfolioDto, PositionDto, Refused, TradeDto, TraderSummary,
    UserHoldingsResponse,
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
        .route("/api/symbols", get(list_symbols))
        .route("/api/symbols/{symbol}", get(get_symbol))
        .route("/api/symbols/{symbol}/bars", get(get_bars))
        .route(
            "/api/symbols/{symbol}/events",
            get(list_symbol_events).post(push_sim_event),
        )
        .route("/api/symbols/{symbol}/shares", get(get_shares))
        .route("/api/symbols/{symbol}/book", get(get_book))
        .route("/api/symbols/{symbol}/trades", get(get_trades))
        .route(
            "/api/symbols/{symbol}/orders",
            get(list_orders).post(submit_order),
        )
        .route(
            "/api/symbols/{symbol}/orders/{order_id}",
            get(get_order).delete(cancel_order),
        )
        .route("/api/traders", get(list_traders).post(create_trader))
        .route("/api/traders/{trader_id}", get(get_trader))
        .route("/api/traders/{trader_id}/orders", get(list_trader_orders))
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
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(app)
}

/// JSON error body: `{"error": {"code": ..., "message": ...}}`.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
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

    fn invalid_order(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_order", message)
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
        (self.status, Json(body)).into_response()
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
    /// Every trader holding shares, largest stake first.
    holders: Vec<HolderDto>,
}

impl SharesDto {
    fn new(market: &Market, s: &SymbolState) -> Self {
        let sym = s.info.symbol;
        let mut holders: Vec<HolderDto> = market
            .traders
            .values()
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
) -> Result<Json<SymbolDetail>, ApiError> {
    let market = app.market();
    let s = market
        .symbol(&symbol)
        .ok_or_else(|| ApiError::not_found(&symbol))?;
    Ok(Json(SymbolDetail {
        info: s.info,
        quote: s.quote(),
        snapshot: s.sim().snapshot().into(),
        config: s.sim().config().clone(),
        ticks_total: s.ticks_total,
        shares: SharesDto::new(&market, s),
    }))
}

async fn get_shares(
    State(app): State<AppState>,
    Path(symbol): Path<String>,
) -> Result<Json<SharesDto>, ApiError> {
    let market = app.market();
    let s = market
        .symbol(&symbol)
        .ok_or_else(|| ApiError::not_found(&symbol))?;
    Ok(Json(SharesDto::new(&market, s)))
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
    let _ = app.tx.send(StreamMessage::Event(record.clone()));
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
    let _ = app.tx.send(StreamMessage::Event(record.clone()));
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
            let user = UserId(user_id);
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
    let dto = portfolio(&market, id)?;
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

async fn list_traders(State(app): State<AppState>) -> Json<Vec<TraderSummary>> {
    let market = app.market();
    Json(
        market
            .traders
            .values()
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
) -> Result<Json<PortfolioDto>, ApiError> {
    let market = app.market();
    Ok(Json(portfolio(&market, TraderId(trader_id))?))
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
        fills: t.fills.iter().rev().cloned().collect(),
    })
}

async fn submit_order(
    State(app): State<AppState>,
    Path(symbol): Path<String>,
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
        if !market.traders.contains_key(&trader) {
            return Err(ApiError::unknown_trader(trader.0));
        }
        let sym = market.symbols[idx].info.symbol;
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
            // Worst-case cash a buy can consume.
            let cost = match order.kind {
                OrderKind::Limit { price_cents } => {
                    i64::try_from(i128::from(price_cents) * i128::from(order.qty))
                        .unwrap_or(i64::MAX)
                }
                OrderKind::Market => {
                    market.symbols[idx]
                        .exchange
                        .preview_market(order.side, order.qty)
                        .notional_cents
                }
            };
            // A symbol has a fixed number of shares: a buy can only be filled
            // from the ones no trader holds or is already bidding for.
            if order.side == fehu::Side::Buy {
                let available = market.available_shares(sym);
                if order.qty > available {
                    return Err(ApiError::refused(Refused::SupplyExhausted {
                        needed: order.qty,
                        available,
                    }));
                }
            }
            // Validate the order against the account that would fund it: it must
            // be active, a buy must have the cash available, and a sell the
            // shares — nothing may be sold that the trader does not hold.
            let account = market
                .account_of(trader)
                .ok_or_else(|| ApiError::unknown_trader(trader.0))?;
            market.traders[&trader]
                .check(account, sym, order.side, order.qty, cost)
                .map_err(ApiError::refused)?;
            let placement = market.symbols[idx]
                .exchange
                .submit(order)
                .map_err(|e| ApiError::invalid_order(e.to_string()))?;
            market.symbols[idx].record_trades(&placement.trades);
            let fills = market.apply_trades(sym, &placement.trades);
            if placement.status == fehu::OrderStatus::Resting
                && let OrderKind::Limit { price_cents } = order.kind
                && let Some((t, account)) = market.trader_and_account(trader)
            {
                t.reserve(account, sym, order.side, placement.remaining, price_cents);
            }
            let response = OrderResponse::new(sym, trader, order.side, order.qty, &placement);
            let now = market.symbols[idx].exchange.clock().0;
            market.record_order(OrderRecord::new(
                client_order_id,
                trader,
                sym,
                &order,
                &placement,
                response.clone(),
                now,
            ));
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
        let _ = app.tx.send(StreamMessage::Fill {
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
) -> Result<Json<OrderRecord>, ApiError> {
    let market = app.market();
    market
        .order(order_id)
        .cloned()
        .map(Json)
        .ok_or_else(|| ApiError::unknown_order(order_id))
}

/// A trader's orders, newest first.
async fn list_trader_orders(
    State(app): State<AppState>,
    Path(trader_id): Path<u64>,
    Query(q): Query<OrderHistoryQuery>,
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
    if !market.traders.contains_key(&trader) {
        return Err(ApiError::unknown_trader(trader_id));
    }
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
) -> Result<Json<Vec<OpenOrderDto>>, ApiError> {
    let market = app.market();
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
    Ok(Json(OpenOrderDto::from_resting(s.info.symbol, o)))
}

async fn cancel_order(
    State(app): State<AppState>,
    Path((symbol, order_id)): Path<(String, u64)>,
    Query(q): Query<TraderQuery>,
) -> Result<Json<OpenOrderDto>, ApiError> {
    let trader = TraderId(q.trader_id);
    let mut market = app.market();
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
    market.cancel_order_record(order_id, cancelled.remaining, now);
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
) -> Result<Json<Vec<OpenOrderDto>>, ApiError> {
    let trader = TraderId(trader_id);
    let mut market = app.market();
    if !market.traders.contains_key(&trader) {
        return Err(ApiError::unknown_trader(trader_id));
    }
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
            market.cancel_order_record(o.id.0, o.remaining, now);
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
    Ok((StatusCode::CREATED, Json(user_dto(&market, id)?)))
}

async fn list_users(State(app): State<AppState>) -> Json<Vec<UserDto>> {
    let market = app.market();
    Json(
        market
            .users
            .keys()
            .filter_map(|&id| user_dto(&market, id).ok())
            .collect(),
    )
}

async fn get_user(
    State(app): State<AppState>,
    Path(user_id): Path<u64>,
) -> Result<Json<UserDto>, ApiError> {
    let market = app.market();
    Ok(Json(user_dto(&market, UserId(user_id))?))
}

/// Every share the user owns, per symbol, across all of their traders.
async fn get_holdings(
    State(app): State<AppState>,
    Path(user_id): Path<u64>,
) -> Result<Json<UserHoldingsResponse>, ApiError> {
    let user = UserId(user_id);
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
    payload: Option<Json<OpenAccountRequest>>,
) -> Result<(StatusCode, Json<AccountDto>), ApiError> {
    let req = payload.map(|Json(r)| r).unwrap_or_default();
    let cash = req.cash_cents.unwrap_or(app.options.starting_cash_cents);
    let user = UserId(user_id);
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
) -> Result<Json<Vec<AccountDto>>, ApiError> {
    let user = UserId(user_id);
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

async fn list_accounts(State(app): State<AppState>) -> Json<Vec<AccountDto>> {
    let market = app.market();
    Json(
        market
            .accounts
            .values()
            .map(|a| account_view(&market, a))
            .collect(),
    )
}

async fn get_account(
    State(app): State<AppState>,
    Path(account_id): Path<u64>,
) -> Result<Json<AccountDto>, ApiError> {
    let market = app.market();
    Ok(Json(account_dto(&market, AccountId(account_id))?))
}

/// Add money: `{"amount_cents": 500000}`. The amount is a positive integer
/// number of cents.
async fn deposit(
    State(app): State<AppState>,
    Path(account_id): Path<u64>,
    payload: Result<Json<TransferRequest>, JsonRejection>,
) -> Result<Json<LedgerResponse>, ApiError> {
    let Json(req) = payload.map_err(ApiError::bad_json)?;
    let mut market = app.market();
    let id = AccountId(account_id);
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
    payload: Result<Json<TransferRequest>, JsonRejection>,
) -> Result<Json<LedgerResponse>, ApiError> {
    let Json(req) = payload.map_err(ApiError::bad_json)?;
    let mut market = app.market();
    let id = AccountId(account_id);
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
    payload: Result<Json<StatusRequest>, JsonRejection>,
) -> Result<Json<AccountDto>, ApiError> {
    let Json(req) = payload.map_err(ApiError::bad_json)?;
    let mut market = app.market();
    let id = AccountId(account_id);
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
) -> Result<Json<AccountCheck>, ApiError> {
    let market = app.market();
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
) -> Result<Json<LedgerResponse>, ApiError> {
    let limit = q
        .limit
        .unwrap_or(100)
        .clamp(1, app.options.ledger_log.max(1));
    let market = app.market();
    let id = AccountId(account_id);
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
    payload: Result<Json<TransferRequest>, JsonRejection>,
) -> Result<Json<PortfolioDto>, ApiError> {
    let Json(req) = payload.map_err(ApiError::bad_json)?;
    let trader = TraderId(trader_id);
    let mut market = app.market();
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
/// and every accepted event. Each message is JSON with a `type` field.
async fn stream(State(app): State<AppState>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = app.tx.subscribe();
    let hello = {
        let market = app.market();
        StreamMessage::Hello {
            sim_now_ms: app.clock.now().0,
            time_scale: app.clock.scale,
            quotes: market.symbols.iter().map(SymbolState::quote).collect(),
        }
    };
    // A lagging client silently skips the messages it missed.
    let live = BroadcastStream::new(rx).filter_map(Result::ok);
    let all = tokio_stream::once(hello)
        .chain(live)
        .filter_map(|m| Event::default().json_data(&m).ok().map(Ok));
    Sse::new(all).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}
