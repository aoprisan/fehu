/** The M1 / M5 / H1 / D1 selector in the chart toolbar. */

import type { Actions } from '../actions.js';
import { byId, el, replace } from '../dom.js';
import { INTERVALS } from '../intervals.js';
import type { Store } from '../store.js';
import type { Interval } from '../types.js';

export class IntervalBar {
  readonly #store: Store;
  readonly #actions: Actions;
  readonly #root = byId('intervals');
  readonly #buttons = new Map<Interval, HTMLButtonElement>();

  constructor(store: Store, actions: Actions) {
    this.#store = store;
    this.#actions = actions;
    replace(
      this.#root,
      INTERVALS.map(({ interval, label }) => this.#button(interval, label)),
    );
    this.#markActive();
  }

  select(interval: Interval): void {
    this.#actions.selectInterval(interval);
    this.#markActive();
  }

  #button(interval: Interval, label: string): HTMLButtonElement {
    const button = el('button', { type: 'button', 'data-iv': interval }, label);
    button.addEventListener('click', () => this.select(interval));
    this.#buttons.set(interval, button);
    return button;
  }

  #markActive(): void {
    for (const [interval, button] of this.#buttons) {
      button.classList.toggle('active', interval === this.#store.state.interval);
    }
  }
}
