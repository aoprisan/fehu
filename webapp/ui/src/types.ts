/**
 * The HTTP/SSE contract with the Rust server.
 *
 * Every type here mirrors a `#[derive(Serialize)]` struct or enum on the
 * server; the source is named above each block. `webapp/tests/contract.rs`
 * asserts the JSON key sets these declarations assume, so a field renamed in
 * Rust fails the Rust test suite instead of silently rendering `undefined`.
 *
 * Money is always integer cents. Timestamps are milliseconds since the Unix
 * epoch, UTC — `*_ms` fields are simulated time unless named `received_at_ms`,
 * which is wall-clock.
 */

// --- fehu core (src/book.rs, src/candles.rs) -------------------------------

/** `fehu::Interval` — serialised as the variant name. */
export type Interval = 'M1' | 'M5' | 'H1' | 'D1';

/** `fehu::Side`. */
export type Side = 'buy' | 'sell';

/** `fehu::TimeInForce`. */
export type TimeInForce = 'gtc' | 'ioc' | 'fok';

/** `fehu::OrderStatus`. */
export type OrderStatus = 'filled' | 'resting' | 'cancelled';

/** `fehu::Candle`. `open_ts` is a transparent `Timestamp`, i.e. a number. */
export interface Candle {
  open_ts: number;
  open: number;
  high: number;
  low: number;
  close: number;
  volume: number;
  ticks: number;
}

/** `fehu::Level` — aggregated depth at one price. */
export interface Level {
  price_cents: number;
  qty: number;
  orders: number;
}

// --- webapp/src/market.rs --------------------------------------------------

/** `market::Quote`. */
export interface Quote {
  symbol: string;
  name: string;
  sector: string;
  ts_ms: number;
  price_cents: number;
  prev_close_cents: number | null;
  change_pct: number | null;
  day_open_cents: number | null;
  day_high_cents: number | null;
  day_low_cents: number | null;
  day_volume: number;
  fundamental_cents: number;
  annual_vol: number;
  pending_events: number;
  bid_cents: number | null;
  ask_cents: number | null;
  /** Shares in existence for this symbol. */
  shares_outstanding: number;
  /** `price × shares_outstanding`. */
  market_cap_cents: number;
}

/** `api::HolderDto` — one trader's stake in a symbol. */
export interface HolderDto {
  trader_id: number;
  user_id: number;
  qty: number;
  reserved_shares: number;
  free_shares: number;
}

/**
 * `GET /api/symbols/{symbol}/shares` (`api::SharesDto`): where the symbol's
 * shares are. The parts add up: `outstanding = held + bid_for + available`.
 */
export interface SharesResponse {
  symbol: string;
  shares_outstanding: number;
  /** Held by traders. */
  held_shares: number;
  /** Bid for by traders' resting buy orders. */
  bid_shares: number;
  /** Neither held nor bid for: what a buy can still be filled from. */
  available_shares: number;
  price_cents: number;
  market_cap_cents: number;
  /** Largest stake first. */
  holders: HolderDto[];
}

/** `GET /api/symbols`. */
export interface SymbolsResponse {
  sim_now_ms: number;
  symbols: Quote[];
}

/** `GET /api/symbols/{symbol}/bars`. */
export interface BarsResponse {
  symbol: string;
  interval: Interval;
  interval_ms: number;
  sim_now_ms: number;
  bars: Candle[];
}

// --- webapp/src/events.rs --------------------------------------------------

/** `events::SimEvent` — internally tagged on `type`. */
export type SimEvent =
  | { type: 'jump'; pct: number }
  | { type: 'drift_shift'; delta: number; half_life_secs: number }
  | { type: 'drift_for_total_move'; total: number; half_life_secs: number }
  | { type: 'vol_shift'; delta: number; half_life_secs: number }
  | { type: 'fundamental_shift'; delta: number }
  | { type: 'fundamental_target'; target_cents: number };

