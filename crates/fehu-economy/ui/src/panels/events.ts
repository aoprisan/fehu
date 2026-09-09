/**
 * The game-event console: the catalogue as buttons, a magnitude slider, a
 * form for raw simulator events, and the audit log.
 */

import type { Actions } from '../actions.js';
import { byId, el, query, replace } from '../dom.js';
import { fmtTs } from '../format.js';
import { MAX_MAGNITUDE, type CatalogEntry, type PushEventRequest, type SimEvent, type SimEventType } from '../types.js';
import type { Store } from '../store.js';

/** Kinds the market reads as bad news, styled as such. */
const NEGATIVE = /miss|scandal|lawsuit|crash|resign|hike/;

const LOG_ROWS = 100;

export class EventsPanel {
  readonly #store: Store;
  readonly #actions: Actions;
  readonly #company = byId('company-events');
  readonly #market = byId('market-events');
  readonly #log = byId('log');
  readonly #magnitude = byId('mag', HTMLInputElement);
  readonly #magnitudeValue = byId('magv');
  readonly #raw = byId('raw', HTMLFormElement);

  constructor(store: Store, actions: Actions) {
    this.#store = store;
    this.#actions = actions;
    store.on('catalog', () => this.renderCatalog());
    store.on('events', () => this.renderLog());

    this.#magnitude.addEventListener('input', () => this.#showMagnitude());
    this.#showMagnitude();
    this.#raw.addEventListener('submit', (ev) => {
      ev.preventDefault();
      void this.#submitRaw();
    });
  }

  renderCatalog(): void {
    const company: HTMLElement[] = [];
    const market: HTMLElement[] = [];
    for (const entry of this.#store.state.catalog) {
      const button = this.#catalogButton(entry);
      (entry.scope === 'market' ? market : company).push(button);
    }
    replace(this.#company, company);
    replace(this.#market, market);
  }

  renderLog(): void {
    replace(
      this.#log,
      this.#store.state.events.slice(0, LOG_ROWS).map((e) => logRow(e)),
    );
  }

  #catalogButton(entry: CatalogEntry): HTMLButtonElement {
    const effects = entry.effects.map((fx) => JSON.stringify(fx)).join('\n');
    const button = el(
      'button',
      {
        type: 'button',
        class: NEGATIVE.test(entry.kind) ? 'danger' : undefined,
        title: `${entry.description}\n\n${effects}`,
      },
      entry.label,
    );
    button.addEventListener('click', () => {
      void this.#actions.sendGameEvent(entry, this.#currentMagnitude());
    });
    return button;
  }

  #currentMagnitude(): number {
    const value = Number.parseFloat(this.#magnitude.value);
    if (!Number.isFinite(value)) return 1;
    return Math.min(MAX_MAGNITUDE, Math.max(0.01, value));
  }

  #showMagnitude(): void {
    this.#magnitudeValue.textContent = this.#currentMagnitude().toFixed(2);
  }

  async #submitRaw(): Promise<void> {
    const type = query(this.#raw, '[name=type]', HTMLSelectElement).value as SimEventType;
    const a = Number.parseFloat(query(this.#raw, '[name=a]', HTMLInputElement).value);
    const b = Number.parseFloat(query(this.#raw, '[name=b]', HTMLInputElement).value);
    const delay = Number.parseFloat(query(this.#raw, '[name=delay]', HTMLInputElement).value);

    const event = buildSimEvent(type, a, b);
    if (event === null) return;
    const body: PushEventRequest = {
      ...event,
      source: 'ui',
      ...(Number.isFinite(delay) ? { delay_secs: delay } : {}),
    };
    await this.#actions.sendSimEvent(body);
  }
}

/**
 * Assemble the tagged event the server expects. `a` is the first numeric
 * field of the variant, `b` its half-life where it has one.
 */
function buildSimEvent(type: SimEventType, a: number, b: number): SimEvent | null {
  if (!Number.isFinite(a)) return null;
  switch (type) {
    case 'jump':
      return { type, pct: a };
    case 'drift_shift':
    case 'vol_shift':
      return Number.isFinite(b) ? { type, delta: a, half_life_secs: b } : null;
    case 'drift_for_total_move':
      return Number.isFinite(b) ? { type, total: a, half_life_secs: b } : null;
    case 'fundamental_shift':
      return { type, delta: a };
    case 'fundamental_target':
      return { type, target_cents: Math.round(a) };
    default:
      return null;
  }
}

function logRow(e: import('../types.js').EventRecord): HTMLLIElement {
  const magnitude = e.magnitude !== null && e.magnitude !== 1 ? ` ×${e.magnitude}` : '';
  const detail = e.summary.join(' · ') + (e.note === null ? '' : ` — ${e.note}`);
  return el(
    'li',
    {},
    el('span', { class: 'k' }, `#${e.id} ${e.kind}${magnitude}`),
    ' ',
    el('span', { class: 'm' }, `${e.symbols.join(', ')} · ${fmtTs(e.at_ms, true)} · ${e.source}`),
    el('br'),
    el('span', { class: 'm' }, detail),
  );
}
