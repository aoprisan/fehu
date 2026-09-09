/** The depth ladder: asks above, bids below, the trader's own levels marked. */

import { byId, el, replace, td } from '../dom.js';
import { fmtPrice, fmtVol } from '../format.js';
import type { Store } from '../store.js';
import type { Level } from '../types.js';

export interface BookCallbacks {
  /** Clicking a level pre-fills the ticket with a limit at that price. */
  onPriceClick(priceCents: number): void;
}

export class BookPanel {
  readonly #store: Store;
  readonly #callbacks: BookCallbacks;
  readonly #body = byId('ladder');
  readonly #symbol = byId('book-sym');

  constructor(store: Store, callbacks: BookCallbacks) {
    this.#store = store;
    this.#callbacks = callbacks;
    // The "mine" marks come from the portfolio, so redraw when it changes.
    store.on(['book', 'trader', 'symbols'], () => this.render());
  }

  render(): void {
    const { book, symbol } = this.#store.state;
    this.#symbol.textContent = symbol ?? '';
    const { bids, asks } = book;
    const peak = Math.max(1, ...bids.map((l) => l.qty), ...asks.map((l) => l.qty));
    const mine = this.#restingLevels();

    const rows = [
      ...[...asks].reverse().map((l) => this.#row(l, 'ask', peak, mine)),
      this.#midRow(bids[0], asks[0]),
      ...bids.map((l) => this.#row(l, 'bid', peak, mine)),
    ];
    replace(this.#body, rows);
  }

  /** `"buy:12345"` keys for every price this trader is resting at. */
  #restingLevels(): Set<string> {
    const { trader, symbol } = this.#store.state;
    const keys = new Set<string>();
    if (trader === null || symbol === null) return keys;
    for (const o of trader.open_orders) {
      if (o.symbol === symbol) keys.add(`${o.side}:${o.price_cents}`);
    }
    return keys;
  }

  #row(level: Level, side: 'ask' | 'bid', peak: number, mine: Set<string>): HTMLTableRowElement {
    const key = `${side === 'ask' ? 'sell' : 'buy'}:${level.price_cents}`;
    const bar = el('i', { style: `width:${((level.qty / peak) * 100).toFixed(0)}%` });
    const row = el(
      'tr',
      { class: mine.has(key) ? `${side} mine` : side },
      td('p', fmtPrice(level.price_cents)),
      td('q', fmtVol(level.qty)),
      td('bar', bar),
    );
    row.addEventListener('click', () => this.#callbacks.onPriceClick(level.price_cents));
    return row;
  }

  #midRow(bestBid: Level | undefined, bestAsk: Level | undefined): HTMLTableRowElement {
    let text = '—';
    if (bestBid !== undefined && bestAsk !== undefined) {
      const spread = bestAsk.price_cents - bestBid.price_cents;
      const bp = ((spread / bestBid.price_cents) * 1e4).toFixed(1);
      text = `spread ${fmtPrice(spread)} (${bp} bp)`;
    }
    return el('tr', { class: 'mid' }, el('td', { colspan: 3 }, text));
  }
}
