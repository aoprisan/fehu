/**
 * The HTTP/SSE contract with the Rust server.
 *
 * Every type here mirrors a `#[derive(Serialize)]` struct or enum on the
 * server; the source is named above each block. `crates/fehu-economy/tests/contract.rs`
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

// --- crates/fehu-economy/src/market.rs -------------------------------------

/**
 * `symbol::AssetKind` — what a listing is. A `stock` has a fixed float and
 * pays dividends; a `good` is issued and consumed and has neither.
 */
export type AssetKind = 'stock' | 'good';

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
  /** What this listing is. */
  asset_kind: AssetKind;
  /** What one unit of a good is called; `null` for a stock. */
  unit: string | null;
  /** Units in existence: a stock's shares, or a good's issued less consumed. */
  shares_outstanding: number;
  /** `price × shares_outstanding`. */
  market_cap_cents: number;
  /** A session is running. */
  market_open: boolean;
  /** Trading is stopped. */
  halted: boolean;
}

/** `market::HaltReason`. */
export type HaltReason = 'limit_move' | 'manual';

/** `market::Halt` — trading in one symbol, stopped. */
export interface Halt {
  reason: HaltReason;
  since_ms: number;
  /** When an automatic halt lifts; `null` for a manual one. */
  until_ms: number | null;
  band_cents: number;
  price_cents: number;
  /** How far the price had moved from the band, as a fraction. */
  move_pct: number;
}

/** `GET /api/symbols/{symbol}/status` (`market::SymbolStatus`). */
export interface SymbolStatus {
  symbol: string;
  ts_ms: number;
  market_open: boolean;
  halted: boolean;
  /** Orders are accepted: open, and not halted. */
  tradable: boolean;
  halt: Halt | null;
  next_open_ms: number | null;
  next_close_ms: number | null;
  band_cents: number;
  move_pct: number;
  /** The move that stops trading; `0` when automatic halts are off. */
  limit_pct: number;
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
  asset_kind: AssetKind;
  /** What one unit of a good is called; `null` for a stock. */
  unit: string | null;
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

// --- crates/fehu-economy/src/events.rs -------------------------------------

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
  /** What it does to production and demand, alongside the price. */
  world: EffectSpec[];
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

// --- crates/fehu-economy/src/trading.rs ------------------------------------

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
  /** Everything still open: what is on show plus what an iceberg holds back. */
  remaining: number;
  /** The slice an iceberg shows at a time; `null` for an ordinary order. */
  display_qty: number | null;
  /** What is on show right now. Equal to `remaining` unless it is an iceberg. */
  shown_qty: number;
  ts_ms: number;
}

/**
 * `trading::StopOrder` — a trigger held aside until the price touches it.
 * It is not an order: it rests nowhere and reserves nothing until it fires,
 * and then it becomes an ordinary order.
 */
export interface StopOrder {
  stop_id: number;
  trader_id: number;
  symbol: string;
  side: Side;
  qty: number;
  /** A buy fires at or above this price, a sell at or below. */
  stop_price_cents: number;
  /** The limit the fired order carries; `null` fires a market order. */
  limit_price_cents: number | null;
  /** The time in force of the order it fires, not of the trigger itself. */
  tif: TimeInForce;
  client_order_id: string | null;
  created_at_ms: number;
}

