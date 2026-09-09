/**
 * The operator's dashboard: what the economy is doing, and what to do about
 * it.
 *
 * It is one overlay rather than a second page because the bundle is one file
 * embedded in the Rust binary (`assets/app.js`, named by `include_str!`), and
 * because the two views want the same store: the clock, the symbols and the
 * stream are already here.
 *
 * It reads `GET /api/overview` — one market job, so every number in it was
 * true at the same instant — and `GET /api/health`, and it reads them only
 * while it is open. Closing it stops the polling; nothing here costs anything
 * when nobody is looking.
 *
 * Two things it does not ask the server for. **Rates**: the ledger's flow
 * meter and the server's metrics are running totals with no window behind
 * them, deliberately, so the page keeps its own readings and takes the
 * differences. **The audit**: `GET /api/reconcile` snapshots the whole market,
 * so it is a button, never a poll.
 */

import type { Actions } from '../actions.js';
import { currentAdminKey } from '../api.js';
import { byId, el, query, replace } from '../dom.js';
import { fmtPrice, fmtTs, fmtVol } from '../format.js';
import type { Store } from '../store.js';
import { readTheme } from '../theme.js';
import type { FlowDto, OverviewDto, SupplyDto, WalletKind, WalletRow } from '../types.js';

/** The hash that opens the dashboard, so a reload comes back to it. */
const HASH = '#economy';

/**
 * What to call each kind of wallet where the currency sits.
 *
 * Identity in that panel is carried by the row's own label, not by a colour,
 * which is why it is a bar per row and not a stacked bar in six parts: six
 * categorical hues that stay apart under colour-blindness on a dark surface
 * do not exist, and a labelled row needs none of them.
 */
const WALLET_LABEL: Readonly<Record<WalletKind, string>> = {
  treasury: 'treasury',
  budget: 'budgets',
  issuer: 'issuers',
  npc: 'merchants',
  player: 'players',
  venue: 'venue',
  synthetic: 'unfunded liquidity',
  issuance: 'issuance',
};

/**
 * The three series the time chart draws, and the hues they wear.
 *
 * Three, and no more: a fourth line would need a fourth hue that stays apart
 * from these under protanopia and deuteranopia against `--panel`, and these
 * three are already at the edge of what that allows. The colours are the
 * chart's own, defined beside the rest of the palette in `styles.css`.
 */
const SERIES: ReadonlyArray<[label: string, key: keyof SupplyDto, cssVar: string]> = [
  ['circulating', 'circulating_cents', '--series-1'],
  ['treasury', 'treasury_cents', '--series-2'],
  ['players', 'player_cents', '--series-3'],
];

/** Room kept at the right of the plot for the lines' own labels. */
const LABEL_GUTTER = 72;

/** Milliseconds in a minute, for the per-minute rates. */
const MINUTE = 60_000;

export class OpsPanel {
  readonly #store: Store;
  readonly #actions: Actions;
  readonly #root = byId('ops');
  readonly #key = byId('ops-key', HTMLInputElement);
  readonly #every = byId('ops-every', HTMLSelectElement);
  readonly #kind = byId('ops-wallet-kind', HTMLSelectElement);
  readonly #chart = byId('ops-chart', HTMLCanvasElement);
  /** The sample the pointer is over, or `null` for the latest. */
  #hover: number | null = null;
  #timer: number | null = null;

