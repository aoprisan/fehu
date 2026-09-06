/**
 * Boot: build the store, wire the panels to it, load the initial data and
 * open the stream.
 *
 * Data flows one way — `actions` write to the store and emit topics, panels
 * subscribe to the topics they draw from. Nothing renders from a fetch
 * response directly.
 */

import './styles.css';

import { Actions } from './actions.js';
import { errorMessage } from './api.js';
import { Chart } from './chart/chart.js';
import { AccountPanel } from './panels/account.js';
import { BookPanel } from './panels/book.js';
import { EventsPanel } from './panels/events.js';
import { HeaderPanel } from './panels/header.js';
import { IntervalBar } from './panels/interval-bar.js';
import { SymbolList } from './panels/symbols.js';
import { TapePanel } from './panels/tape.js';
import { TicketPanel } from './panels/ticket.js';
import { setStatus } from './status.js';
import { MarketStream } from './stream.js';
import { Store } from './store.js';

const store = new Store();
const actions = new Actions(store);

const ticket = new TicketPanel(store, actions);
const chart = new Chart(store, {
  onHover: (index) => actions.setHover(index),
  onZoom: (pitch) => actions.setPitch(pitch),
});

new HeaderPanel(store);
new SymbolList(store, actions);
new BookPanel(store, { onPriceClick: (cents) => ticket.setLimitPrice(cents) });
new TapePanel(store);
new AccountPanel(store, actions);
new EventsPanel(store, actions);
const intervals = new IntervalBar(store, actions);

// Anything that changes what the chart shows schedules one repaint.
store.on(['bars', 'events', 'trader', 'symbols'], () => chart.schedule());

const stream = new MarketStream(store, actions);

async function boot(): Promise<void> {
  await actions.loadSymbols();
  intervals.select(store.state.interval);
  ticket.updateLabel();
  await Promise.all([
    actions.loadCatalog(),
    actions.loadEvents(),
    actions.loadTrader(),
    actions.loadBookAndTape(),
  ]);
  stream.connect();
}

boot().catch((e: unknown) => setStatus(errorMessage(e), true));
