/**
 * Everything that writes to the store: fetches, stream application and the
 * user-initiated commands. Panels call these; nothing else mutates state.
 */

import { api, errorMessage, ops, setAdminKey, setApiKey } from './api.js';
import { fmtPrice } from './format.js';
import { bucketOf } from './intervals.js';
import { setOrderStatus, setStatus } from './status.js';
import { MAX_EVENTS, MAX_LIVE_FILLS, MAX_OPS_SAMPLES, MAX_TAPE, type Store } from './store.js';
import type {
  AccountStatus,
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
const KEY_STORAGE_KEY = 'fehu.api_key';
const ADMIN_KEY_STORAGE_KEY = 'fehu.admin_key';

/** `localStorage` is unavailable in some privacy modes; degrade, don't crash. */
function readStored(key: string): string | null {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}

function storeIdentity(id: number, apiKey: string | null): void {
  try {
    localStorage.setItem(TRADER_STORAGE_KEY, String(id));
    if (apiKey !== null) localStorage.setItem(KEY_STORAGE_KEY, apiKey);
  } catch {
    // Not fatal: the session just will not survive a reload.
  }
}

function forgetIdentity(): void {
  try {
    localStorage.removeItem(TRADER_STORAGE_KEY);
    localStorage.removeItem(KEY_STORAGE_KEY);
  } catch {
    // Nothing was stored in the first place.
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

  /**
   * Reuse the trader from a previous visit, or sign a new one up. The API key
   * that came with the sign-up is what proves the account is ours; without a
   * stored one — or with a stale one the server no longer knows — there is
   * nothing to come back to, so we start again.
   */
  async loadTrader(): Promise<void> {
    const stored = Number.parseInt(readStored(TRADER_STORAGE_KEY) ?? '', 10);
    const key = readStored(KEY_STORAGE_KEY);
    if (Number.isSafeInteger(stored) && key !== null) {
      setApiKey(key);
      try {
        this.#state.trader = await api.trader(stored);
      } catch {
        // Server restarted, or the key no longer opens this trader.
        this.#state.trader = null;
        setApiKey(null);
        forgetIdentity();
      }
    }
    if (this.#state.trader === null) {
      const trader = await api.createTrader({ name: 'player' });
      setApiKey(trader.api_key);
      storeIdentity(trader.id, trader.api_key);
      this.#state.trader = trader;
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

  /**
   * Add money to the player's account. The amount arrives here in cents —
   * the API takes nothing else — and the account is validated server-side,
   * so a refused deposit shows up as a status line, not as a wrong balance.
   */
  async deposit(amountCents: number): Promise<void> {
    const trader = this.#state.trader;
    if (trader === null) return;
    if (!Number.isSafeInteger(amountCents) || amountCents <= 0) {
      setOrderStatus('deposit: amount must be a positive number of cents', true);
      return;
    }
    try {
      this.#state.trader = await api.deposit(trader.id, { amount_cents: amountCents });
      this.#store.emit('trader');
      setOrderStatus(`deposited ${fmtPrice(amountCents)}`);
    } catch (e) {
      setOrderStatus(errorMessage(e), true);
    }
  }

  /**
   * The economy half of a player: their wallet, their units of the world's
   * goods, the recipes they can run and the jobs they have running.
   *
   * One call after anything that could have moved any of them — a job
   * started, a job delivered on the stream, a fill — because they move
   * together: smelting takes ore, cents and time and gives back an ingot.
   */
  async loadEconomy(): Promise<void> {
    const trader = this.#state.trader;
    if (trader === null) return;
    try {
      const [inventory, recipes, jobs, world] = await Promise.all([
        api.inventory(trader.id),
        api.recipes(),
        api.jobs(),
        api.world(),
      ]);
      this.#state.inventory = inventory.inventory;
      this.#state.recipes = recipes.recipes;
      this.#state.jobs = jobs.jobs.slice().reverse();
      this.#state.effects = world.symbols;
      this.#state.modifiers = world.modifiers;
      // The account is what knows which wallet is this player's; the wallet
      // is what the ledger actually holds the money in.
      const account = await api.account(trader.account_id);
      this.#state.wallet = await api.wallet(account.wallet_id);
      this.#store.emit('economy');
    } catch (e) {
      setStatus(errorMessage(e), true);
    }
  }

  /**
   * Run a recipe. The inputs and the cost go now; the outputs arrive on the
   * engine step that reaches the job's due instant, which is what the panel
   * counts down to.
   */
  async startJob(recipe: string): Promise<void> {
    const trader = this.#state.trader;
    if (trader === null) return;
    try {
      const job = await api.startJob(trader.id, recipe);
      const made = job.outputs.map((o) => `${o.qty} ${o.symbol}`).join(', ');
      setOrderStatus(`job #${job.id}: ${job.recipe} → ${made}`);
      await Promise.all([this.refreshTrader(), this.loadEconomy()]);
    } catch (e) {
      setOrderStatus(errorMessage(e), true);
    }
  }

  /** Stop a job before it is due. The inputs stay in the crucible. */
  async cancelJob(jobId: number): Promise<void> {
    try {
      const job = await api.cancelJob(jobId);
      setOrderStatus(
        job.refunded_cents > 0
          ? `job #${job.id} cancelled, ${fmtPrice(job.refunded_cents)} back`
          : `job #${job.id} cancelled`,
      );
      await Promise.all([this.refreshTrader(), this.loadEconomy()]);
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

  // --- the operator's dashboard ---------------------------------------------

  /**
   * Take up the operator's key from the last session, if there was one.
   *
   * A server with no admin key set admits the operator routes to anybody —
   * the single-player default — so the absence of one is not an error and
   * the dashboard opens either way.
   */
  restoreAdminKey(): void {
    setAdminKey(readStored(ADMIN_KEY_STORAGE_KEY));
  }

  /** Use `key` for the operator routes from here on, and remember it. */
  useAdminKey(key: string | null): void {
    setAdminKey(key);
    try {
      if (key === null || key === '') localStorage.removeItem(ADMIN_KEY_STORAGE_KEY);
      else localStorage.setItem(ADMIN_KEY_STORAGE_KEY, key);
    } catch {
      // Not fatal: the key just will not survive a reload.
    }
    this.#store.emit('ops');
  }

  /** Open or close the dashboard. The panel starts and stops polling. */
  setOpsOpen(open: boolean): void {
    if (this.#state.ops.open === open) return;
    this.#state.ops.open = open;
    this.#store.emit('ops');
  }

  /**
   * One reading: the whole economy, and the server's own counters.
   *
   * Both in one round trip each, and the pair kept as a sample so the panel
   * can draw a rate. A failure leaves the last good reading on screen with
   * the error beside it — a dashboard that blanked itself on a dropped
   * request would be worse than one that says it is stale.
   */
  async loadOps(): Promise<void> {
    try {
      const [overview, health] = await Promise.all([ops.overview(), ops.health()]);
      this.#state.ops.overview = overview;
      this.#state.ops.health = health;
      this.#state.ops.error = null;
      this.#state.ops.history.push({
        at: Date.now(),
        supply: overview.supply,
        flows: overview.flows,
      });
      if (this.#state.ops.history.length > MAX_OPS_SAMPLES) this.#state.ops.history.shift();
    } catch (e) {
      this.#state.ops.error = errorMessage(e);
    }
    this.#store.emit('ops');
  }

  /** Run the audit. Asked for, never polled: it snapshots the whole market. */
  async runAudit(): Promise<void> {
    try {
      this.#state.ops.reconciliation = await ops.reconcile();
      this.#state.ops.error = null;
    } catch (e) {
      this.#state.ops.error = errorMessage(e);
    }
    this.#store.emit('ops');
  }

  /**
   * Run one operator command, then read the world back.
   *
   * Every mutation goes through here so that all of them report the same way
   * and none of them leaves the dashboard showing the world as it was before
   * the change.
   */
  async #command(what: string, run: () => Promise<unknown>): Promise<void> {
    try {
      await run();
      this.#state.ops.note = what;
      this.#state.ops.error = null;
    } catch (e) {
      this.#state.ops.note = null;
      this.#state.ops.error = errorMessage(e);
      this.#store.emit('ops');
      return;
    }
    await this.loadOps();
  }

  /** Create currency into an account: the one way supply goes up. */
  async mint(accountId: number, amountCents: number): Promise<void> {
    await this.#command(`minted ${fmtPrice(amountCents)} into account #${accountId}`, () =>
      ops.mint(accountId, amountCents),
    );
  }

  /** Destroy currency out of one: the one way it goes down. */
  async burn(accountId: number, amountCents: number): Promise<void> {
    await this.#command(`burned ${fmtPrice(amountCents)} out of account #${accountId}`, () =>
      ops.burn(accountId, amountCents),
    );
  }

  async setAccountStatus(accountId: number, status: AccountStatus): Promise<void> {
    await this.#command(`account #${accountId} is ${status}`, () =>
      ops.setAccountStatus(accountId, status),
    );
  }

  async createBudget(name: string, cashCents: number): Promise<void> {
    await this.#command(`opened budget ${name} with ${fmtPrice(cashCents)}`, () =>
      ops.createBudget(name, cashCents),
    );
  }

  async fundBudget(wallet: number, amountCents: number): Promise<void> {
    await this.#command(`funded budget #${wallet} with ${fmtPrice(amountCents)}`, () =>
      ops.fundBudget(wallet, amountCents),
    );
  }

  async setRewardRule(id: string, budget: number, amountCents: number): Promise<void> {
    await this.#command(`${id} pays ${fmtPrice(amountCents)} from budget #${budget}`, () =>
      ops.setRewardRule(id, budget, amountCents),
    );
  }

  async removeRewardRule(id: string): Promise<void> {
    await this.#command(`rule ${id} is gone`, () => ops.removeRewardRule(id));
  }

  async setNpcActive(traderId: number, active: boolean): Promise<void> {
    await this.#command(`merchant #${traderId} is ${active ? 'quoting' : 'idle'}`, () =>
      ops.setNpcActive(traderId, active),
    );
  }

  async setSymbolHalted(symbol: string, halted: boolean): Promise<void> {
    await this.#command(`${symbol} is ${halted ? 'halted' : 'trading'}`, () =>
      halted ? ops.halt(symbol) : ops.resume(symbol),
    );
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
