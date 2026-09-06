//! HTTP surface: JSON endpoints, the SSE stream and the embedded UI.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use fehu::{Candle, Config, Interval};
use serde::{Deserialize, Serialize};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{Stream, StreamExt};
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

use crate::events::{
    CatalogEntry, EventRecord, GameEventKind, GameEventRequest, MAX_MAGNITUDE, Prepared,
    PushEventRequest, Scope, SimEvent,
};
use crate::market::{App, Quote, SnapshotDto, StreamMessage, SymbolInfo, SymbolState, wall_now_ms};

type AppState = Arc<App>;

const INDEX_HTML: &str = include_str!("../static/index.html");

/// Build the router over a shared [`App`].
pub fn router(app: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/health", get(health))
        .route("/api/symbols", get(list_symbols))
        .route("/api/symbols/{symbol}", get(get_symbol))
        .route("/api/symbols/{symbol}/bars", get(get_bars))
        .route(
            "/api/symbols/{symbol}/events",
            get(list_symbol_events).post(push_sim_event),
        )
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

#[derive(Serialize)]
struct Health {
    status: &'static str,
    uptime_secs: u64,
    sim_now_ms: i64,
    time_scale: f64,
    symbols: usize,
    events_logged: usize,
    ticks_total: u64,
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
        snapshot: s.sim.snapshot().into(),
        config: s.sim.config().clone(),
        ticks_total: s.ticks_total,
    }))
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
            .apply(&mut s.sim, at)
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
            let sim = &mut market.symbols[i].sim;
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
