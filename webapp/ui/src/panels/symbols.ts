/** The symbol rail: one row per ticker, live price and change on the day. */

import type { Actions } from '../actions.js';
import { byId, el, replace } from '../dom.js';
import { fmtPct, fmtPrice, trendClass } from '../format.js';
import type { Store } from '../store.js';
import type { Quote } from '../types.js';

interface Row {
  root: HTMLElement;
  price: HTMLElement;
  change: HTMLElement;
}

export class SymbolList {
  readonly #store: Store;
  readonly #actions: Actions;
  readonly #root = byId('symbols');
  readonly #title = byId('title');
  readonly #subtitle = byId('subtitle');
  readonly #target = byId('target');
  readonly #rows = new Map<string, Row>();

  constructor(store: Store, actions: Actions) {
    this.#store = store;
    this.#actions = actions;
    store.on('symbols', () => this.render());
    store.on('quotes', () => this.refresh());
  }

  /** Rebuild the rows: the listing or the selection changed. */
  render(): void {
    const { quotes, symbol } = this.#store.state;
    this.#rows.clear();
    replace(
      this.#root,
      [...quotes.values()].map((q) => this.#row(q, q.symbol === symbol)),
    );
    const current = this.#store.currentQuote();
    this.#title.textContent = current === null ? '—' : `${current.symbol} · ${current.name}`;
    this.#subtitle.textContent = current?.sector ?? '';
    this.#target.textContent = current?.symbol ?? '—';
  }

  /** Update prices in place — cheap enough to run on every tick. */
  refresh(): void {
    for (const q of this.#store.state.quotes.values()) {
      const row = this.#rows.get(q.symbol);
      if (row === undefined) continue;
      const change = changeOf(q);
      const cls = trendClass(change);
      row.price.textContent = fmtPrice(q.price_cents);
      row.price.className = `p ${cls}`;
      row.change.textContent = change === null ? '' : fmtPct(change);
      row.change.className = `c ${cls}`;
    }
  }

  #row(q: Quote, active: boolean): HTMLElement {
    const change = changeOf(q);
    const cls = trendClass(change);
    const price = el('span', { class: `p ${cls}` }, fmtPrice(q.price_cents));
    const changeEl = el('span', { class: `c ${cls}` }, change === null ? '' : fmtPct(change));
    // A symbol nobody can trade says so where it is chosen.
    const name = q.halted
      ? el('span', { class: 'n' }, el('b', { class: 'stopped' }, 'halted'), ` · ${q.name}`)
      : q.market_open
        ? el('span', { class: 'n' }, `${q.name} · ${q.sector}`)
        : el('span', { class: 'n' }, el('b', { class: 'stopped' }, 'closed'), ` · ${q.name}`);
    const root = el(
      'div',
      { class: active ? 'sym active' : 'sym', 'data-symbol': q.symbol },
      el('span', { class: 't' }, q.symbol),
      price,
      name,
      changeEl,
    );
    root.addEventListener('click', () => this.#actions.selectSymbol(q.symbol));
    this.#rows.set(q.symbol, { root, price, change: changeEl });
    return root;
  }
}

/**
 * Percent change on the day. The server sends `change_pct` with the quote,
 * but a streamed tick only carries the price, so recompute from the previous
 * close to keep the two paths consistent.
 */
function changeOf(q: Quote): number | null {
  if (q.prev_close_cents === null || q.prev_close_cents === 0) return q.change_pct;
  return (q.price_cents / q.prev_close_cents - 1) * 100;
}
