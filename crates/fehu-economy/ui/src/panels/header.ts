/** The header: simulated clock, time scale and the stream's connection state. */

import { byId, el, replace } from '../dom.js';
import { fmtTs } from '../format.js';
import type { Store } from '../store.js';

const LABELS = {
  connecting: 'connecting',
  live: 'live',
  reconnecting: 'reconnecting',
} as const;

const DOT_CLASS = {
  connecting: 'dot',
  live: 'dot live',
  reconnecting: 'dot off',
} as const;

export class HeaderPanel {
  readonly #store: Store;
  readonly #clock = byId('clock');
  readonly #connection = byId('conn');

  constructor(store: Store) {
    this.#store = store;
    store.on('clock', () => this.renderClock());
    store.on('connection', () => this.renderConnection());
    this.renderConnection();
  }

  renderClock(): void {
    const { simNow, timeScale } = this.#store.state;
    if (simNow === null) return;
    const scale = timeScale === 1 ? '' : ` · ×${timeScale}`;
    this.#clock.textContent = `sim ${fmtTs(simNow, true)} UTC${scale}`;
  }

  renderConnection(): void {
    const state = this.#store.state.connection;
    replace(this.#connection, [el('span', { class: DOT_CLASS[state] }), LABELS[state]]);
  }
}