/** The discriminant of {@link SimEvent}. */
export type SimEventType = SimEvent['type'];

/** `events::GameEventKind`. */
export type GameEventKind =
  | 'product_launch'
  | 'earnings_beat'
  | 'earnings_miss'
  | 'scandal'
  | 'lawsuit'
  | 'buyback'
  | 'ceo_resigns'
  | 'hype'
  | 'market_crash'
  | 'market_rally'
  | 'rate_hike'
  | 'rate_cut';

/** `events::Scope`. */
export type Scope = 'company' | 'market';

/** Largest `magnitude` the server accepts (`events::MAX_MAGNITUDE`). */
export const MAX_MAGNITUDE = 5.0;

/** One entry of `GET /api/game/catalog`. */
export interface CatalogEntry {
  kind: GameEventKind;
  label: string;
  scope: Scope;
  description: string;
  effects: SimEvent[];
}

/** `events::EventRecord` — the audit-log row, also pushed to the stream. */
export interface EventRecord {
  id: number;
  received_at_ms: number;
  at_ms: number;
  symbols: string[];
  /** `"game:scandal"` or `"sim:jump"`. */
  kind: string;
  source: string;
  note: string | null;
  magnitude: number | null;
  effects: SimEvent[];
  summary: string[];
}

/** `GET /api/events`. */
export interface EventsResponse {
  sim_now_ms: number;
  /** Newest first. */
  events: EventRecord[];
}

/** `events::Timing` — give at most one of the two. */
export interface Timing {
  at_ms?: number;
  delay_secs?: number;
}

/** Body of `POST /api/symbols/{symbol}/events`. */
export type PushEventRequest = SimEvent &
  Timing & {
    source?: string;
    note?: string;
  };

/** Body of `POST /api/game/events`. */
export interface GameEventRequest extends Timing {
  kind: GameEventKind;
  /** Required for company-scoped kinds. */
  symbol?: string;
  magnitude?: number;
  source?: string;
  note?: string;
}

// --- webapp/src/trading.rs -------------------------------------------------

/** `trading::BookDto`. */
export interface BookDto {
  bids: Level[];
  asks: Level[];
}

/** `GET /api/symbols/{symbol}/book` — `BookDto` is flattened into it. */
export interface BookResponse extends BookDto {
  symbol: string;
  ts_ms: number;
  reference_cents: number;
  bid_cents: number | null;
  ask_cents: number | null;
  pending_flow: number;
}

/** `trading::TradeDto` — one print on the tape. */
export interface TradeDto {
  ts_ms: number;
  price_cents: number;
  qty: number;
  taker_side: Side;
  taker_order_id: number;
  maker_order_id: number;
  /** `null` for synthetic liquidity. */
  taker_trader: number | null;
  maker_trader: number | null;
  hidden: boolean;
}

/** `GET /api/symbols/{symbol}/trades`. */
export interface TradesResponse {
  symbol: string;
  sim_now_ms: number;
  /** Newest first. */
  trades: TradeDto[];
}

/** `trading::OpenOrderDto`. */
export interface OpenOrderDto {
  symbol: string;
  order_id: number;
  trader_id: number;
  side: Side;
  price_cents: number;
  qty: number;
  remaining: number;
  ts_ms: number;
}

/**
 * `trading::OrderRecord` — one submitted order and what became of it. The
 * book forgets an order once it is filled or cancelled; this does not.
 */
export interface OrderRecord {
  order_id: number;
  /** The caller's own id for the order, if it gave one. */
  client_order_id: string | null;
  trader_id: number;
  symbol: string;
  side: Side;
  kind: 'market' | 'limit';
  /** The limit price; `null` for a market order. */
  price_cents: number | null;
  tif: TimeInForce;
  qty: number;
  filled: number;
  /** Not executed: resting, or withdrawn when cancelled. */
  remaining: number;
  status: OrderStatus;
  notional_cents: number;
  avg_price_cents: number | null;
  submitted_at_ms: number;
  updated_at_ms: number;
}

