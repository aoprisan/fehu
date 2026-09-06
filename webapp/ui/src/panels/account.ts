/** Cash, positions and resting orders, plus the equity readout in the header. */

import type { Actions } from '../actions.js';
import { byId, el, replace, td } from '../dom.js';
import { fmtPrice, fmtSignedPrice, trendClass } from '../format.js';
import type { Store } from '../store.js';
import type { OpenOrderDto, PositionDto } from '../types.js';

export class AccountPanel {
  readonly #store: Store;
  readonly #actions: Actions;
  readonly #header = byId('acct-hdr');
  readonly #summary = byId('acct');
  readonly #positions = byId('positions');
  readonly #orders = byId('orders');

  constructor(store: Store, actions: Actions) {
    this.#store = store;
    this.#actions = actions;
    store.on('trader', () => this.render());
  }

  render(): void {
    const trader = this.#store.state.trader;
    if (trader === null) return;

    const pnl = trader.realised_pnl_cents + trader.unrealised_pnl_cents;
    replace(this.#header, [
      `${trader.name} #${trader.id} · equity `,
      el('b', {}, fmtPrice(trader.equity_cents)),
      ' · P&L ',
      el('b', { class: trendClass(pnl) }, fmtSignedPrice(pnl)),
    ]);

    replace(this.#summary, [
      stat('cash', fmtPrice(trader.cash_cents)),
      stat('free', fmtPrice(trader.free_cash_cents)),
      stat('realised', fmtPrice(trader.realised_pnl_cents), trendClass(trader.realised_pnl_cents)),
      stat(
        'unrealised',
        fmtPrice(trader.unrealised_pnl_cents),
        trendClass(trader.unrealised_pnl_cents),
      ),
    ]);

    replace(
      this.#positions,
      trader.positions.map((p) => this.#positionRow(p)),
    );
    replace(
      this.#orders,
      trader.open_orders.map((o) => this.#orderRow(o)),
    );
  }

  #positionRow(p: PositionDto): HTMLTableRowElement {
    const row = el(
      'tr',
      { style: 'cursor:pointer' },
      el('td', {}, el('b', {}, p.symbol)),
      td('r', String(p.qty)),
      td('r', `@ ${p.avg_cost_cents === null ? '—' : fmtPrice(p.avg_cost_cents)}`),
      td(`r ${trendClass(p.unrealised_pnl_cents)}`, fmtSignedPrice(p.unrealised_pnl_cents)),
    );
    row.addEventListener('click', () => this.#actions.selectSymbol(p.symbol));
    return row;
  }

  #orderRow(o: OpenOrderDto): HTMLLIElement {
    const cancel = el('button', { type: 'button', title: 'cancel' }, '✕');
    cancel.addEventListener('click', () => void this.#actions.cancelOrder(o));
    return el(
      'li',
      {},
      el('span', { class: o.side === 'buy' ? 'up' : 'down' }, o.side),
      el('span', {}, o.symbol),
      el('span', {}, `${o.remaining}/${o.qty}`),
      el('span', {}, `@ ${fmtPrice(o.price_cents)}`),
      cancel,
    );
  }
}

function stat(label: string, value: string, cls = ''): HTMLElement {
  return el('span', {}, `${label} `, el('b', cls === '' ? {} : { class: cls }, value));
}
