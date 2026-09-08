/**
 * The single mutable application state, plus a topic-keyed subscription so a
 * tick that only moves the book does not re-render the account panel.
 *
 * Panels read `store.state` and subscribe to the topics they draw from;
 * `actions.ts` is the only place that writes.
 */

import type {
  BookDto,
  CatalogEntry,
  Candle,
  EventRecord,
  FillRecord,
  HoldingDto,
  Interval,
  Job,
  Modifier,
  PortfolioDto,
  Quote,
  Recipe,
  Side,
  SymbolEffects,
  TradeDto,
  WalletDto,
} from './types.js';

export type ConnectionState = 'connecting' | 'live' | 'reconnecting';

export interface AppState {
  /** Selected ticker; `null` only before the first `/api/symbols` response. */
  symbol: string | null;
  interval: Interval;
  /** Bars for `symbol` at `interval`, oldest first. */
  bars: Candle[];
  barsLoading: boolean;
  /** Latest quote per ticker, in listing order. */
  quotes: Map<string, Quote>;
  catalog: CatalogEntry[];
  /** Newest first, capped at {@link MAX_EVENTS}. */
  events: EventRecord[];
  book: BookDto;
  /** Newest first. */
  tape: TradeDto[];
  trader: PortfolioDto | null;
  /** The player's wallet, as the ledger holds it. */
  wallet: WalletDto | null;
  /** The player's units of the world's goods. */
  inventory: HoldingDto[];
  /** What the world knows how to make. */
  recipes: Recipe[];
  /** The player's jobs, newest first. */
  jobs: Job[];
  /** What events are doing to production and demand, per symbol. */
  effects: SymbolEffects[];
  /** The modifiers behind those numbers, newest last. */
  modifiers: Modifier[];
  /** Fills seen on the stream since load, newest first. */
  liveFills: FillRecord[];
  /** Side the order ticket is set to. */
  side: Side;
  /** Horizontal pixels per candle. */
  pitch: number;
  /** Index into the drawn window of the hovered bar. */
  hover: number | null;
  /** Latest simulated time seen, from any source. */
  simNow: number | null;
  /** Simulated seconds per wall second. */
  timeScale: number;
  connection: ConnectionState;
}

export const MAX_EVENTS = 200;
export const MAX_TAPE = 60;
export const MAX_LIVE_FILLS = 200;

/** What a panel can subscribe to. */
export type Topic =
  /** The symbol list or the selection changed: rebuild the rows. */
  | 'symbols'
  /** Prices moved: refresh the existing rows in place. */
  | 'quotes'
  | 'bars'
  | 'book'
  | 'tape'
  | 'trader'
  /** The wallet, the inventory, the recipes or the jobs changed. */
  | 'economy'
  | 'events'
  | 'catalog'
  | 'clock'
  | 'connection';

type Listener = () => void;

export class Store {
  readonly state: AppState = {
    symbol: null,
    interval: 'M1',
    bars: [],
    barsLoading: false,
    quotes: new Map(),
    catalog: [],
    events: [],
    book: { bids: [], asks: [] },
    tape: [],
    trader: null,
    wallet: null,
    inventory: [],
    recipes: [],
    jobs: [],
    effects: [],
    modifiers: [],
    liveFills: [],
    side: 'buy',
    pitch: 9,
    hover: null,
    simNow: null,
    timeScale: 1,
    connection: 'connecting',
  };

  readonly #listeners = new Map<Topic, Set<Listener>>();

  /** Subscribe to one or more topics. Returns an unsubscribe function. */
  on(topics: Topic | Topic[], fn: Listener): () => void {
    const list = Array.isArray(topics) ? topics : [topics];
    for (const t of list) {
      let set = this.#listeners.get(t);
      if (set === undefined) {
        set = new Set();
        this.#listeners.set(t, set);
      }
      set.add(fn);
    }
    return () => {
      for (const t of list) this.#listeners.get(t)?.delete(fn);
    };
  }

  /** Notify subscribers. Each listener runs at most once per call. */
  emit(...topics: Topic[]): void {
    const seen = new Set<Listener>();
    for (const t of topics) {
      const set = this.#listeners.get(t);
      if (set === undefined) continue;
      for (const fn of set) {
        if (seen.has(fn)) continue;
        seen.add(fn);
        fn();
      }
    }
  }

  /** The selected symbol's latest quote, if it has arrived. */
  currentQuote(): Quote | null {
    const { symbol } = this.state;
    return symbol === null ? null : (this.state.quotes.get(symbol) ?? null);
  }
}
