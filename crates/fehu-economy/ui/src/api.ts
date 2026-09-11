/** Typed client for the server's JSON endpoints. */

import type {
  AccountCheck,
  AccountStatus,
  BudgetDto,
  BudgetsResponse,
  AmendRequest,
  AmendResponse,
  AccountDto,
  ApiErrorBody,
  BarsResponse,
  BookResponse,
  CatalogEntry,
  CreateTraderRequest,
  EventRecord,
  EventsResponse,
  GameEventRequest,
  Health,
  InventoryResponse,
  Interval,
  Job,
  JobsResponse,
  LedgerResponse,
  OpenOrderDto,
  OrderRecord,
  OrderRequest,
  OrderResponse,
  OverviewDto,
  PortfolioDto,
  PushEventRequest,
  RecipesResponse,
  Reconciliation,
  SharesResponse,
  SymbolStatus,
  SymbolsResponse,
  TradesResponse,
  TransferRequest,
  UserHoldingsResponse,
  WalletDto,
  WorldResponse,
} from './types.js';

/**
 * A non-2xx response, carrying the server's `error.code` when it sent one
 * and, for a refusal that asks the client to wait, how long.
 */
export class ApiError extends Error {
  readonly status: number;
  readonly code: string;
  /** `Retry-After`, in seconds, on `rate_limited` and `overloaded`. */
  readonly retryAfterSecs: number | null;

  constructor(status: number, code: string, message: string, retryAfterSecs: number | null = null) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
    this.code = code;
    this.retryAfterSecs = retryAfterSecs;
  }
}

/**
 * The message to show for anything thrown by this module (or by `fetch`).
 *
 * Refusals a player can act on are said in their own words rather than as
 * `code: message`: how long to wait, that a post-only order would have
 * taken, that the order in the way is their own.
 */
export function errorMessage(e: unknown): string {
  if (e instanceof ApiError) {
    const wait = e.retryAfterSecs === null ? '' : ` — try again in ${e.retryAfterSecs}s`;
    switch (e.code) {
      case 'rate_limited':
        return `too fast${wait}`;
      case 'overloaded':
        return `server busy${wait}`;
      case 'post_only_would_cross':
        return 'post-only: that price would trade on arrival, not rest';
      case 'self_trade':
        return `self-trade: ${e.message}`;
      case 'revoked_api_key':
        return 'this key has been revoked';
      case 'market_closed':
        return 'the market is closed';
      case 'symbol_halted':
        return 'trading in this symbol is halted';
      case 'network':
        return `no answer from the server: ${e.message}`;
      default:
        return `${e.code}: ${e.message}`;
    }
  }
  if (e instanceof Error) return e.message;
  return String(e);
}

/**
 * The key the server issued when this player was created. Everything that
 * belongs to a user — their portfolio, orders, accounts and money — is sent
 * with it; market data needs none.
 */
let apiKey: string | null = null;

/** Send every following request as the holder of `key`. */
export function setApiKey(key: string | null): void {
  apiKey = key;
}

/** The key in use, for the one place that cannot set a header: the stream. */
export function currentApiKey(): string | null {
  return apiKey;
}

/**
 * The operator's key, which is a different principal from the player's.
 *
 * The dashboard needs it and the trading UI must not: a player key opens a
 * player's own things, an operator key opens everybody's. They are held apart
 * here so that no ordinary request can pick up the wrong one — {@link ops}
 * sends this, everything else sends {@link setApiKey}'s.
 *
 * A server with no `FEHU_ADMIN_KEY` set — the single-player default — admits
 * the operator routes without one, so this stays `null` and the dashboard
 * works out of the box.
 */
let adminKey: string | null = null;

export function setAdminKey(key: string | null): void {
  adminKey = key === null || key === '' ? null : key;
}

export function currentAdminKey(): string | null {
  return adminKey;
}

async function request<T>(path: string, init?: RequestInit, key = apiKey): Promise<T> {
  let response: Response;
  const withKey: RequestInit =
    key === null
      ? { ...init }
      : { ...init, headers: { ...(init?.headers ?? {}), authorization: `Bearer ${key}` } };
  try {
    response = await fetch(path, withKey);
  } catch (cause) {
    throw new ApiError(0, 'network', cause instanceof Error ? cause.message : 'request failed');
  }
  const body: unknown = await response.json().catch(() => ({}));
  if (!response.ok) {
    const err = (body as ApiErrorBody).error;
    const retryAfter = response.headers.get('retry-after');
    const secs = retryAfter === null ? NaN : Number(retryAfter);
    throw new ApiError(
      response.status,
      err?.code ?? 'http_error',
      err?.message ?? `HTTP ${response.status}`,
      Number.isFinite(secs) ? secs : null,
    );
  }
  return body as T;
}

