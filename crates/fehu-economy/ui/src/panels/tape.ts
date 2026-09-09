/** The tape: recent prints, newest first, with the trader's own highlighted. */

import { byId, el, replace, td } from '../dom.js';
import { fmtPrice, fmtTs, fmtVol } from '../format.js';
import type { Store } from '../store.js';
import type { TradeDto } from '../types.js';

const MAX_ROWS = 40;

export class TapePanel {
  readonly #store: Store;
  readonly #body = byId('tape');

  constructor(store: Store) {
    this.#store = store;
    store.on(['tape', 'trader'], () => this.render());
  }

  render(): void {
    const me = this.#store.state.trader?.id ?? null;
    replace(
      this.#body,
      this.#store.state.tape.slice(0, MAX_ROWS).map((t) => row(t, me)),
    );
  }
}

function row(trade: TradeDto, me: number | null): HTMLTableRowElement {
  const mine = me !== null && (trade.taker_trader === me || trade.maker_trader === me);
  const traded = trade.taker_trader !== null || trade.maker_trader !== null;
  const who = traded ? (mine ? 'you' : 'trader') : trade.hidden ? 'hidden' : '';
  const side = trade.taker_side === 'buy' ? 'buy' : 'sell';
  return el(
    'tr',
    { class: mine ? `${side} mine` : side },
    td('q', fmtTs(trade.ts_ms)),
    td('p', fmtPrice(trade.price_cents)),
    td('q', fmtVol(trade.qty)),
    td('q', who),
  );
}