/** Body of `POST /api/symbols/{symbol}/stops`. */
export interface StopRequest {
  trader_id: number;
  side: Side;
  qty: number;
  stop_price_cents: number;
  /** Absent makes it a stop-market rather than a stop-limit. */
  limit_price_cents?: number | null;
  tif?: TimeInForce;
  client_order_id?: string | null;
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
  /**
   * Simulated time this order is withdrawn at if it is still resting: a
   * good-till-date order's deadline, or a day order's session close. `null`
   * leaves it resting until it fills or is cancelled.
   */
  expires_at_ms: number | null;
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
  /**
   * The venue's fee, signed the way the ledger is: negative was taken out of
   * the account, positive was a rebate paid in. Its own ledger entry, never
   * folded into the price.
   */
  fee_cents: number;
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
  /** Triggers waiting for a price, oldest first. */
  stops: StopOrder[];
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

// --- crates/fehu-economy/src/account.rs ------------------------------------

/** `account::AccountStatus`. */
export type AccountStatus = 'active' | 'frozen' | 'closed';

/** `account::LedgerKind`. */
export type LedgerKind =
  | 'open'
  | 'deposit'
  | 'withdrawal'
  | 'buy'
  | 'sell'
  | 'fee'
  | 'purchase'
  | 'dividend'
  | 'delisting'
  | 'transfer_out'
  | 'transfer_in'
  | 'reward'
  | 'job_cost'
  | 'job_refund';

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
  /** The wallet in the currency ledger this account's money lives in. */
  wallet_id: number;
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
  /** The balanced ledger transaction this entry is one side of. */
  tx_id: number;
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
  /**
   * The order must rest: if it would trade on arrival it is refused. Only
   * meaningful for a `gtc` limit order.
   */
  post_only?: boolean;
  /**
   * Show only this much at a time, keeping the rest back and posting the
   * next slice — at the back of the queue for its price — as each one fills.
   * Only a `gtc` limit order can hide anything.
   */
  display_qty?: number;
  /**
   * Simulated time at which the resting remainder is withdrawn. Absent
   * leaves it resting until it fills or is cancelled.
   */
  expires_at_ms?: number;
  /**
   * A day order: the resting remainder is withdrawn at the close of the
   * session it was sent in. Needs a trading calendar.
   */
  day?: boolean;
};

/** Body of `PATCH /api/symbols/{symbol}/orders/{id}`. */
export interface AmendRequest {
  trader_id: number;
  /** New limit price; unchanged if absent. */
  price_cents?: number;
  /** New quantity; what is still resting if absent. */
  qty?: number;
  client_order_id?: string;
  post_only?: boolean;
}

/**
 * Response to an amendment. An amendment is a cancel and a fresh order, so
 * the replacement is a new order at the back of the queue for its price.
 */
