/**
 * The economy half of a player: the wallet their money is in, the units they
 * hold of the world's goods, and what they have in the furnace.
 *
 * The three belong together because a job spends all three at once — it takes
 * units and cents now and gives back units later — so they are one panel with
 * three sections rather than three panels that would always be read as one.
 */

import type { Actions } from '../actions.js';
import { byId, el, replace } from '../dom.js';
import { fmtPrice } from '../format.js';
import type { Store } from '../store.js';
import type { HoldingDto, Job, Recipe } from '../types.js';

/** Basis points in one, as the server counts them. */
const BPS = 10_000;

export class EconomyPanel {
  readonly #store: Store;
  readonly #actions: Actions;
  readonly #wallet = byId('wallet');
  readonly #inventory = byId('inventory');
  readonly #jobs = byId('jobs');
  readonly #recipes = byId('recipes', HTMLSelectElement);
  readonly #start = byId('start-job', HTMLFormElement);

  constructor(store: Store, actions: Actions) {
    this.#store = store;
    this.#actions = actions;
    store.on(['economy', 'trader'], () => this.render());
    // The clock moves what a job has left to run, and nothing else does.
    store.on('clock', () => this.#renderJobs());
    this.#start.addEventListener('submit', (ev) => {
      ev.preventDefault();
      const recipe = this.#recipes.value;
      if (recipe === '') return;
      void this.#actions.startJob(recipe);
    });
  }

  render(): void {
    this.#renderWallet();
    this.#renderInventory();
    this.#renderRecipes();
    this.#renderJobs();
  }

  #renderWallet(): void {
    const { wallet, effects, symbol } = this.#store.state;
    if (wallet === null) {
      replace(this.#wallet, ['—']);
      return;
    }
    const here = effects.find((e) => e.symbol === symbol);
    const rows: (string | HTMLElement)[] = [
      stat(`wallet #${wallet.wallet}`, wallet.kind),
      stat('balance', fmtPrice(wallet.balance_cents)),
      stat('reserved', fmtPrice(wallet.reserved_cents)),
      stat('free', fmtPrice(wallet.available_cents)),
    ];
    // What the world's mood is doing here, shown only when it is doing
    // something: an unmoved market should not carry two "×1.00" readouts.
    if (here !== undefined && here.demand_bps !== BPS) {
      rows.push(stat('demand', mult(here.demand_bps), trend(here.demand_bps)));
    }
    if (here !== undefined && here.production_bps !== BPS) {
      rows.push(stat('yield', mult(here.production_bps), trend(here.production_bps)));
    }
    replace(this.#wallet, rows);
  }

  #renderInventory(): void {
    const { inventory } = this.#store.state;
    if (inventory.length === 0) {
      replace(this.#inventory, [el('tr', {}, el('td', { class: 'm' }, 'no goods held'))]);
      return;
    }
    replace(
      this.#inventory,
      inventory.map((h) => this.#inventoryRow(h)),
    );
  }

  /** One good. Reserved units show as `free/held`: only free can be smelted. */
  #inventoryRow(h: HoldingDto): HTMLTableRowElement {
    return el(
      'tr',
      {},
      el('td', {}, el('b', {}, h.symbol)),
      el(
        'td',
        { class: 'r' },
        h.reserved_shares === 0 ? String(h.qty) : `${h.free_shares}/${h.qty}`,
      ),
      el('td', { class: 'r' }, `@ ${h.avg_cost_cents === null ? '—' : fmtPrice(h.avg_cost_cents)}`),
    );
  }

  #renderRecipes(): void {
    const { recipes } = this.#store.state;
    const chosen = this.#recipes.value;
    replace(
      this.#recipes,
      recipes.map((r) => el('option', { value: r.id }, label(r))),
    );
    this.#start.classList.toggle('closed', recipes.length === 0);
    if (recipes.some((r) => r.id === chosen)) this.#recipes.value = chosen;
  }

  #renderJobs(): void {
    const { jobs, simNow } = this.#store.state;
    if (jobs.length === 0) {
      replace(this.#jobs, [el('li', { class: 'm' }, 'nothing in the furnace')]);
      return;
    }
    replace(
      this.#jobs,
      jobs.slice(0, 12).map((j) => this.#jobRow(j, simNow)),
    );
  }

  #jobRow(job: Job, simNow: number | null): HTMLLIElement {
    const made = job.outputs.map((o) => `${o.qty} ${o.symbol}`).join(', ');
    const left = simNow === null ? null : job.due_at_ms - simNow;
    const state =
      job.status === 'running'
        ? left === null || left <= 0
          ? 'due'
          : countdown(left)
        : job.status;
    const row = el(
      'li',
      {},
      el('span', { class: job.status === 'running' ? '' : 'm' }, `#${job.id}`),
      el('span', {}, job.recipe),
      el('span', {}, made),
      el('span', { class: job.status === 'done' ? 'up' : '' }, state),
    );
    if (job.status === 'running') {
      const cancel = el('button', { type: 'button', title: 'cancel' }, '✕');
      cancel.addEventListener('click', () => void this.#actions.cancelJob(job.id));
      row.append(cancel);
    }
    return row;
  }
}

/** `2 ORE → 1 INGOT · 5.00 · 5m`, which is the whole of a recipe. */
function label(r: Recipe): string {
  const side = (lines: Recipe['inputs']): string =>
    lines.length === 0 ? '—' : lines.map((l) => `${l.qty} ${l.symbol}`).join(' + ');
  const parts = [`${side(r.inputs)} → ${side(r.outputs)}`];
  if (r.cost_cents > 0) parts.push(fmtPrice(r.cost_cents));
  parts.push(duration(r.duration_secs));
  return `${r.id}: ${parts.join(' · ')}`;
}

/** Simulated seconds, as a game would say them: `90s`, `5m`, `2h`. */
function duration(secs: number): string {
  if (secs < 90) return `${secs}s`;
  if (secs < 5_400) return `${Math.round(secs / 60)}m`;
  return `${Math.round(secs / 360) / 10}h`;
}

function countdown(ms: number): string {
  return duration(Math.ceil(ms / 1_000));
}

/** `×1.4`, from basis points. */
function mult(bps: number): string {
  return `×${(bps / BPS).toFixed(2)}`;
}

function trend(bps: number): string {
  if (bps > BPS) return 'up';
  return bps < BPS ? 'down' : '';
}

function stat(label: string, value: string, cls = ''): HTMLElement {
  return el('span', {}, `${label} `, el('b', cls === '' ? {} : { class: cls }, value));
}
