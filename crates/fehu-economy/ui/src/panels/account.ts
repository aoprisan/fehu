/**
 * Cash, positions, resting orders and held stops, plus the equity readout in
 * the header.
 *
 * Orders and stops are listed apart because they are different things: an
 * order is in the book and holds cash or shares against it, a stop is a
 * trigger that holds nothing until the price reaches it.
 */

import type { Actions } from '../actions.js';
import { byId, el, query, replace, td } from '../dom.js';
import { fmtPrice, fmtSignedPrice, trendClass } from '../format.js';
import type { Store } from '../store.js';
import type { OpenOrderDto, PositionDto, StopOrder } from '../types.js';

export class AccountPanel {
  readonly #store: Store;
  readonly #actions: Actions;
  readonly #header = byId('acct-hdr');
  readonly #summary = byId('acct');
  readonly #positions = byId('positions');
  readonly #orders = byId('orders');
  readonly #stops = byId('stops');
  readonly #deposit = byId('deposit', HTMLFormElement);
  readonly #amount = query(byId('deposit'), '[name=amount]', HTMLInputElement);

  constructor(store: Store, actions: Actions) {
    this.#store = store;
    this.#actions = actions;
    store.on('trader', () => this.render());
    // The form takes an amount in the same units the panel shows; the API
    // itself only ever sees integer cents.
    this.#deposit.addEventListener('submit', (ev) => {
      ev.preventDefault();
      const amount = Number.parseFloat(this.#amount.value);
      if (!Number.isFinite(amount)) return;
      this.#amount.value = '';
      void this.#actions.deposit(Math.round(amount * 100));
    });
  }

  render(): void {
    const trader = this.#store.state.trader;
    if (trader === null) return;

    const pnl = trader.realised_pnl_cents + trader.unrealised_pnl_cents;
    const frozen = trader.account_status !== 'active';
    this.#deposit.classList.toggle('closed', trader.account_status === 'closed');
    replace(this.#header, [
      `${trader.name} #${trader.id} · equity `,
      el('b', {}, fmtPrice(trader.equity_cents)),
      ' · P&L ',
      el('b', { class: trendClass(pnl) }, fmtSignedPrice(pnl)),
    ]);

    replace(this.#summary, [
      stat(`account #${trader.account_id}`, trader.account_status, frozen ? 'down' : ''),
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
    replace(
      this.#stops,
      trader.stops.map((s) => this.#stopRow(s)),
    );
  }

  /** One position. Reserved shares show as `free/held`: only free can be sold. */
  #positionRow(p: PositionDto): HTMLTableRowElement {
    const row = el(
      'tr',
      { style: 'cursor:pointer' },
      el('td', {}, el('b', {}, p.symbol)),
      td('r', p.reserved_shares === 0 ? String(p.qty) : `${p.free_shares}/${p.qty}`),
      td('r', `@ ${p.avg_cost_cents === null ? '—' : fmtPrice(p.avg_cost_cents)}`),
      td(`r ${trendClass(p.unrealised_pnl_cents)}`, fmtSignedPrice(p.unrealised_pnl_cents)),
    );
    row.addEventListener('click', () => this.#actions.selectSymbol(p.symbol));
    return row;
  }

  /**
   * One held stop: what it will do, and the price that will make it do it.
   * An iceberg's hidden remainder is the order list's business; here the
   * quantity is the whole of what fires.
   */
  #stopRow(s: StopOrder): HTMLLIElement {
    const cancel = el('button', { type: 'button', title: 'withdraw' }, '✕');
    cancel.addEventListener('click', () => void this.#actions.cancelStop(s));
    const becomes =
      s.limit_price_cents === null ? 'at market' : `limit ${fmtPrice(s.limit_price_cents)}`;
    return el(
      'li',
      {},
      el('span', { class: s.side === 'buy' ? 'up' : 'down' }, s.side),
      el('span', {}, s.symbol),
      el('span', {}, String(s.qty)),
      el('span', {}, `⇢ ${fmtPrice(s.stop_price_cents)}`),
      el('span', { class: 'becomes' }, becomes),
      cancel,
    );
  }

  /**
   * One resting order. An iceberg says what it is showing: the book only
   * ever sees that slice, so `10/100` resting with `10` on show is not the
   * same order as `100` on show, and the ticket can now place either.
   */
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
      ...(o.display_qty === null
        ? []
        : [el('span', { class: 'becomes' }, `${o.shown_qty} shown`)]),
      cancel,
    );
  }
}

function stat(label: string, value: string, cls = ''): HTMLElement {
  return el('span', {}, `${label} `, el('b', cls === '' ? {} : { class: cls }, value));
}