/** `trading::FillRecord`. */
export interface FillRecord {
  id: number;
  trader_id: number;
  ts_ms: number;
  symbol: string;
  order_id: number;
  side: Side;
  qty: number;
  price_cents: number;
  /** `"maker"` if the trader's order was resting, `"taker"` if it took. */
  liquidity: 'maker' | 'taker';
  counterparty: 'synthetic' | 'trader';
}

/** `trading::PositionDto`. */
export interface PositionDto {
  symbol: string;
  qty: number;
  avg_cost_cents: number | null;
  mark_cents: number;
  market_value_cents: number;
  unrealised_pnl_cents: number;
  realised_pnl_cents: number;
  /** Shares promised to resting sell orders. */
  reserved_shares: number;
  /** `qty − reserved_shares`: the most this trader may still sell. */
  free_shares: number;
}

/** `trading::HoldingDto` — one user's shares in one symbol. */
export interface HoldingDto {
  symbol: string;
  qty: number;
  reserved_shares: number;
  /** What the user can still sell. */
  free_shares: number;
  cost_cents: number;
  avg_cost_cents: number | null;
  mark_cents: number;
  market_value_cents: number;
  unrealised_pnl_cents: number;
  realised_pnl_cents: number;
  /** The user's traders holding this symbol. */
  traders: number[];
}

/** `GET /api/users/{id}/holdings` (`trading::UserHoldingsResponse`). */
export interface UserHoldingsResponse {
  user_id: number;
  shares_owned: number;
  reserved_shares: number;
  free_shares: number;
  market_value_cents: number;
  /** One entry per symbol the user holds, by ticker. */
  holdings: HoldingDto[];
}

/** `GET /api/traders/{id}`. */
export interface PortfolioDto {
  id: number;
  user_id: number;
  account_id: number;
  name: string;
  created_at_ms: number;
  account_status: AccountStatus;
  /** The account's balance. */
  cash_cents: number;
  reserved_cents: number;
  free_cash_cents: number;
  equity_cents: number;
  realised_pnl_cents: number;
  unrealised_pnl_cents: number;
  positions: PositionDto[];
  open_orders: OpenOrderDto[];
  /** Newest first. */
  fills: FillRecord[];
  /**
   * The key that proves a request speaks for this user, shown **once**: in
   * the response that created them, and `null` everywhere after. Send it as
   * `Authorization: Bearer <key>`.
   */
  api_key: string | null;
}

/** Body of `POST /api/traders`. */
export interface CreateTraderRequest {
  name?: string;
  cash_cents?: number;
  /** Attach to an existing user instead of creating one. */
  user_id?: number;
  /** Trade on an existing account of `user_id`. */
  account_id?: number;
  email?: string;
}

// --- webapp/src/account.rs -------------------------------------------------

/** `account::AccountStatus`. */
export type AccountStatus = 'active' | 'frozen' | 'closed';

/** `account::LedgerKind`. */
export type LedgerKind = 'open' | 'deposit' | 'withdrawal' | 'buy' | 'sell';

/** `account::UserDto`. */
export interface UserDto {
  id: number;
  name: string;
  email: string | null;
  created_at_ms: number;
  accounts: number[];
  traders: number[];
  /** Every account's balance added up. */
  balance_cents: number;
  /** Shares owned across every symbol and every trader of the user. */
  shares_owned: number;
  /** Those shares at the reference prices. */
  holdings_value_cents: number;
  /**
   * The key that proves a request speaks for this user, shown **once**: in
   * the response that created them, and `null` everywhere after. Send it as
   * `Authorization: Bearer <key>`.
   */
  api_key: string | null;
}