/** A fresh `Idempotency-Key`: 128 bits from the platform, or a fallback. */
function idempotencyKey(): string {
  if (typeof crypto !== 'undefined' && typeof crypto.randomUUID === 'function') {
    return crypto.randomUUID();
  }
  return `${Date.now().toString(36)}-${Math.random().toString(36).slice(2)}`;
}

/**
 * A write. Every one carries an `Idempotency-Key`, and one whose answer was
 * lost on the wire — the request threw before a response came back — is
 * sent once more under the same key, so a deposit or an order the server
 * did apply is answered rather than applied twice. A response the server
 * *gave*, refusal included, is never retried here: that is the caller's.
 */
async function send<T>(
  method: 'POST' | 'PATCH' | 'DELETE',
  path: string,
  body?: unknown,
  key = apiKey,
): Promise<T> {
  const headers: Record<string, string> = { 'idempotency-key': idempotencyKey() };
  const init: RequestInit = { method, headers };
  if (body !== undefined) {
    headers['content-type'] = 'application/json';
    init.body = JSON.stringify(body);
  }
  try {
    return await request<T>(path, init, key);
  } catch (e) {
    if (e instanceof ApiError && e.code === 'network') {
      return request<T>(path, init, key);
    }
    throw e;
  }
}

const enc = encodeURIComponent;

export const api = {
  symbols: (): Promise<SymbolsResponse> => request('/api/symbols'),

  bars: (symbol: string, interval: Interval, limit: number): Promise<BarsResponse> =>
    request(`/api/symbols/${enc(symbol)}/bars?interval=${interval}&limit=${limit}`),

  events: (limit: number): Promise<EventsResponse> => request(`/api/events?limit=${limit}`),

  catalog: (): Promise<CatalogEntry[]> => request('/api/game/catalog'),

  book: (symbol: string, depth: number): Promise<BookResponse> =>
    request(`/api/symbols/${enc(symbol)}/book?depth=${depth}`),

  trades: (symbol: string, limit: number): Promise<TradesResponse> =>
    request(`/api/symbols/${enc(symbol)}/trades?limit=${limit}`),

  /** Where one symbol's shares are: outstanding, held, bid for, available. */
  shares: (symbol: string): Promise<SharesResponse> =>
    request(`/api/symbols/${enc(symbol)}/shares`),

  /** Whether a symbol can be traded right now, and if not, why not. */
  status: (symbol: string): Promise<SymbolStatus> =>
    request(`/api/symbols/${enc(symbol)}/status`),

  trader: (id: number): Promise<PortfolioDto> => request(`/api/traders/${id}`),

  /** Every share a user owns, per symbol, across all of their traders. */
  holdings: (userId: number): Promise<UserHoldingsResponse> =>
    request(`/api/users/${userId}/holdings`),

  createTrader: (body: CreateTraderRequest): Promise<PortfolioDto> =>
    send('POST', '/api/traders', body),

  /** Add money to the account a trader trades on. */
  deposit: (traderId: number, body: TransferRequest): Promise<PortfolioDto> =>
    send('POST', `/api/traders/${traderId}/deposit`, body),

  account: (id: number): Promise<AccountDto> => request(`/api/accounts/${id}`),

  ledger: (id: number, limit: number): Promise<LedgerResponse> =>
    request(`/api/accounts/${id}/ledger?limit=${limit}`),

  validateAccount: (id: number): Promise<AccountCheck> => request(`/api/accounts/${id}/validate`),

  submitOrder: (symbol: string, body: OrderRequest): Promise<OrderResponse> =>
    send('POST', `/api/symbols/${enc(symbol)}/orders`, body),

  /** One order by id, filled and cancelled ones included. */
  order: (orderId: number): Promise<OrderRecord> => request(`/api/orders/${orderId}`),

  /** A trader's orders, newest first. */
  traderOrders: (traderId: number, limit: number): Promise<OrderRecord[]> =>
    request(`/api/traders/${traderId}/orders?limit=${limit}`),

  /** Replace a resting order with another at a new price or quantity. */
  amendOrder: (symbol: string, orderId: number, body: AmendRequest): Promise<AmendResponse> =>
    send('PATCH', `/api/symbols/${enc(symbol)}/orders/${orderId}`, body),

  cancelOrder: (symbol: string, orderId: number, traderId: number): Promise<OpenOrderDto> =>
    send('DELETE', `/api/symbols/${enc(symbol)}/orders/${orderId}?trader_id=${traderId}`),

  /** One wallet by id: the owner's or the operator's to read. */
  wallet: (walletId: number): Promise<WalletDto> => request(`/api/wallets/${walletId}`),

  /** A trader's units of the world's goods, reservations included. */
  inventory: (traderId: number): Promise<InventoryResponse> =>
    request(`/api/traders/${traderId}/inventory`),

  /** What the world knows how to make. */
  recipes: (): Promise<RecipesResponse> => request('/api/recipes'),

  /** The caller's jobs, running and finished, oldest first. */
  jobs: (): Promise<JobsResponse> => request('/api/jobs'),

  /** Start a job: the inputs and the cost now, the outputs when it is due. */
  startJob: (traderId: number, recipe: string, runs = 1): Promise<Job> =>
    send('POST', '/api/jobs', { trader_id: traderId, recipe, runs }),

  /** Stop one before it is due. What comes back is what the recipe says. */
  cancelJob: (jobId: number): Promise<Job> => send('POST', `/api/jobs/${jobId}/cancel`, {}),

  /** What game events are doing to production and demand. */
  world: (): Promise<WorldResponse> => request('/api/world'),

  /** The pools rewards are paid from. Operator authority. */
  budgets: (): Promise<BudgetsResponse> => request('/api/budgets'),

  pushGameEvent: (body: GameEventRequest): Promise<EventRecord> =>
    send('POST', '/api/game/events', body),

  pushSimEvent: (symbol: string, body: PushEventRequest): Promise<EventRecord> =>
    send('POST', `/api/symbols/${enc(symbol)}/events`, body),
};

