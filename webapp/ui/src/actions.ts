/**
 * Everything that writes to the store: fetches, stream application and the
 * user-initiated commands. Panels call these; nothing else mutates state.
 */

import { api, errorMessage } from './api.js';
import { bucketOf } from './intervals.js';
import { setOrderStatus, setStatus } from './status.js';
import { MAX_EVENTS, MAX_LIVE_FILLS, MAX_TAPE, type Store } from './store.js';
import type {
  CatalogEntry,
  EventRecord,
  FillRecord,
  Interval,
  OpenOrderDto,
  OrderRequest,
  PushEventRequest,
  TickMessage,
} from './types.js';

const BAR_LIMIT = 2000;
const EVENT_LIMIT = MAX_EVENTS;
const BOOK_DEPTH = 8;
const TAPE_LIMIT = 40;
const TRADER_STORAGE_KEY = 'fehu.trader_id';

/** `localStorage` is unavailable in some privacy modes; degrade, don't crash. */
function readStoredTraderId(): number | null {
  try {
    const raw = localStorage.getItem(TRADER_STORAGE_KEY);
    if (raw === null) return null;
    const id = Number.parseInt(raw, 10);
    return Number.isSafeInteger(id) ? id : null;
  } catch {
    return null;
  }
}

function storeTraderId(id: number): void {
  try {
    localStorage.setItem(TRADER_STORAGE_KEY, String(id));
  } catch {
    // Not fatal: the session just will not survive a reload.
  }
}

export class Actions {
  readonly #store: Store;

  constructor(store: Store) {
    this.#store = store;
  }

