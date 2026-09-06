/** Typed client for the server's JSON endpoints. */

import type {
  ApiErrorBody,
  BarsResponse,
  BookResponse,
  CatalogEntry,
  CreateTraderRequest,
  EventRecord,
  EventsResponse,
  GameEventRequest,
  Interval,
  OpenOrderDto,
  OrderRequest,
  OrderResponse,
  PortfolioDto,
  PushEventRequest,
  SymbolsResponse,
  TradesResponse,
} from './types.js';

/** A non-2xx response, carrying the server's `error.code` when it sent one. */
export class ApiError extends Error {
  readonly status: number;
  readonly code: string;

  constructor(status: number, code: string, message: string) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
    this.code = code;
  }
}

/** The message to show for anything thrown by this module (or by `fetch`). */
export function errorMessage(e: unknown): string {
  if (e instanceof ApiError) return `${e.code}: ${e.message}`;
  if (e instanceof Error) return e.message;
  return String(e);
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  let response: Response;
  try {
    response = await fetch(path, init);
  } catch (cause) {
    throw new ApiError(0, 'network', cause instanceof Error ? cause.message : 'request failed');
  }
  const body: unknown = await response.json().catch(() => ({}));
  if (!response.ok) {
    const err = (body as ApiErrorBody).error;
    throw new ApiError(
      response.status,
      err?.code ?? 'http_error',
      err?.message ?? `HTTP ${response.status}`,
    );
  }
  return body as T;
}

function send<T>(method: 'POST' | 'DELETE', path: string, body?: unknown): Promise<T> {
  const init: RequestInit = { method };
  if (body !== undefined) {
    init.headers = { 'content-type': 'application/json' };
    init.body = JSON.stringify(body);
  }
  return request<T>(path, init);
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

  trader: (id: number): Promise<PortfolioDto> => request(`/api/traders/${id}`),

  createTrader: (body: CreateTraderRequest): Promise<PortfolioDto> => send('POST', '/api/traders', body),

  submitOrder: (symbol: string, body: OrderRequest): Promise<OrderResponse> =>
    send('POST', `/api/symbols/${enc(symbol)}/orders`, body),

  cancelOrder: (symbol: string, orderId: number, traderId: number): Promise<OpenOrderDto> =>
    send('DELETE', `/api/symbols/${enc(symbol)}/orders/${orderId}?trader_id=${traderId}`),

  pushGameEvent: (body: GameEventRequest): Promise<EventRecord> =>
    send('POST', '/api/game/events', body),

  pushSimEvent: (symbol: string, body: PushEventRequest): Promise<EventRecord> =>
    send('POST', `/api/symbols/${enc(symbol)}/events`, body),
};
