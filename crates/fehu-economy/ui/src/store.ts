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
  FlowDto,
  Health,
  HoldingDto,
  Interval,
  Job,
  Modifier,
  OverviewDto,
  PortfolioDto,
  Quote,
  Recipe,
  Reconciliation,
  Side,
  SupplyDto,
  SymbolEffects,
  TradeDto,
  WalletDto,
} from './types.js';

/**
 * One reading of the economy, kept by the page.
 *
 * The server counts, it does not remember: supply and flows are running
 * totals with no window and no history behind them, deliberately. So the
 * dashboard keeps its own readings and takes the differences — which is all a
 * rate ever was — and a reload starts the record again.
 */
export interface OpsSample {
  /** Wall-clock milliseconds the reading was taken at. */
  at: number;
  supply: SupplyDto;
  flows: FlowDto[];
}

/** The operator's dashboard: what it has read, and what it is showing. */
export interface OpsState {
  open: boolean;
  overview: OverviewDto | null;
  health: Health | null;
  /** The last audit asked for. Not polled: it snapshots the whole market. */
  reconciliation: Reconciliation | null;
  /** Readings oldest first, capped at {@link MAX_OPS_SAMPLES}. */
  history: OpsSample[];
  /** Wall milliseconds between polls while the dashboard is open. */
  intervalMs: number;
  /** What the last request said, if it failed. */
  error: string | null;
  /** What the last command did, for the line under the forms. */
  note: string | null;
}

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
  /** The operator's dashboard. Only polled while it is open. */
  ops: OpsState;
  /** Latest simulated time seen, from any source. */
  simNow: number | null;
  /** Simulated seconds per wall second. */
  timeScale: number;
  connection: ConnectionState;
}

export const MAX_EVENTS = 200;
/**
 * Readings the dashboard keeps. At the default five-second poll that is a
 * little over half an hour of history, which is as far back as a chart the
 * width of a panel can usefully draw.
 */
export const MAX_OPS_SAMPLES = 400;
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
  /** The operator's dashboard read something, or changed something. */
  | 'ops'
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
    ops: {
      open: false,
      overview: null,
      health: null,
      reconciliation: null,
      history: [],
      intervalMs: 5_000,
      error: null,
      note: null,
    },
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