export type AmendResponse = OrderResponse & {
  replaced_order_id: number;
  /** Shares of the replaced order that had already filled. */
  replaced_filled: number;
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

/**
 * First message on every connection. Its `seq` is where the connection joins
 * rather than a number of its own: the next message is `seq + 1`.
 */
export interface HelloMessage {
  type: 'hello';
  sim_now_ms: number;
  time_scale: number;
  quotes: Quote[];
  /** The earliest sequence `?since=` can still ask for. */
  oldest_seq: number;
  /**
   * This connection asked to resume from further back than the server's
   * replay buffer reaches: messages were missed for good, and the client
   * should reload its snapshots rather than trust its state.
   */
  gap: boolean;
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

/** A symbol stopped trading, or started again. */
export type StatusMessage = { type: 'status' } & SymbolStatus;

/**
 * A resting order was withdrawn by the venue rather than by its owner: it
 * reached its expiry, or its symbol was delisted.
 */
export interface OrderExpiredMessage {
  type: 'order_expired';
  trader_id: number;
  order: OrderRecord;
}

/**
 * A stop fired. It is held no longer: it either became `order`, or was
 * `refused` when the account was checked the second time.
 */
export interface StopTriggeredMessage {
  type: 'stop_triggered';
  trader_id: number;
  stop: StopOrder;
  /** The price that reached the trigger. */
  price_cents: number;
  order: OrderResponse | null;
  refused: string | null;
}

/** A symbol was listed: it is quoted and tradable from this message on. */
export interface ListedMessage {
  type: 'listed';
  quote: Quote;
}

/** What a delisting undid, and what it paid for the shares. */
export interface Delisting {
  symbol: string;
  /** Paid on every share held. Zero is a real answer. */
  cents_per_share: number;
  /** What the symbol last traded at, for comparison. */
  last_price_cents: number;
  orders_cancelled: number;
  stops_cancelled: number;
  shares_bought_out: number;
  accounts_paid: number;
  total_cents: number;
}

/**
 * A symbol was delisted. Its book and stops are gone, every holder has been
 * bought out, and orders in it are refused from here on. `Delisting` is
 * flattened alongside the tag.
 */
export type DelistedMessage = { type: 'delisted' } & Delisting;

/**
 * Every message carries the sequence number it was published under, so a
 * client can tell a quiet market from a gap and resume with `?since=`.
 */
export type Sequenced<M> = M & { seq: number };

/**
 * A production job came due and delivered what it made. There is no message
 * for a job starting: that is a command with a response, and its owner
 * already has it.
 */
export type JobDoneMessage = { type: 'job_done' } & JobDelivery;

/** One published message, before the stream numbers it. */
export type StreamPayload =
  | HelloMessage
  | TickMessage
  | EventMessage
  | FillMessage
  | StatusMessage
  | StopTriggeredMessage
  | OrderExpiredMessage
  | ListedMessage
  | DelistedMessage
  | JobDoneMessage;

export type StreamMessage = Sequenced<StreamPayload>;

/**
 * What the outbox carries: everything the stream publishes except the
 * per-connection `hello` and the price `tick`.
 */
export type OutboxPayload = Exclude<
  StreamPayload,
  HelloMessage | TickMessage
>;

// --- the outbox (outbox.rs) ------------------------------------------------

/**
 * One committed fact, waiting for the game backend. The payload in `event` is
 * the same object the SSE stream carries, minus its `seq` — the outbox
 * numbers its entries itself.
 */
export interface OutboxEntry {
  /** Dense from 1, never reused. This is the cursor. */
  seq: number;
  /** The journal sequence of the command that caused it. */
  command_seq: number;
  /** The payload's `type`, lifted out so a consumer can route without parsing. */
  kind: string;
  /** Simulated instant the command ran at. */
  at_ms: number;
  /** Wall-clock instant it arrived. */
  wall_ms: number;
  event: OutboxPayload;
}

/** `GET /api/outbox` (`outbox::Page`). */
export interface OutboxPage {
  events: OutboxEntry[];
  /** What to send as `after` next time. */
  next: number;
  /** How far the server has recorded the consumer as having read. */
  cursor: number;
  /** The lowest sequence still held; `0` when nothing is. */
  oldest: number;
  /** The highest ever appended. */
  latest: number;
  /** Entries still waiting after `next`. */
  pending: number;
  /** Facts evicted before they were acknowledged, ever. */
  dropped: number;
  /** The read started further back than the log reaches: resynchronise. */
  gap: boolean;
  /** Entries this world keeps. `0` means the outbox is switched off. */
  cap: number;
}

/** `POST /api/outbox/ack` (`outbox::Cursor`). */
export interface OutboxCursor {
  cursor: number;
  oldest: number;
  latest: number;
  pending: number;
  dropped: number;
  cap: number;
}

/** Body of `POST /api/outbox/ack`. */
export interface OutboxAckBody {
  /** The highest sequence the consumer has finished with. */
  through: number;
}

/** The server's error body: `{"error": {"code", "message"}}`. */
export interface ApiErrorBody {
  error?: { code: string; message: string };
}

/** Game-master audit of one consistent market snapshot. */
export interface Reconciliation {
  valid: boolean;
  accounts_checked: number;
  traders_checked: number;
  symbols_checked: number;
  resting_orders_checked: number;
  /** Wallets in the currency ledger, the four the world always has included. */
  wallets_checked: number;
  minted_cents: number;
  burned_cents: number;
  /** `minted − burned`. */
  outstanding_cents: number;
  /** What the wallets hold. Equal to `outstanding_cents` in a healthy world. */
  circulating_cents: number;
  /** What unfunded liquidity has put into players' hands beyond its float. */
  synthetic_debt_cents: number;
  /** Jobs still in the furnace: promises the world has taken payment for. */
  jobs_running: number;
  /** Budget wallets rewards are paid from. */
  budgets_checked: number;
  /** Players the game backend has provisioned, each checked against the
   *  user, account, trader and wallet its mapping names. */
  players_checked: number;
  issues: string[];
}

/** `GET /api/supply`: how much currency exists, and where it sits. */
export interface SupplyDto {
  minted_cents: number;
  burned_cents: number;
  /** `minted − burned`. */
  outstanding_cents: number;
  /** What the wallets actually hold. */
  circulating_cents: number;
  /** The two agree: currency is conserved. */
  balanced: boolean;
  treasury_cents: number;
  venue_cents: number;
  issuer_cents: number;
  player_cents: number;
  /** Sitting in the tills of the traders the world runs itself. */
  npc_cents: number;
  /** Set aside in budgets, waiting to be paid out as rewards. */
  budget_cents: number;
  synthetic_debt_cents: number;
  wallets: number;
}

// --- crates/fehu-economy/src/catalog.rs ------------------------------------

/**
 * `catalog::CatalogItem` — one line of the catalogue: a good, its price, and
 * how much of it is left to make.
 */
export interface CatalogItem {
  symbol: string;
  /** What one unit costs. Always at least a cent. */
  price_cents: number;
  /** Units this line may still issue; `null` for a seam that never runs out. */
  available: number | null;
  /** Units it has issued since the line was written. */
  issued: number;
  note: string | null;
}

/** `GET /api/catalog` (`catalog::CatalogResponse`). */
export interface CatalogResponse {
  items: CatalogItem[];
}

/**
 * `POST /api/traders/{id}/purchases` (`catalog::PurchaseReceipt`): currency
 * to the good's issuer, units that did not exist to the buyer.
 */
export interface PurchaseReceipt {
  trader_id: number;
  symbol: string;
  qty: number;
  unit_price_cents: number;
  /** `qty × unit_price_cents`, the amount that moved. */
  total_cents: number;
  /** The balanced transaction that moved it. */
  tx_id: number;
  /** What the trader holds of the good now. */
  position_qty: number;
  /** Units of the good in existence now. */
  units_outstanding: number;
  /** What the line has left to make, or `null` for a seam. */
  available: number | null;
}

/**
 * `POST /api/traders/{id}/consume` (`catalog::ConsumeReceipt`). No currency
 * moves: a thing used up is not a thing sold, so there is no transaction to
 * name.
 */
export interface ConsumeReceipt {
  trader_id: number;
  symbol: string;
  qty: number;
  position_qty: number;
  units_outstanding: number;
}

// --- crates/fehu-economy/src/npc.rs ----------------------------------------

/**
 * `npc::Policy` — how an NPC quotes: a ladder of its own, in basis points of
 * the reference price.
 */
export interface NpcPolicy {
  /** Half the spread. The best bid sits this far below the reference. */
  half_spread_bps: number;
  levels: number;
  level_step_bps: number;
  /** Units quoted at each level. */
  size: number;
  /** How far the reference must move before the quotes are redrawn. */
  requote_bps: number;
}

/**
 * `npc::NpcDto` — one of the traders the world runs itself, and what it has
 * left. Its bid disappears when its wallet is empty and its ask when its
 * inventory is.
 */
export interface NpcDto {
  trader_id: number;
  user_id: number;
  account_id: number;
  symbol: string;
  name: string;
  policy: NpcPolicy;
  /** Quoting is on. Off, it keeps its money and stock and stops offering them. */
  active: boolean;
  /** What it quotes at each level now: its policy size, scaled by demand. */
  quoted_size: number;
  /** Currency it can still bid with. */
  cash_cents: number;
  /** Units it holds. */
  inventory: number;
  /** Units already promised to resting sells. */
  reserved: number;
}

/** `GET /api/npcs` (`npc::NpcsResponse`). */
export interface NpcsResponse {
  npcs: NpcDto[];
}


// --- crates/fehu-economy/src/jobs.rs ---------------------------------------

/** `jobs::Line` — how many units of what, on one side of a recipe. */
export interface RecipeLine {
  symbol: string;
  qty: number;
}

/** `jobs::Recipe` — what the world knows how to make, and what it takes. */
export interface Recipe {
  id: string;
  /** Bumped every time an operator rewrites it. */
  version: number;
  /** Units consumed when a job starts. */
  inputs: RecipeLine[];
  /** Units issued when it completes, before the world's effect on the yield. */
  outputs: RecipeLine[];
  /** What the furnace charges, paid to the venue when the job starts. */
  cost_cents: number;
  /** How long it takes, in simulated seconds. */
  duration_secs: number;
  /** What a cancellation gives back, in basis points of the cost. */
  refund_bps: number;
  note: string | null;
}

/** `GET /api/recipes` (`jobs::RecipesResponse`). */
export interface RecipesResponse {
  recipes: Recipe[];
}

/** `jobs::JobStatus`. */
export type JobStatus = 'running' | 'done' | 'cancelled';

/** `jobs::Job` — one run of a recipe. */
export interface Job {
  id: number;
  recipe: string;
  /** The version of the recipe this job was started under. */
  recipe_version: number;
  trader_id: number;
  account_id: number;
  /** What it took, and what it will deliver. Both fixed when it started. */
  inputs: RecipeLine[];
  outputs: RecipeLine[];
  /** The yield the world was running at when it started; 10000 is as written. */
  yield_bps: number;
  cost_cents: number;
  /** The balanced transaction that paid the cost, or 0 for a free recipe. */
  tx_id: number;
  /** What the inputs cost their owner: it goes into what the output cost. */
  inputs_cost_cents: number;
  started_at_ms: number;
  due_at_ms: number;
  finished_at_ms: number | null;
  status: JobStatus;
  /** What a cancellation actually paid back. */
  refunded_cents: number;
}

/** `GET /api/jobs` (`jobs::JobsResponse`). */
export interface JobsResponse {
  jobs: Job[];
}

/** What a completed job delivered (`jobs::JobDelivery`). */
export interface JobDelivery {
  job_id: number;
  trader_id: number;
  recipe: string;
  /** What actually arrived: a line whose good was delisted delivers nothing. */
  delivered: RecipeLine[];
  at_ms: number;
}

// --- crates/fehu-economy/src/rewards.rs ------------------------------------

/** `rewards::BudgetDto` — a pool rewards are paid out of. */
export interface BudgetDto {
  wallet: number;
  name: string;
  created_at_ms: number;
  /** What the wallet holds now: whether the next reward can be paid. */
  balance_cents: number;
  paid_cents: number;
  paid_count: number;
}

/** `rewards::RewardRule` — what a named reward is worth. */
export interface RewardRule {
  id: string;
  version: number;
  /** The budget's wallet id. */
  budget: number;
  amount_cents: number;
  note: string | null;
  paid_cents: number;
  paid_count: number;
}

/** `GET /api/budgets` (`rewards::BudgetsResponse`). */
export interface BudgetsResponse {
  budgets: BudgetDto[];
  rules: RewardRule[];
}

/** `POST /api/rewards` (`rewards::RewardReceipt`). */
export interface RewardReceipt {
  rule: string;
  /** The game's own id for what happened. Paid at most once. */
  source: string;
  trader_id: number;
  account_id: number;
  budget: number;
  amount_cents: number;
  tx_id: number;
  balance_cents: number;
  at_ms: number;
  /** The receipt of an earlier payment: nothing moved for this request. */
  duplicate: boolean;
}

// --- crates/fehu-economy/src/world.rs --------------------------------------

/** What a modifier changes (`world::Effect`). */
export type Effect = 'production' | 'demand';

/** `world::EffectSpec` — one event's pull, and how long it lasts. */
export interface EffectSpec {
  effect: Effect;
  /** At full strength, in basis points: 2000 is a fifth more. */
  delta_bps: number;
  /** How long it takes to ramp down to nothing, in simulated seconds. */
  secs: number;
}

/** `world::Modifier` — a pull in force, ramping down in a straight line. */
export interface Modifier {
  id: number;
  effect: Effect;
  /** The symbol it hits, or `null` for every symbol. */
  symbol: string | null;
  delta_bps: number;
  from_ms: number;
  until_ms: number;
  /** `"game:scandal"`, as the event log spells it. */
  kind: string;
  source: string;
}

/** One symbol's standing with the world (`world::SymbolEffects`). */
export interface SymbolEffects {
  symbol: string;
  /** What a recipe making this yields, in basis points of the recipe. */
  production_bps: number;
  /** What a merchant in this quotes, in basis points of its policy size. */
  demand_bps: number;
}

/** `GET /api/world` (`world::WorldResponse`). */
export interface WorldResponse {
  at_ms: number;
  modifiers: Modifier[];
  symbols: SymbolEffects[];
}

// --- wallets and transfers -------------------------------------------------

/** `api::WalletDto` — one wallet: what it is, and what it holds. */
export interface WalletDto {
  wallet: number;
  kind:
    | 'player'
    | 'treasury'
    | 'budget'
    | 'npc'
    | 'issuer'
    | 'venue'
    | 'issuance'
    | 'synthetic';
  status: AccountStatus;
  balance_cents: number;
  reserved_cents: number;
  available_cents: number;
  /** The account this wallet is the money of, if it is somebody's. */
  account_id: number | null;
}

/** What a wallet is for. `market::WalletRow['kind']`. */
export type WalletKind = WalletDto['kind'];

/**
 * `market::WalletRow` — one wallet in the operator's directory: what it
 * holds, and whose it is.
 *
 * The same facts as {@link WalletDto}, plus the one a list needs and a single
 * read does not: a name to show instead of a number.
 */
export interface WalletRow {
  wallet: number;
  kind: WalletKind;
  status: AccountStatus;
  balance_cents: number;
  reserved_cents: number;
  /** `balance − reserved`, never below zero. */
  available_cents: number;
  account_id: number | null;
  /**
   * The account's name, the merchant's, the budget's, or the symbol whose
   * payouts it funds. `null` for the four wallets the world always has.
   */
  owner: string | null;
}

/** `ledger::Reason`'s label: why currency moved. */
export type FlowReason =
  | 'genesis'
  | 'mint'
  | 'burn'
  | 'transfer'
  | 'faucet'
  | 'reward'
  | 'purchase'
  | 'fee'
  | 'rebate'
  | 'buy'
  | 'sell'
  | 'dividend'
  | 'delisting'
  | 'job_cost'
  | 'job_refund'
  | 'migration';

/**
 * `market::FlowDto` — what one reason has moved since genesis.
 *
 * Running totals, never a rate: two readings and the time between them are
 * what a rate is made of, and the server keeps no history to make one from.
 */
export interface FlowDto {
  reason: FlowReason;
  count: number;
  cents: number;
}

/** `market::JobsSummary` — what is in the furnace. */
export interface JobsSummary {
  /** Jobs the book holds, running and finished. */
  held: number;
  running: number;
  done: number;
  cancelled: number;
  /** When the next running job is due, or `null` if none is. */
  next_due_ms: number | null;
}

/** `market::PeopleSummary` — who is in the world. */
export interface PeopleSummary {
  users: number;
  accounts: number;
  traders: number;
  /** Players the game backend has provisioned. */
  players: number;
  /** Accounts whose wallet is frozen, and whose is closed. */
  frozen: number;
  closed: number;
}

/**
 * `GET /api/overview` (`market::OverviewDto`) — the whole economy in one
 * consistent read, for the operator's dashboard. One market job, so every
 * number in it was true at the same instant.
 */
export interface OverviewDto {
  /** Simulated time the reading was taken at. */
  at_ms: number;
  supply: SupplyDto;
  /** Every reason, in a fixed order, whether it has moved anything or not. */
  flows: FlowDto[];
  /** Every wallet, in id order. */
  wallets: WalletRow[];
  budgets: BudgetDto[];
  rules: RewardRule[];
  npcs: NpcDto[];
  effects: SymbolEffects[];
  modifiers: Modifier[];
  jobs: JobsSummary;
  people: PeopleSummary;
  outbox: OutboxCursor;
  /** Commands applied since the world began. */
  journal_seq: number;
}

/** `metrics::TimingDto` — a count, a total and a worst case since start-up. */
export interface TimingDto {
  count: number;
  micros_last: number;
  /** Never decays: it is a high-water mark. */
  micros_max: number;
  micros_mean: number;
}

/** `metrics::MetricsDto` — what the server has been doing, and how fast. */
export interface MetricsDto {
  requests: TimingDto;
  requests_failed: number;
  requests_limited: number;
  /** Turned away because the server was already full. */
  requests_shed: number;
  engine_step: TimingDto;
}

/** `GET /api/health` — is it up, is it keeping up, and since when. */
export interface Health {
  status: string;
  uptime_secs: number;
  sim_now_ms: number;
  time_scale: number;
  symbols: number;
  events_logged: number;
  ticks_total: number;
  trades_total: number;
  users: number;
  accounts: number;
  traders: number;
  cash_cents: number;
  resting_orders: number;
  stops_held: number;
  orders_placed: number;
  orders_refused: number;
  fills_booked: number;
  /** Zero in a healthy market: shares moved and money did not. */
  settlement_failures: number;
  stream_messages: number;
  stream_subscribers: number;
  requests_in_flight: number;
  /** `0` means there is no bound. */
  max_in_flight: number;
  max_streams: number;
  tracked_clients: number;
  metrics: MetricsDto;
}

/** `api::InventoryResponse` — a trader's units of the world's goods. */
export interface InventoryResponse {
  trader_id: number;
  inventory: HoldingDto[];
}

/** Body of `POST /api/transfers`. */
export interface TransferBody {
  from_account_id: number;
  to_account_id: number;
  amount_cents: number;
  memo?: string | null;
}

// --- crates/fehu-economy/src/service.rs ------------------------------------

/**
 * One thing a service credential may do (`service::Scope`).
 *
 * Named `ServiceScope` here and not `Scope`, because `events::Scope` — what a
 * game event reaches — already has that name and TypeScript has no modules
 * to keep the two apart.
 *
 * A scope narrows a credential; it does not narrow the operator, which
 * reaches every route a scope opens exactly as it did before there were any.
 */
export type ServiceScope = 'provision' | 'reward' | 'inventory' | 'events';

/**
 * `service::ServiceDto` — a credential the game backend speaks with.
 *
 * The digest it is stored as is never in the response, and `api_key` holds
 * the key only in the one response that issued it.
 */
export interface ServiceDto {
  id: number;
  name: string;
  scopes: ServiceScope[];
  revoked: boolean;
  created_ms: number;
  revoked_ms: number | null;
  /** Shown once, in the response that issued it; `null` everywhere after. */
  api_key: string | null;
}

/** `GET /api/v1/economy/admin/services` (`service::ServicesResponse`). */
export interface ServicesResponse {
  services: ServiceDto[];
}

/** Body of `POST /api/v1/economy/admin/services`. */
export interface ServiceRequest {
  name: string;
  scopes: ServiceScope[];
}

/**
 * `account::PlayerDto` — a player the game already has, mapped onto this
 * world's ids. `POST /api/v1/economy/players` is idempotent on
 * `external_id`, so a repeat answers `created: false` and no key.
 */
export interface PlayerDto {
  /** The game's own id for this player. */
  external_id: string;
  user_id: number;
  account_id: number;
  trader_id: number;
  wallet_id: number;
  created_at_ms: number;
  /** Whether this call is what made the mapping. */
  created: boolean;
  /** Shown once, in the response that provisioned them; `null` on a repeat. */
  api_key: string | null;
}

/** `GET /api/v1/economy/players` (`account::PlayersResponse`). */
export interface PlayersResponse {
  players: PlayerDto[];
}

/** Body of `POST /api/v1/economy/players`. */
export interface ProvisionRequest {
  external_id: string;
  name?: string | null;
  email?: string | null;
}
