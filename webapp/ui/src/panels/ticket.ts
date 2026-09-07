/**
 * The order ticket: side, quantity, market or limit, time in force.
 *
 * A sell is bounded by what the trader actually holds — the server refuses
 * anything more, so the ticket shows the free share count and stops the order
 * before it is sent.
 */

import type { Actions } from '../actions.js';
import { byId, query, queryAll } from '../dom.js';
import { fmtVol } from '../format.js';
import { setOrderStatus } from '../status.js';
import type { Store } from '../store.js';
import type { OrderRequest, Side, TimeInForce } from '../types.js';

const TIFS: readonly TimeInForce[] = ['gtc', 'ioc', 'fok'];

function isTif(value: string): value is TimeInForce {
  return (TIFS as readonly string[]).includes(value);
}

export class TicketPanel {
  readonly #store: Store;
  readonly #actions: Actions;
  readonly #form = byId('ticket', HTMLFormElement);
  readonly #symbol = byId('ticket-sym');
  readonly #qty = query(byId('ticket'), '[name=qty]', HTMLInputElement);
  readonly #kind = query(byId('ticket'), '[name=type]', HTMLSelectElement);
  readonly #price = query(byId('ticket'), '[name=price]', HTMLInputElement);
  readonly #tif = query(byId('ticket'), '[name=tif]', HTMLSelectElement);
  readonly #submit = query(byId('ticket'), '.submit', HTMLButtonElement);
  readonly #holding = byId('ticket-holding');

  constructor(store: Store, actions: Actions) {
    this.#store = store;
    this.#actions = actions;

    for (const button of queryAll<HTMLButtonElement>(this.#form, '.side button')) {
      button.addEventListener('click', () => {
        const side = button.dataset['side'];
        if (side !== 'buy' && side !== 'sell') return;
        this.#setSide(side);
      });
    }
    for (const input of [this.#qty, this.#kind, this.#price]) {
      input.addEventListener('input', () => this.updateLabel());
    }
    this.#form.addEventListener('submit', (ev) => {
      ev.preventDefault();
      void this.#submitOrder();
    });
    store.on('symbols', () => this.#onSymbolChange());
    // A fill changes what is sellable, but must not clear a typed price.
    store.on('trader', () => this.#renderHolding());
  }

  /** Shares of the selected symbol this trader may still sell. */
  #freeShares(): number {
    const { trader, symbol } = this.#store.state;
    if (trader === null || symbol === null) return 0;
    return trader.positions.find((p) => p.symbol === symbol)?.free_shares ?? 0;
  }

  /** What the trader holds of the selected symbol, and the symbol's float. */
  #renderHolding(): void {
    const { trader, symbol } = this.#store.state;
    if (trader === null || symbol === null) {
      this.#holding.textContent = '';
      return;
    }
    const position = trader.positions.find((p) => p.symbol === symbol);
    const qty = position?.qty ?? 0;
    const free = position?.free_shares ?? 0;
    const parts = [`own ${qty}`, `sellable ${free}`];
    const quote = this.#store.currentQuote();
    if (quote !== null) parts.push(`of ${fmtVol(quote.shares_outstanding)} shares`);
    this.#holding.textContent = `${symbol}: ${parts.join(' · ')}`;
  }

  /** Pre-fill a limit at a price clicked in the ladder. */
  setLimitPrice(priceCents: number): void {
    this.#kind.value = 'limit';
    this.#price.disabled = false;
    this.#price.value = (priceCents / 100).toFixed(2);
    this.updateLabel();
  }

  /** Why the market will take no order for the selected symbol, if it won't. */
  #stopped(): 'halted' | 'closed' | null {
    const quote = this.#store.currentQuote();
    if (quote === null) return null;
    if (quote.halted) return 'halted';
    return quote.market_open ? null : 'closed';
  }

  updateLabel(): void {
    this.#renderHolding();
    const limit = this.#kind.value === 'limit';
    this.#price.disabled = !limit;
    const side = this.#store.state.side;
    const qty = this.#qty.value === '' ? '?' : this.#qty.value;
    const how = limit ? (this.#price.value === '' ? 'limit' : `@ ${this.#price.value}`) : 'at market';
    // A halted or closed market takes no orders; the server refuses them too.
    const stopped = this.#stopped();
    this.#submit.disabled = stopped !== null;
    this.#submit.textContent =
      stopped === null
        ? `${side === 'buy' ? 'Buy' : 'Sell'} ${qty} ${how}`
        : stopped === 'halted'
          ? 'Trading halted'
          : 'Market closed';
    this.#submit.className = `submit full ${stopped === null ? side : 'stopped'}`;
  }

  #onSymbolChange(): void {
    this.#symbol.textContent = this.#store.state.symbol ?? '';
    this.#price.value = '';
    this.updateLabel();
  }

  #setSide(side: Side): void {
    this.#actions.setSide(side);
    for (const button of queryAll<HTMLButtonElement>(this.#form, '.side button')) {
      button.classList.toggle('active', button.dataset['side'] === side);
    }
    this.updateLabel();
  }

  async #submitOrder(): Promise<void> {
    const trader = this.#store.state.trader;
    if (trader === null) return;
    const stopped = this.#stopped();
    if (stopped !== null) {
      setOrderStatus(
        stopped === 'halted' ? 'trading in this symbol is halted' : 'the market is closed',
        true,
      );
      return;
    }
    const qty = Number.parseInt(this.#qty.value, 10);
    if (!Number.isSafeInteger(qty) || qty <= 0) return;
    const side = this.#store.state.side;
    // Nothing may be sold that is not owned; the server checks this too.
    if (side === 'sell' && qty > this.#freeShares()) {
      setOrderStatus(`insufficient shares: ${this.#freeShares()} free to sell`, true);
      return;
    }
    const tif = isTif(this.#tif.value) ? this.#tif.value : 'gtc';
    const base = { trader_id: trader.id, side, qty, tif };

    let order: OrderRequest;
    if (this.#kind.value === 'limit') {
      const price = Number.parseFloat(this.#price.value);
      if (!Number.isFinite(price)) return;
      order = { ...base, type: 'limit', price_cents: Math.round(price * 100) };
    } else {
      order = { ...base, type: 'market' };
    }
    await this.#actions.submitOrder(order);
  }
}