/** `account::AccountDto`. Money is integer cents. */
export interface AccountDto {
  id: number;
  user_id: number;
  name: string;
  status: AccountStatus;
  opened_at_ms: number;
  balance_cents: number;
  /** Held against resting buy orders. */
  reserved_cents: number;
  /** `balance − reserved`: what an order or a withdrawal can use. */
  available_cents: number;
  deposited_cents: number;
  withdrawn_cents: number;
  entries_total: number;
  trader_id: number | null;
  valid: boolean;
}

/** `account::LedgerEntry` — one movement of money. */
export interface LedgerEntry {
  id: number;
  ts_ms: number;
  kind: LedgerKind;
  /** Signed: positive credits the account, negative debits it. */
  amount_cents: number;
  /** The balance after this entry. */
  balance_cents: number;
  symbol: string | null;
  order_id: number | null;
  memo: string | null;
}

/** `GET /api/accounts/{id}/ledger`, and the reply to a deposit. */
export interface LedgerResponse {
  account: AccountDto;
  /** Newest first. */
  entries: LedgerEntry[];
}

/** `GET /api/accounts/{id}/validate`. */
export interface AccountCheck {
  account_id: number;
  status: AccountStatus;
  valid: boolean;
  issues: string[];
  balance_cents: number;
  reserved_cents: number;
  available_cents: number;
  can_trade: boolean;
  can_deposit: boolean;
  can_withdraw: boolean;
}

/** Body of `POST /api/accounts/{id}/deposit` and `.../withdraw`. */
export interface TransferRequest {
  /** A positive integer number of cents. */
  amount_cents: number;
  memo?: string;
}

/** Body of `POST /api/users`. */
export interface CreateUserRequest {
  name?: string;
  email?: string;
}

/** Body of `POST /api/users/{id}/accounts`. */
export interface OpenAccountRequest {
  name?: string;
  cash_cents?: number;
}

/** Body of `POST /api/accounts/{id}/status`. */
export interface StatusRequest {
  status: AccountStatus;
}

/** `fehu::OrderKind`, internally tagged on `type`. */
export type OrderKind = { type: 'market' } | { type: 'limit'; price_cents: number };

/** Body of `POST /api/symbols/{symbol}/orders`. */
export type OrderRequest = OrderKind & {
  trader_id: number;
  side: Side;
  qty: number;
  tif: TimeInForce;
  /**
   * Caller-chosen id, unique per trader, that makes the submission
   * idempotent: the same order sent twice is placed once and the first
   * response replayed (`200` rather than `201`).
   */
  client_order_id?: string;
};

/** Response to a submitted order. */
export interface OrderResponse {
  symbol: string;
  trader_id: number;
  order_id: number;
  side: Side;
  qty: number;
  filled: number;
  remaining: number;
  status: OrderStatus;
  avg_price_cents: number | null;
  notional_cents: number;
  trades: TradeDto[];
}

// --- the SSE stream (market::StreamMessage) --------------------------------

/** First message on every connection. */
export interface HelloMessage {
  type: 'hello';
  sim_now_ms: number;
  time_scale: number;
  quotes: Quote[];
}

/** The last tick of one engine step for one symbol. */
export interface TickMessage {
  type: 'tick';
  symbol: string;
  ts_ms: number;
  price_cents: number;
  volume: number;
  /** Intervals whose bar closed during the step: refetch instead of extending. */
  closed: Interval[];
  bid_cents: number | null;
  ask_cents: number | null;
  book: BookDto;
  /** Newest prints of the step, newest first. */
  trades: TradeDto[];
}

/** An accepted event. `EventRecord` is flattened alongside the tag. */
export type EventMessage = { type: 'event' } & EventRecord;

/** A trader's order executed, in whole or in part. */
export interface FillMessage {
  type: 'fill';
  trader_id: number;
  fill: FillRecord;
}

export type StreamMessage = HelloMessage | TickMessage | EventMessage | FillMessage;

/** The server's error body: `{"error": {"code", "message"}}`. */
export interface ApiErrorBody {
  error?: { code: string; message: string };
}