  constructor(store: Store, actions: Actions) {
    this.#store = store;
    this.#actions = actions;
    store.on('ops', () => this.render());
    // The symbol rows show what is halted, and the stream is what says so.
    store.on(['quotes', 'symbols'], () => this.#renderSymbols());

    byId('ops-open').addEventListener('click', () => this.open(true));
    byId('ops-close').addEventListener('click', () => this.open(false));
    byId('ops-audit').addEventListener('click', () => void this.#actions.runAudit());
    this.#key.value = currentAdminKey() ?? '';
    this.#key.addEventListener('change', () => {
      this.#actions.useAdminKey(this.#key.value.trim());
      void this.#actions.loadOps();
    });
    this.#every.addEventListener('change', () => {
      this.#store.state.ops.intervalMs = Number(this.#every.value);
      this.#schedule();
    });
    this.#kind.addEventListener('change', () => this.#renderWallets());
    this.#chart.addEventListener('mousemove', (ev) => this.#onHover(ev));
    this.#chart.addEventListener('mouseleave', () => {
      this.#hover = null;
      this.#drawChart();
      this.#renderLegend();
    });
    window.addEventListener('resize', () => this.#drawChart());
    window.addEventListener('hashchange', () => this.open(location.hash === HASH));

    this.#wireForms();
    if (location.hash === HASH) this.open(true);
  }

  /** Show or hide the dashboard, and start or stop the polling with it. */
  open(open: boolean): void {
    this.#actions.setOpsOpen(open);
    this.#root.hidden = !open;
    if (open && location.hash !== HASH) location.hash = HASH;
    if (!open && location.hash === HASH) {
      history.replaceState(null, '', location.pathname + location.search);
    }
    this.#schedule();
    if (open) void this.#actions.loadOps();
  }

  /** Poll while open, at the chosen cadence; never while closed. */
  #schedule(): void {
    if (this.#timer !== null) {
      clearInterval(this.#timer);
      this.#timer = null;
    }
    const { open, intervalMs } = this.#store.state.ops;
    if (!open || intervalMs <= 0) return;
    this.#timer = setInterval(() => void this.#actions.loadOps(), intervalMs);
  }

  render(): void {
    const { ops } = this.#store.state;
    this.#root.hidden = !ops.open;
    if (!ops.open) return;
    const message = byId('ops-msg');
    message.textContent = ops.error ?? ops.note ?? '';
    message.classList.toggle('err', ops.error !== null);
    const overview = ops.overview;
    if (overview === null) return;
    byId('ops-clock').textContent = `sim ${fmtTs(overview.at_ms, true)} UTC`;
    byId('ops-seq').textContent = `${fmtVol(overview.journal_seq)} commands`;
    this.#renderTiles(overview);
    this.#renderWhere(overview.supply, overview.wallets);
    this.#renderLegend();
    this.#drawChart();
    this.#renderFlows(overview.flows);
    this.#renderServer();
    this.#renderAudit();
    this.#renderWallets();
    this.#renderBudgets(overview);
    this.#renderNpcs(overview);
    this.#renderWorld(overview);
    this.#renderSymbols();
  }

  // --- currency --------------------------------------------------------------

  #renderTiles(overview: OverviewDto): void {
    const s = overview.supply;
    const drift = s.circulating_cents - s.outstanding_cents;
    replace(byId('ops-tiles'), [
      tile('in circulation', money(s.circulating_cents)),
      tile('minted', money(s.minted_cents)),
      tile('burned', money(s.burned_cents)),
      tile('wallets', String(s.wallets)),
      s.balanced
        ? tile('conserved', 'yes', 'up')
        : tile('conserved', `off by ${money(drift)}`, 'down'),
      s.synthetic_debt_cents > 0
        ? tile('unfunded debt', money(s.synthetic_debt_cents), 'down')
        : tile('unfunded debt', 'none'),
    ]);
  }

  /**
   * One bar per place the currency sits, longest first, each labelled with
   * what it is and what it holds.
   *
   * Added up from the wallets rather than read off the supply, because the
   * supply names six kinds and there are eight: the two that may run
   * negative are places currency sits too, and a composition that left them
   * out would not come to a hundred per cent. Issuance is the exception —
   * it is the mirror of the sum, not a term in it.
   */
  #renderWhere(supply: SupplyDto, wallets: WalletRow[]): void {
    const byKind = new Map<WalletKind, number>();
    for (const w of wallets) {
      if (w.kind === 'issuance') continue;
      byKind.set(w.kind, (byKind.get(w.kind) ?? 0) + w.balance_cents);
    }
    const rows = [...byKind.entries()]
      .map(([kind, cents]) => [WALLET_LABEL[kind], cents] as const)
      .sort((a, b) => b[1] - a[1]);
    const most = Math.max(1, ...rows.map(([, cents]) => Math.abs(cents)));
    replace(
      byId('ops-where'),
      rows.map(([label, cents]) =>
        el(
          'tr',
          {},
          el('td', {}, label),
          el(
            'td',
            { class: 'bar' },
            el('i', {
              class: cents < 0 ? 'down' : '',
              style: `width:${(Math.abs(cents) / most) * 100}%`,
            }),
          ),
          el('td', { class: 'r' }, fmtPrice(cents)),
          el(
            'td',
            { class: 'r m' },
            supply.circulating_cents > 0
              ? `${((cents / supply.circulating_cents) * 100).toFixed(1)}%`
              : '—',
          ),
        ),
      ),
    );
  }

  /**
   * The legend, which doubles as the chart's readout: hovering a sample
   * writes that sample's values into it rather than into a floating tooltip
   * that would cover the line it is describing.
   */
  #renderLegend(): void {
    const { history } = this.#store.state.ops;
    const at = this.#hover ?? history.length - 1;
    const sample = history[at];
    replace(
      byId('ops-legend'),
      SERIES.map(([label, key, cssVar]) =>
        el(
          'span',
          { class: 'key' },
          el('i', { style: `background:var(${cssVar})` }),
          label,
          ' ',
          el('b', {}, sample === undefined ? '—' : money(sample.supply[key] as number)),
        ),
      ).concat(
        el(
          'span',
          { class: 'm' },
          sample === undefined
            ? 'reading…'
            : this.#hover === null
              ? `${history.length} reading${history.length === 1 ? '' : 's'}`
              : new Date(sample.at).toLocaleTimeString(),
        ),
      ),
    );
  }

  #onHover(ev: MouseEvent): void {
    const { history } = this.#store.state.ops;
    if (history.length === 0) return;
    const rect = this.#chart.getBoundingClientRect();
    const x = (ev.clientX - rect.left) / Math.max(1, rect.width);
    this.#hover = Math.min(history.length - 1, Math.max(0, Math.round(x * (history.length - 1))));
    this.#drawChart();
    this.#renderLegend();
  }

  /**
   * The three series over the readings the page has taken, each drawn as
   * what it has *changed by* since the first reading the page still holds.
   *
   * One axis for all three, and never two: a second scale would let any pair
   * of them be made to cross wherever the drawing pleased. But a trillion in
   * circulation and a hundred thousand in players' hands share an axis only
   * as two flat lines, so what is plotted is the change rather than the
   * level — the same unit for all three, a common zero, and the thing an
   * operator is actually watching for. The levels themselves are in the
   * legend, where they are read as numbers rather than as heights.
   */
  #drawChart(): void {
    const canvas = this.#chart;
    const history = this.#store.state.ops.history;
    const dpr = window.devicePixelRatio || 1;
    const width = canvas.clientWidth;
    const height = canvas.clientHeight;
    if (width === 0 || height === 0) return;
    canvas.width = Math.round(width * dpr);
    canvas.height = Math.round(height * dpr);
    const ctx = canvas.getContext('2d');
    if (ctx === null) return;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, width, height);
    const theme = readTheme();
    if (history.length < 2) {
      ctx.fillStyle = theme.muted;
      ctx.font = '12px system-ui, sans-serif';
      ctx.fillText('waiting for a second reading', 8, height / 2);
      return;
    }
    const style = getComputedStyle(document.documentElement);
    const base = history[0];
    if (base === undefined) return;
    const series = SERIES.map(([label, key, cssVar]) => ({
      label,
      cssVar,
      points: history.map((s) => (s.supply[key] as number) - (base.supply[key] as number)),
    }));
    const moves = series.flatMap((s) => s.points);
    // A world where nothing has moved yet still gets an axis, rather than a
    // division by a span of zero.
    const reach = Math.max(1, ...moves.map(Math.abs));
    const pad = 10;
    const y = (v: number) => pad + (1 - (v + reach) / (reach * 2)) * (height - pad * 2);
    const x = (i: number) => (i / (history.length - 1)) * (width - LABEL_GUTTER);

    // Zero: where every series starts, and the line each is read against.
    ctx.strokeStyle = theme.gridStrong;
    ctx.lineWidth = 1;
    ctx.beginPath();
    ctx.moveTo(0, y(0) + 0.5);
    ctx.lineTo(width - LABEL_GUTTER, y(0) + 0.5);
    ctx.stroke();

    for (const { label, cssVar, points } of series) {
      ctx.strokeStyle = style.getPropertyValue(cssVar).trim() || theme.accent;
      ctx.lineWidth = 2;
      ctx.beginPath();
      points.forEach((value, i) => {
        if (i === 0) ctx.moveTo(x(i), y(value));
        else ctx.lineTo(x(i), y(value));
      });
      ctx.stroke();
      // Direct labels: identity never rests on colour alone.
      const last = points[points.length - 1];
      if (last !== undefined) {
        ctx.fillStyle = theme.muted;
        ctx.font = '10px system-ui, sans-serif';
        ctx.fillText(label, width - LABEL_GUTTER + 6, y(last) + 3);
      }
    }
    ctx.fillStyle = theme.muted;
    ctx.font = '10px system-ui, sans-serif';
    ctx.fillText(`±${money(reach)}`, 2, 10);

    if (this.#hover !== null) {
      const hx = x(this.#hover);
      ctx.strokeStyle = theme.gridStrong;
      ctx.lineWidth = 1;
      ctx.beginPath();
      ctx.moveTo(hx + 0.5, 0);
      ctx.lineTo(hx + 0.5, height);
      ctx.stroke();
    }
  }

  // --- movements -------------------------------------------------------------

  /**
   * What every reason has moved, and how fast it is moving now.
   *
   * The rate is this page's arithmetic: the difference between the oldest
   * reading it still holds and the newest, over the wall time between them.
   */
  #renderFlows(flows: FlowDto[]): void {
    const { history } = this.#store.state.ops;
    const first = history[0];
    const last = history[history.length - 1];
    const minutes =
      first === undefined || last === undefined ? 0 : (last.at - first.at) / MINUTE;
    const earlier = new Map((first?.flows ?? []).map((f) => [f.reason, f.cents]));
    replace(
      byId('ops-flows'),
      flows
        .filter((f) => f.count > 0)
        .map((f) => {
          const since = f.cents - (earlier.get(f.reason) ?? f.cents);
          return el(
            'tr',
            {},
            el('td', {}, f.reason.replace('_', ' ')),
            el('td', { class: 'r m' }, fmtVol(f.count)),
            el('td', { class: 'r' }, fmtPrice(f.cents)),
            el(
              'td',
              { class: 'r m' },
              minutes > 0.2 && since > 0 ? fmtPrice(since / minutes) : '—',
            ),
          );
        })
        .concat(
          flows.every((f) => f.count === 0)
            ? [el('tr', {}, el('td', { class: 'm' }, 'nothing has moved yet'))]
            : [],
        ),
    );
  }

  // --- the server itself -----------------------------------------------------

  #renderServer(): void {
    const health = this.#store.state.ops.health;
    const overview = this.#store.state.ops.overview;
    if (health === null || overview === null) return;
    const m = health.metrics;
    replace(byId('ops-server'), [
      tile('up', duration(health.uptime_secs)),
      tile('ticks', fmtVol(health.ticks_total)),
      tile('trades', fmtVol(health.trades_total)),
      tile('requests', fmtVol(m.requests.count)),
      tile('request mean', micros(m.requests.micros_mean)),
      tile('request worst', micros(m.requests.micros_max)),
      tile('step mean', micros(m.engine_step.micros_mean)),
      tile('step worst', micros(m.engine_step.micros_max)),
      tile(
        'in flight',
        `${health.requests_in_flight}${health.max_in_flight > 0 ? `/${health.max_in_flight}` : ''}`,
      ),
      tile('streams', String(health.stream_subscribers)),
      m.requests_shed > 0
        ? tile('shed', fmtVol(m.requests_shed), 'down')
        : tile('shed', 'none'),
      health.settlement_failures > 0
        ? tile('settlement failures', String(health.settlement_failures), 'down')
        : tile('settlement failures', 'none'),
      tile('outbox pending', fmtVol(overview.outbox.pending)),
      overview.outbox.dropped > 0
        ? tile('outbox dropped', fmtVol(overview.outbox.dropped), 'down')
        : tile('outbox dropped', 'none'),
    ]);
  }

  #renderAudit(): void {
    const audit = this.#store.state.ops.reconciliation;
    const out = byId('ops-audit-out');
    if (audit === null) {
      out.textContent = 'not run';
      out.classList.remove('err');
      return;
    }
    out.classList.toggle('err', !audit.valid);
    replace(out, [
      el(
        'div',
        {},
        el('b', { class: audit.valid ? 'up' : 'down' }, audit.valid ? 'balanced' : 'broken'),
        ` · ${audit.accounts_checked} accounts · ${audit.wallets_checked} wallets · ` +
          `${audit.resting_orders_checked} resting orders · ${audit.jobs_running} jobs`,
      ),
      ...audit.issues.slice(0, 20).map((issue) => el('div', { class: 'down' }, issue)),
    ]);
  }

  // --- wallets ---------------------------------------------------------------

  #renderWallets(): void {
    const overview = this.#store.state.ops.overview;
    if (overview === null) return;
    const kinds = [...new Set(overview.wallets.map((w) => w.kind))].sort();
    if (this.#kind.options.length !== kinds.length + 1) {
      const chosen = this.#kind.value;
      replace(this.#kind, [
        el('option', { value: '' }, 'every kind'),
        ...kinds.map((k) => el('option', { value: k }, k)),
      ]);
      this.#kind.value = chosen;
    }
    const wanted = this.#kind.value;
    const rows = overview.wallets
      .filter((w) => wanted === '' || w.kind === wanted)
      .slice()
      .sort((a, b) => b.balance_cents - a.balance_cents);
    replace(
      byId('ops-wallets'),
      rows.length === 0
        ? [el('tr', {}, el('td', { class: 'm' }, 'no wallets of that kind'))]
        : rows.map((w) => this.#walletRow(w)),
    );
  }

  #walletRow(w: WalletRow): HTMLTableRowElement {
    const frozen = w.status !== 'active';
    const row = el(
      'tr',
      {},
      el('td', { class: 'm' }, `#${w.wallet}`),
      el('td', {}, w.kind),
      el('td', {}, w.owner ?? '—'),
      el('td', { class: 'r' }, fmtPrice(w.balance_cents)),
      el('td', { class: 'r m' }, w.reserved_cents === 0 ? '—' : fmtPrice(w.reserved_cents)),
      el('td', { class: frozen ? 'down' : 'm' }, w.status),
    );
    const actions = el('td', { class: 'r' });
    // Only an account has a status to change: the world's own wallets and a
    // symbol's issuer are not somebody who can be frozen out of the market.
    if (w.account_id !== null && w.status !== 'closed') {
      const button = el(
        'button',
        { type: 'button', class: frozen ? '' : 'danger' },
        frozen ? 'unfreeze' : 'freeze',
      );
      const account = w.account_id;
      button.addEventListener('click', () => {
        void this.#actions.setAccountStatus(account, frozen ? 'active' : 'frozen');
      });
      actions.append(button);
    }
    row.append(actions);
    return row;
  }

  // --- budgets, merchants, the world ----------------------------------------

  #renderBudgets(overview: OverviewDto): void {
    replace(
      byId('ops-budgets'),
      overview.budgets.length === 0
        ? [el('tr', {}, el('td', { class: 'm' }, 'no budgets'))]
        : overview.budgets.map((b) => {
            const fund = el('button', { type: 'button' }, 'fund');
            fund.addEventListener('click', () => {
              const amount = prompt(`Fund ${b.name} out of treasury by how much?`, '100.00');
              if (amount === null) return;
              const cents = toCents(amount);
              if (cents !== null) void this.#actions.fundBudget(b.wallet, cents);
            });
            return el(
              'tr',
              {},
              el('td', {}, b.name, el('span', { class: 'm' }, ` #${b.wallet}`)),
              el('td', { class: 'r' }, fmtPrice(b.balance_cents)),
              el('td', { class: 'r m' }, `${fmtPrice(b.paid_cents)} · ${b.paid_count}`),
              el('td', { class: 'r' }, fund),
            );
          }),
    );
    replace(
      byId('ops-rules'),
      overview.rules.length === 0
        ? [el('tr', {}, el('td', { class: 'm' }, 'no rules'))]
        : overview.rules.map((r) => {
            const remove = el('button', { type: 'button', class: 'danger' }, '✕');
            remove.addEventListener('click', () => void this.#actions.removeRewardRule(r.id));
            return el(
              'tr',
              {},
              el('td', {}, r.id),
              el('td', { class: 'r' }, fmtPrice(r.amount_cents)),
              el('td', { class: 'm' }, `#${r.budget}`),
              el('td', { class: 'r' }, remove),
            );
          }),
    );
    const budgets = byId('ops-rule-budget', HTMLSelectElement);
    const chosen = budgets.value;
    replace(
      budgets,
      overview.budgets.map((b) => el('option', { value: String(b.wallet) }, b.name)),
    );
    if (overview.budgets.some((b) => String(b.wallet) === chosen)) budgets.value = chosen;

    // The accounts currency can be minted into or burned out of.
    const accounts = byId('ops-mint-account', HTMLSelectElement);
    const account = accounts.value;
    const players = overview.wallets.filter((w) => w.account_id !== null);
    replace(
      accounts,
      players.map((w) =>
        el(
          'option',
          { value: String(w.account_id) },
          `${w.owner ?? 'account'} #${w.account_id ?? 0} · ${fmtPrice(w.balance_cents)}`,
        ),
      ),
    );
    if (players.some((w) => String(w.account_id) === account)) accounts.value = account;
  }

  #renderNpcs(overview: OverviewDto): void {
    replace(
      byId('ops-npcs'),
      overview.npcs.length === 0
        ? [el('tr', {}, el('td', { class: 'm' }, 'the world runs no merchants'))]
        : overview.npcs.map((n) => {
            const toggle = el(
              'button',
              { type: 'button', class: n.active ? 'danger' : '' },
              n.active ? 'stop' : 'start',
            );
            toggle.addEventListener('click', () => {
              void this.#actions.setNpcActive(n.trader_id, !n.active);
            });
            return el(
              'tr',
              { class: n.active ? '' : 'off' },
              el('td', {}, n.name),
              el('td', {}, n.symbol),
              el('td', { class: 'r' }, fmtPrice(n.cash_cents)),
              el(
                'td',
                { class: 'r m' },
                n.reserved === 0 ? String(n.inventory) : `${n.inventory} · ${n.reserved} held`,
              ),
              el('td', { class: 'r' }, toggle),
            );
          }),
    );
  }

  #renderWorld(overview: OverviewDto): void {
    const { jobs, people } = overview;
    replace(byId('ops-world'), [
      tile('provisioned', String(people.players)),
      tile('accounts', String(people.accounts)),
      tile('traders', String(people.traders)),
      people.frozen > 0 ? tile('frozen', String(people.frozen), 'down') : tile('frozen', 'none'),
      tile('jobs running', String(jobs.running)),
      tile('jobs done', String(jobs.done)),
      tile(
        'next due',
        jobs.next_due_ms === null ? '—' : fmtTs(jobs.next_due_ms),
      ),
    ]);
    replace(
      byId('ops-modifiers'),
      overview.modifiers.length === 0
        ? [el('li', { class: 'm' }, 'the world is at rest')]
        : overview.modifiers
            .slice(-12)
            .reverse()
            .map((mod) =>
              el(
                'li',
                {},
                el('span', {}, `${mod.effect} ${mod.symbol ?? 'everywhere'}`),
                el(
                  'span',
                  { class: mod.delta_bps >= 0 ? 'up' : 'down' },
                  `${mod.delta_bps >= 0 ? '+' : ''}${(mod.delta_bps / 100).toFixed(1)}%`,
                ),
                el('span', { class: 'm' }, mod.kind),
              ),
            ),
    );
  }

  /** What can be traded, and the one switch that decides it. */
  #renderSymbols(): void {
    if (!this.#store.state.ops.open) return;
    const quotes = [...this.#store.state.quotes.values()];
    replace(
      byId('ops-symbols'),
      quotes.map((q) => {
        const button = el(
          'button',
          { type: 'button', class: q.halted ? '' : 'danger' },
          q.halted ? 'resume' : 'halt',
        );
        button.addEventListener('click', () => {
          void this.#actions.setSymbolHalted(q.symbol, !q.halted);
        });
        return el(
          'tr',
          {},
          el('td', {}, el('b', {}, q.symbol)),
          el('td', { class: 'm' }, q.asset_kind),
          el('td', { class: 'r' }, fmtPrice(q.price_cents)),
          el('td', { class: q.halted ? 'down' : 'm' }, q.halted ? 'halted' : 'trading'),
          el('td', { class: 'r' }, button),
        );
      }),
    );
  }

  // --- the forms -------------------------------------------------------------

  #wireForms(): void {
    const budget = byId('ops-budget-new', HTMLFormElement);
    budget.addEventListener('submit', (ev) => {
      ev.preventDefault();
      const name = query(budget, 'input[name=name]', HTMLInputElement).value.trim();
      const cents = toCents(query(budget, 'input[name=amount]', HTMLInputElement).value);
      if (name === '' || cents === null) return;
      void this.#actions.createBudget(name, cents).then(() => budget.reset());
    });

    const rule = byId('ops-rule-new', HTMLFormElement);
    rule.addEventListener('submit', (ev) => {
      ev.preventDefault();
      const id = query(rule, 'input[name=id]', HTMLInputElement).value.trim();
      const wallet = Number(byId('ops-rule-budget', HTMLSelectElement).value);
      const cents = toCents(query(rule, 'input[name=amount]', HTMLInputElement).value);
      if (id === '' || cents === null || !Number.isFinite(wallet)) return;
      void this.#actions.setRewardRule(id, wallet, cents);
    });

    // One form, two buttons: the same amount and the same account, in or out.
    const supply = byId('ops-mint', HTMLFormElement);
    supply.addEventListener('submit', (ev) => {
      ev.preventDefault();
      const submitter = (ev as SubmitEvent).submitter;
      const account = Number(byId('ops-mint-account', HTMLSelectElement).value);
      const cents = toCents(query(supply, 'input[name=amount]', HTMLInputElement).value);
      if (cents === null || !Number.isFinite(account)) return;
      const burn = submitter instanceof HTMLButtonElement && submitter.name === 'burn';
      void (burn ? this.#actions.burn(account, cents) : this.#actions.mint(account, cents));
    });
  }
}

