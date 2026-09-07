/**
 * The SSE client. One `EventSource` carries every symbol's ticks, the
 * accepted events and this trader's fills; `EventSource` reconnects on its
 * own, and each reconnect replays a `hello`.
 */

import { currentApiKey } from './api.js';
import type { Actions } from './actions.js';
import { setOrderStatus } from './status.js';
import type { Store } from './store.js';
import type { StreamMessage, TickMessage } from './types.js';

/** How often, in simulated seconds, a tick may trigger a portfolio refresh. */
const MARK_REFRESH_SECS = 5;

export class MarketStream {
  readonly #store: Store;
  readonly #actions: Actions;
  #source: EventSource | null = null;
  #lastMarkRefresh = 0;

  constructor(store: Store, actions: Actions) {
    this.#store = store;
    this.#actions = actions;
  }

  connect(): void {
    // Ticks and events are public, but a fill belongs to the trader that made
    // it, and `EventSource` cannot set headers — hence the key in the query.
    const key = currentApiKey();
    const url = key === null ? '/api/stream' : `/api/stream?api_key=${encodeURIComponent(key)}`;
    const es = new EventSource(url);
    this.#source = es;
    es.onopen = () => {
      this.#setConnection('live');
      void this.#actions.loadBars();
    };
    es.onerror = () => this.#setConnection('reconnecting');
    es.onmessage = (ev: MessageEvent<string>) => {
      let message: StreamMessage;
      try {
        message = JSON.parse(ev.data) as StreamMessage;
      } catch {
        return; // A truncated frame; the next one will be whole.
      }
      this.#handle(message);
    };
  }

  close(): void {
    this.#source?.close();
    this.#source = null;
  }

  #setConnection(state: 'live' | 'reconnecting'): void {
    this.#store.state.connection = state;
    this.#store.emit('connection');
  }

  #handle(message: StreamMessage): void {
    const state = this.#store.state;
    switch (message.type) {
      case 'hello': {
        state.timeScale = message.time_scale;
        state.simNow = message.sim_now_ms;
        for (const q of message.quotes) state.quotes.set(q.symbol, q);
        this.#store.emit('symbols', 'clock');
        break;
      }
      case 'tick': {
        this.#onTick(message);
        break;
      }
      case 'fill': {
        const trader = state.trader;
        if (trader === null || message.trader_id !== trader.id) break;
        const fill = message.fill;
        this.#actions.pushLiveFill(fill);
        setOrderStatus(
          `fill: ${fill.side} ${fill.qty} ${fill.symbol} @ ` +
            `${(fill.price_cents / 100).toFixed(2)} (${fill.liquidity})`,
        );
        void this.#actions.refreshTrader();
        if (fill.symbol === state.symbol) this.#store.emit('bars');
        break;
      }
      case 'status': {
        const quote = state.quotes.get(message.symbol);
        if (quote !== undefined) {
          quote.halted = message.halted;
          quote.market_open = message.market_open;
          this.#store.emit('symbols');
        }
        if (message.symbol === state.symbol) {
          setOrderStatus(
            message.tradable
              ? `${message.symbol}: trading resumed`
              : `${message.symbol}: ${message.halted ? 'trading halted' : 'market closed'}`,
            !message.tradable,
          );
        }
        break;
      }
      case 'event': {
        const { type: _tag, ...record } = message;
        this.#actions.recordEvent(record);
        break;
      }
    }
    this.#store.emit('clock');
  }

  #onTick(tick: TickMessage): void {
    const state = this.#store.state;
    const quote = state.quotes.get(tick.symbol);
    if (quote !== undefined) {
      quote.price_cents = tick.price_cents;
      quote.ts_ms = tick.ts_ms;
      quote.bid_cents = tick.bid_cents;
      quote.ask_cents = tick.ask_cents;
      this.#store.emit('quotes');
    }
    if (tick.ts_ms > (state.simNow ?? 0)) state.simNow = tick.ts_ms;

    if (tick.symbol === state.symbol) {
      if (tick.closed.includes(state.interval)) {
        // A bar closed during the step: take the server's aggregation.
        void this.#actions.loadBars();
      } else {
        this.#actions.applyTick(tick);
      }
      state.book = tick.book;
      this.#store.emit('book', 'bars');
      this.#actions.pushTrades(tick.trades);
    }

    // Marks move with every tick, so re-price the portfolio periodically
    // rather than on each one.
    const holds = state.trader?.positions.some((p) => p.symbol === tick.symbol) ?? false;
    const secs = Math.floor(tick.ts_ms / 1000);
    if (holds && secs - this.#lastMarkRefresh >= MARK_REFRESH_SECS) {
      this.#lastMarkRefresh = secs;
      void this.#actions.refreshTrader();
    }
  }
}
