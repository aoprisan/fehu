/**
 * The SSE client. One `EventSource` carries every symbol's ticks, the
 * accepted events and this trader's fills; each connection opens with a
 * `hello` saying which sequence number it joins at.
 *
 * Every message is numbered, so a reconnect asks to resume with `?since=`
 * and the server replays what was missed out of its bounded buffer. When it
 * cannot reach back that far it says so (`gap`), and then — and only then —
 * the snapshots are reloaded, because current state is all that is left.
 */

import { currentApiKey } from './api.js';
import type { Actions } from './actions.js';
import { setOrderStatus } from './status.js';
import type { Store } from './store.js';
import type { StreamMessage, TickMessage } from './types.js';

/** How often, in simulated seconds, a tick may trigger a portfolio refresh. */
const MARK_REFRESH_SECS = 5;

/** How long to wait before reopening a stream that dropped. */
const RECONNECT_MS = 1_000;

export class MarketStream {
  readonly #store: Store;
  readonly #actions: Actions;
  #source: EventSource | null = null;
  #lastMarkRefresh = 0;
  /** The last sequence number seen, resumed from on the next connection. */
  #seq: number | null = null;
  /** Set until a `hello` says whether this connection missed anything. */
  #resuming = false;

  constructor(store: Store, actions: Actions) {
    this.#store = store;
    this.#actions = actions;
  }

  connect(): void {
    // Ticks and events are public, but a fill belongs to the trader that made
    // it, and `EventSource` cannot set headers — hence the key in the query.
    const key = currentApiKey();
    const params = new URLSearchParams();
    if (key !== null) params.set('api_key', key);
    // `EventSource` reconnects to the same URL, so resuming means opening a
    // new one from where this client got to.
    if (this.#seq !== null) params.set('since', String(this.#seq));
    this.#resuming = this.#seq !== null;
    const query = params.toString();
    const es = new EventSource(query === '' ? '/api/stream' : `/api/stream?${query}`);
    this.#source = es;
    es.onopen = () => {
      this.#setConnection('live');
      // A first connection has no history to replay, so it loads everything.
      // A resumed one waits for the `hello` to say whether anything was lost.
      if (!this.#resuming) this.#reload();
    };
    es.onerror = () => {
      this.#setConnection('reconnecting');
      // Reopen at our own sequence rather than letting `EventSource` rejoin
      // the live feed and silently skip whatever happened in between.
      if (this.#seq !== null) {
        es.close();
        if (this.#source === es) window.setTimeout(() => this.connect(), RECONNECT_MS);
      }
    };
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

  /** Take every snapshot again: the only recovery from a lost message. */
  #reload(): void {
    void Promise.allSettled([
      this.#actions.loadSymbols(),
      this.#actions.loadBars(),
      this.#actions.loadEvents(),
      this.#actions.loadBookAndTape(),
      this.#actions.refreshTrader(),
    ]);
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
    if (typeof message.seq === 'number') this.#seq = message.seq;
    switch (message.type) {
      case 'hello': {
        state.timeScale = message.time_scale;
        state.simNow = message.sim_now_ms;
        for (const q of message.quotes) state.quotes.set(q.symbol, q);
        this.#store.emit('symbols', 'clock');
        // Resumed, but the server could not reach back far enough: what was
        // missed is gone, so the snapshots are the only truth left.
        if (this.#resuming && message.gap) this.#reload();
        this.#resuming = false;
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
      case 'listed': {
        state.quotes.set(message.quote.symbol, message.quote);
        this.#store.emit('symbols');
        setOrderStatus(`${message.quote.symbol} listed`);
        break;
      }
      case 'delisted': {
        state.quotes.delete(message.symbol);
        // The tab for a symbol that no longer exists cannot be left selected:
        // every panel behind it would ask the server for a symbol it has just
        // been told is gone.
        if (state.symbol === message.symbol) {
          const next = state.quotes.keys().next();
          state.symbol = next.done === true ? null : next.value;
          void Promise.allSettled([this.#actions.loadBars(), this.#actions.loadBookAndTape()]);
        }
        this.#store.emit('symbols', 'bars', 'book', 'tape');
        void this.#actions.refreshTrader();
        setOrderStatus(
          `${message.symbol} delisted at ${(message.cents_per_share / 100).toFixed(2)} a share`,
          true,
        );
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