/** One reading on the tile strip: what it is, and what it says. */
function tile(label: string, value: string, cls = ''): HTMLElement {
  return el(
    'div',
    { class: 'tile' },
    el('span', { class: 'm' }, label),
    el('b', cls === '' ? {} : { class: cls }, value),
  );
}

/**
 * Currency where the space is a tile, not a table: `2,750.00` while it fits,
 * `1.25M` and `10.0T` when it does not. Tables keep {@link fmtPrice}, which
 * is exact.
 */
function money(cents: number): string {
  const units = Math.abs(cents) / 100;
  if (units < 1_000_000) return fmtPrice(cents);
  const sign = cents < 0 ? '-' : '';
  for (const [size, suffix] of [
    [1e12, 'T'],
    [1e9, 'B'],
    [1e6, 'M'],
  ] as const) {
    if (units >= size) return `${sign}${(units / size).toFixed(2)}${suffix}`;
  }
  return fmtPrice(cents);
}

/** `"12.50"` → `1250`, and anything that is not a number → `null`. */
function toCents(text: string): number | null {
  const value = Number(text);
  if (!Number.isFinite(value) || value <= 0) return null;
  return Math.round(value * 100);
}

/** Microseconds as a human reads them: `840µs`, `12ms`, `1.4s`. */
function micros(us: number): string {
  if (us < 1_000) return `${us}µs`;
  if (us < 1_000_000) return `${Math.round(us / 1_000)}ms`;
  return `${(us / 1_000_000).toFixed(1)}s`;
}

/** Wall seconds, as an uptime is read: `4m`, `2h 10m`, `3d 4h`. */
function duration(secs: number): string {
  if (secs < 90) return `${secs}s`;
  if (secs < 3_600) return `${Math.round(secs / 60)}m`;
  if (secs < 86_400) return `${Math.floor(secs / 3_600)}h ${Math.round((secs % 3_600) / 60)}m`;
  return `${Math.floor(secs / 86_400)}d ${Math.round((secs % 86_400) / 3_600)}h`;
}