  get #state() {
    return this.#store.state;
  }

  async loadSymbols(): Promise<void> {
    const r = await api.symbols();
    for (const q of r.symbols) this.#state.quotes.set(q.symbol, q);
    this.#state.simNow = r.sim_now_ms;
    this.#state.symbol ??= r.symbols[0]?.symbol ?? null;
    this.#store.emit('symbols', 'clock');
  }

  async loadBars(): Promise<void> {
    const { symbol, interval } = this.#state;
    if (symbol === null || this.#state.barsLoading) return;
    this.#state.barsLoading = true;
    this.#store.emit('bars');
    try {
      const r = await api.bars(symbol, interval, BAR_LIMIT);
      // Discard a response the user has already navigated away from.
      if (r.symbol !== this.#state.symbol || r.interval !== this.#state.interval) return;
      this.#state.bars = r.bars;
    } catch (e) {
      setStatus(errorMessage(e), true);
    } finally {
      this.#state.barsLoading = false;
      this.#store.emit('bars');
    }
  }

  async loadEvents(): Promise<void> {
    const r = await api.events(EVENT_LIMIT);
    this.#state.events = r.events;
    this.#store.emit('events');
  }

  async loadCatalog(): Promise<void> {
    this.#state.catalog = await api.catalog();
    this.#store.emit('catalog');
  }

  /** Reuse the trader from a previous visit, or open a fresh account. */
  async loadTrader(): Promise<void> {
    const id = readStoredTraderId();
    if (id !== null) {
      try {
        this.#state.trader = await api.trader(id);
      } catch {
        this.#state.trader = null; // Server restarted: the id is stale.
      }
    }
    if (this.#state.trader === null) {
      this.#state.trader = await api.createTrader({ name: 'player' });
      storeTraderId(this.#state.trader.id);
    }
    this.#store.emit('trader', 'book');
  }

  async refreshTrader(): Promise<void> {
    const trader = this.#state.trader;
    if (trader === null) return;
    try {
      this.#state.trader = await api.trader(trader.id);
      this.#store.emit('trader', 'book');
    } catch (e) {
      setOrderStatus(errorMessage(e), true);
    }
  }

  async loadBookAndTape(): Promise<void> {
    const symbol = this.#state.symbol;
    if (symbol === null) return;
    const [book, trades] = await Promise.all([
      api.book(symbol, BOOK_DEPTH),
      api.trades(symbol, TAPE_LIMIT),
    ]);
    if (book.symbol !== this.#state.symbol) return;
    this.#state.book = { bids: book.bids, asks: book.asks };
    this.#state.tape = trades.trades;
    this.#store.emit('book', 'tape');
  }

  selectSymbol(symbol: string): void {
    if (symbol === this.#state.symbol) return;
    this.#state.symbol = symbol;
    this.#state.bars = [];
    this.#state.hover = null;
    this.#state.tape = [];
    this.#state.book = { bids: [], asks: [] };
    this.#store.emit('symbols', 'bars', 'book', 'tape');
    void this.loadBars();
    this.loadBookAndTape().catch((e: unknown) => setStatus(errorMessage(e), true));
  }

  selectInterval(interval: Interval): void {
    this.#state.interval = interval;
    this.#state.bars = [];
    this.#state.hover = null;
    this.#store.emit('bars');
    void this.loadBars();
  }

  setSide(side: 'buy' | 'sell'): void {
    this.#state.side = side;
  }

  setPitch(pitch: number): void {
    this.#state.pitch = Math.min(40, Math.max(2, pitch));
    this.#store.emit('bars');
  }

  setHover(index: number | null): void {
    if (this.#state.hover === index) return;
    this.#state.hover = index;
    this.#store.emit('bars');
  }

  async submitOrder(body: OrderRequest): Promise<void> {
    const symbol = this.#state.symbol;
    if (symbol === null) return;
    try {
      const r = await api.submitOrder(symbol, body);
      const avg = r.avg_price_cents === null ? '' : ` avg ${(r.avg_price_cents / 100).toFixed(2)}`;
      setOrderStatus(`#${r.order_id} ${r.status}: ${r.filled}/${r.qty} filled${avg}`);
      await this.refreshTrader();
      this.loadBookAndTape().catch(() => {
        // The next tick refreshes both anyway.
      });
    } catch (e) {
      setOrderStatus(errorMessage(e), true);
    }
  }

  async cancelOrder(order: OpenOrderDto): Promise<void> {
    const trader = this.#state.trader;
    if (trader === null) return;
    try {
      await api.cancelOrder(order.symbol, order.order_id, trader.id);
      setOrderStatus(`cancelled #${order.order_id}`);
      await this.refreshTrader();
    } catch (e) {
      setOrderStatus(errorMessage(e), true);
    }
  }

  async sendGameEvent(entry: CatalogEntry, magnitude: number): Promise<void> {
    const symbol = this.#state.symbol;
    if (entry.scope === 'company' && symbol === null) return;
    try {
      const r = await api.pushGameEvent({
        kind: entry.kind,
        magnitude,
        source: 'ui',
        ...(entry.scope === 'company' && symbol !== null ? { symbol } : {}),
      });
      setStatus(`accepted #${r.id}: ${r.kind} on ${r.symbols.join(', ')}`);
    } catch (e) {
      setStatus(errorMessage(e), true);
    }
  }

  async sendSimEvent(body: PushEventRequest): Promise<void> {
    const symbol = this.#state.symbol;
    if (symbol === null) return;
    try {
      const r = await api.pushSimEvent(symbol, body);
      setStatus(`accepted #${r.id}: ${r.summary.join(', ')}`);
    } catch (e) {
      setStatus(errorMessage(e), true);
    }
  }

  // --- stream application ---------------------------------------------------

  recordEvent(record: EventRecord): void {
    this.#state.events.unshift(record);
    if (this.#state.events.length > MAX_EVENTS) this.#state.events.pop();
    this.#store.emit('events');
  }

  /**
   * Extend the last bar with a tick, or open a new one. Called only for
   * intervals the server did not report as closed during the step; a closed
   * interval is refetched instead, so the client never disagrees with the
   * server's aggregation.
   */
  applyTick(tick: TickMessage): void {
    const bucket = bucketOf(tick.ts_ms, this.#state.interval);
    const last = this.#state.bars.at(-1);
    if (last !== undefined && last.open_ts === bucket) {
      last.high = Math.max(last.high, tick.price_cents);
      last.low = Math.min(last.low, tick.price_cents);
      last.close = tick.price_cents;
      last.volume += tick.volume;
      last.ticks += 1;
    } else if (last === undefined || bucket > last.open_ts) {
      this.#state.bars.push({
        open_ts: bucket,
        open: tick.price_cents,
        high: tick.price_cents,
        low: tick.price_cents,
        close: tick.price_cents,
        volume: tick.volume,
        ticks: 1,
      });
    }
  }

  pushTrades(trades: TickMessage['trades']): void {
    if (trades.length === 0) return;
    this.#state.tape = trades.concat(this.#state.tape).slice(0, MAX_TAPE);
    this.#store.emit('tape');
  }

  pushLiveFill(fill: FillRecord): void {
    this.#state.liveFills.unshift(fill);
    if (this.#state.liveFills.length > MAX_LIVE_FILLS) this.#state.liveFills.pop();
  }
}