/**
 * The operator's endpoints, sent with the operator's key.
 *
 * Everything here either reads the whole world or changes what the world is
 * allowed to do, so none of it belongs on a player's key. The reads are two:
 * one consistent picture of the economy, and the server's own counters.
 * Everything else is a mutation the server journals like any other command.
 *
 * `reconcile` is deliberately not on the polling path: it takes a snapshot of
 * the entire market, so it is asked for when somebody wants an audit.
 */
export const ops = {
  /** The whole economy, taken at one instant. */
  overview: (): Promise<OverviewDto> => request('/api/overview', undefined, adminKey),

  /** Is the server up, is it keeping up, and since when. */
  health: (): Promise<Health> => request('/api/health', undefined, adminKey),

  /** The audit: does the currency add up, and is every unit somewhere. */
  reconcile: (): Promise<Reconciliation> => request('/api/reconcile', undefined, adminKey),

  /** Create currency into an account. The one way supply goes up. */
  mint: (accountId: number, amountCents: number): Promise<unknown> =>
    send('POST', `/api/accounts/${accountId}/deposit`, { amount_cents: amountCents }, adminKey),

  /** Destroy currency out of an account. The one way it goes down. */
  burn: (accountId: number, amountCents: number): Promise<unknown> =>
    send('POST', `/api/accounts/${accountId}/withdraw`, { amount_cents: amountCents }, adminKey),

  /** Freeze an account (it may still be paid) or let it move money again. */
  setAccountStatus: (accountId: number, status: AccountStatus): Promise<unknown> =>
    send('POST', `/api/accounts/${accountId}/status`, { status }, adminKey),

  /** Open a budget, funded out of treasury. Nothing is minted. */
  createBudget: (name: string, cashCents: number): Promise<BudgetDto> =>
    send('POST', '/api/budgets', { name, cash_cents: cashCents }, adminKey),

  /** Top one up, out of treasury again. */
  fundBudget: (wallet: number, amountCents: number): Promise<unknown> =>
    send('POST', `/api/budgets/${wallet}/fund`, { amount_cents: amountCents }, adminKey),

  /** What a named reward is worth, and which budget pays it. */
  setRewardRule: (id: string, budget: number, amountCents: number): Promise<unknown> =>
    send('POST', '/api/rewards/rules', { id, budget, amount_cents: amountCents }, adminKey),

  removeRewardRule: (id: string): Promise<unknown> =>
    send('DELETE', `/api/rewards/rules/${enc(id)}`, undefined, adminKey),

  /** Switch a merchant's quoting on or off. Its money and stock stay put. */
  setNpcActive: (traderId: number, active: boolean): Promise<unknown> =>
    send('POST', `/api/npcs/${traderId}/active`, { active }, adminKey),

  /** Stop trading in a symbol, or start it again. */
  halt: (symbol: string): Promise<unknown> =>
    send('POST', `/api/symbols/${enc(symbol)}/halt`, {}, adminKey),

  resume: (symbol: string): Promise<unknown> =>
    send('POST', `/api/symbols/${enc(symbol)}/resume`, {}, adminKey),
};
