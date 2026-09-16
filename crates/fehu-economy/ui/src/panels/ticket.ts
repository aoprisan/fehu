/**
 * The order ticket: side, quantity, what kind of order, time in force, and —
 * folded away under `more` — the options an ordinary order does not need.
 *
 * Four kinds share the form. Market and limit are orders and go to the
 * symbol's order route; stop and stop-limit are *triggers* and go to its stop
 * route, because a stop rests nowhere and reserves nothing until the price
 * reaches it. Which inputs are live follows from the kind and the time in
 * force, in {@link TicketPanel.updateLabel}.
 *
 * A sell is bounded by what the trader actually holds, and a trigger the
 * market has already passed is not a trigger — the server refuses both, so
 * the ticket says so first rather than spending a round trip on it.
 */

import type { Actions } from '../actions.js';
import { byId, query, queryAll } from '../dom.js';
import { fmtPrice, fmtVol } from '../format.js';
import { setOrderStatus } from '../status.js';
import type { Store } from '../store.js';
import type { OrderRequest, Side, StopRequest, TimeInForce } from '../types.js';

const TIFS: readonly TimeInForce[] = ['gtc', 'ioc', 'fok'];

/** What the type select offers. The last two are triggers, not orders. */
type Kind = 'market' | 'limit' | 'stop' | 'stop_limit';

const KINDS: readonly Kind[] = ['market', 'limit', 'stop', 'stop_limit'];

function isTif(value: string): value is TimeInForce {
  return (TIFS as readonly string[]).includes(value);
}

function isKind(value: string): value is Kind {
  return (KINDS as readonly string[]).includes(value);
}

/** Cents from a price field, or `null` if it holds nothing usable. */
function cents(input: HTMLInputElement): number | null {
  const value = Number.parseFloat(input.value);
  return Number.isFinite(value) ? Math.round(value * 100) : null;
}

/** A positive whole number from an optional field, or `null` if it is empty. */
function count(input: HTMLInputElement): number | null {
  if (input.value.trim() === '') return null;
  const value = Number.parseInt(input.value, 10);
  return Number.isSafeInteger(value) && value > 0 ? value : null;
}

export class TicketPanel {
  readonly #store: Store;
  readonly #actions: Actions;
  readonly #form = byId('ticket', HTMLFormElement);
  readonly #symbol = byId('ticket-sym');
  readonly #qty = query(byId('ticket'), '[name=qty]', HTMLInputElement);
  readonly #kind = query(byId('ticket'), '[name=type]', HTMLSelectElement);
  readonly #price = query(byId('ticket'), '[name=price]', HTMLInputElement);
  readonly #stopPrice = query(byId('ticket'), '[name=stop_price]', HTMLInputElement);
  readonly #tif = query(byId('ticket'), '[name=tif]', HTMLSelectElement);
  readonly #postOnly = query(byId('ticket'), '[name=post_only]', HTMLInputElement);
  readonly #day = query(byId('ticket'), '[name=day]', HTMLInputElement);
  readonly #displayQty = query(byId('ticket'), '[name=display_qty]', HTMLInputElement);
  readonly #expiresMins = query(byId('ticket'), '[name=expires_mins]', HTMLInputElement);
  readonly #clientOrderId = query(byId('ticket'), '[name=client_order_id]', HTMLInputElement);
  readonly #submit = query(byId('ticket'), '.submit', HTMLButtonElement);
  readonly #holding = byId('ticket-holding');

  constructor(store: Store, actions: Actions) {
    this.#store = store;
    this.#actions = actions;

    for (const button of queryAll<HTMLButtonElement>(this.#form, '.side button')) {
      button.addEventListener('click', () => {
        const side = button.dataset['side'];
        if (side !== 'buy' && side !== 'sell') return;
        this.#setSide(side);
      });
    }
    const inputs: HTMLElement[] = [
      this.#qty,
      this.#kind,
      this.#price,
      this.#stopPrice,
      this.#tif,
      this.#postOnly,
      this.#day,
      this.#displayQty,
      this.#expiresMins,
    ];
    for (const input of inputs) {
      input.addEventListener('input', () => this.updateLabel());
    }
    this.#form.addEventListener('submit', (ev) => {
      ev.preventDefault();
      void this.#submitOrder();
    });
    store.on('symbols', () => this.#onSymbolChange());
    // A fill changes what is sellable, but must not clear a typed price.
    store.on('trader', () => this.#renderHolding());
  }

  /** Shares of the selected symbol this trader may still sell. */
  #freeShares(): number {
    const { trader, symbol } = this.#store.state;
    if (trader === null || symbol === null) return 0;
    return trader.positions.find((p) => p.symbol === symbol)?.free_shares ?? 0;
  }

  /** What the trader holds of the selected symbol, and the symbol's float. */
  #renderHolding(): void {
    const { trader, symbol } = this.#store.state;
    if (trader === null || symbol === null) {
      this.#holding.textContent = '';
      return;
    }
    const position = trader.positions.find((p) => p.symbol === symbol);
    const qty = position?.qty ?? 0;
    const free = position?.free_shares ?? 0;
    const parts = [`own ${qty}`, `sellable ${free}`];
    const quote = this.#store.currentQuote();
    if (quote !== null) {
      const unit = quote.unit === null ? 'shares' : `${quote.unit}`;
      parts.push(`of ${fmtVol(quote.shares_outstanding)} ${unit}`);
    }
    this.#holding.textContent = `${symbol}: ${parts.join(' · ')}`;
  }

  /** Pre-fill a limit at a price clicked in the ladder. */
  setLimitPrice(priceCents: number): void {
    // A click in the ladder means "trade here", so it leaves a stop ticket
    // as a stop and only fills in the limit the trigger would fire.
    if (this.#currentKind() !== 'stop_limit') this.#kind.value = 'limit';
    this.#price.disabled = false;
    this.#price.value = (priceCents / 100).toFixed(2);
    this.updateLabel();
  }

  #currentKind(): Kind {
    return isKind(this.#kind.value) ? this.#kind.value : 'market';
  }

  #currentTif(): TimeInForce {
    return isTif(this.#tif.value) ? this.#tif.value : 'gtc';
  }

  /** Why the market will take no order for the selected symbol, if it won't. */
  #stopped(): 'halted' | 'closed' | null {
    const quote = this.#store.currentQuote();
    if (quote === null) return null;
    if (quote.halted) return 'halted';
    return quote.market_open ? null : 'closed';
  }

  updateLabel(): void {
    this.#renderHolding();
    const kind = this.#currentKind();
    const wantsLimit = kind === 'limit' || kind === 'stop_limit';
    const trigger = kind === 'stop' || kind === 'stop_limit';
    this.#price.disabled = !wantsLimit;
    this.#stopPrice.disabled = !trigger;
    this.#gateOptions(kind);

    const side = this.#store.state.side;
    const qty = this.#qty.value === '' ? '?' : this.#qty.value;
    const limit = this.#price.value === '' ? 'limit' : `@ ${this.#price.value}`;
    const at = this.#stopPrice.value === '' ? 'stop' : `stop ${this.#stopPrice.value}`;
    const how =
      kind === 'market'
        ? 'at market'
        : kind === 'limit'
          ? limit
          : kind === 'stop'
            ? `${at} at market`
            : `${at} ${limit}`;
    // A halted or closed market takes no orders; the server refuses them too.
    // A stop is no exception: it is armed against a market that is running.
    const stopped = this.#stopped();
    this.#submit.disabled = stopped !== null;
    this.#submit.textContent =
      stopped === null
        ? `${side === 'buy' ? 'Buy' : 'Sell'} ${qty} ${how}`
        : stopped === 'halted'
          ? 'Trading halted'
          : 'Market closed';
    this.#submit.className = `submit full ${stopped === null ? side : 'stopped'}`;
  }

  /**
   * Turn off the options this ticket cannot carry, rather than letting the
   * server refuse them: post-only and an iceberg need a resting limit order,
   * so they need `gtc`; a day order and an expiry are two ways of saying the
   * same thing and the server takes one; a trigger carries none of them,
   * because the order it fires is written when it fires.
   */
  #gateOptions(kind: Kind): void {
    const trigger = kind === 'stop' || kind === 'stop_limit';
    const resting = kind === 'limit' && this.#currentTif() === 'gtc';
    this.#postOnly.disabled = !resting;
    this.#displayQty.disabled = !resting;
    this.#day.disabled = trigger || this.#expiresMins.value.trim() !== '';
    this.#expiresMins.disabled = trigger || this.#day.checked;
    for (const box of [this.#postOnly, this.#day]) {
      if (box.disabled) box.checked = false;
    }
    for (const field of [this.#displayQty, this.#expiresMins]) {
      if (field.disabled) field.value = '';
    }
  }

  #onSymbolChange(): void {
    this.#symbol.textContent = this.#store.state.symbol ?? '';
    // Prices belong to the symbol they were typed for; a client order id is
    // the caller's own and would collide with itself if it were kept.
    this.#price.value = '';
    this.#stopPrice.value = '';
    this.#clientOrderId.value = '';
    this.updateLabel();
  }

  #setSide(side: Side): void {
    this.#actions.setSide(side);
    for (const button of queryAll<HTMLButtonElement>(this.#form, '.side button')) {
      button.classList.toggle('active', button.dataset['side'] === side);
    }
    this.updateLabel();
  }

  async #submitOrder(): Promise<void> {
    const trader = this.#store.state.trader;
    if (trader === null) return;
    const stopped = this.#stopped();
    if (stopped !== null) {
      setOrderStatus(
        stopped === 'halted' ? 'trading in this symbol is halted' : 'the market is closed',
        true,
      );
      return;
    }
    const qty = Number.parseInt(this.#qty.value, 10);
    if (!Number.isSafeInteger(qty) || qty <= 0) return;
    const side = this.#store.state.side;
    // Nothing may be sold that is not owned. A stop reserves nothing, but the
    // server still checks the shares are there when it is armed — arming a
    // trigger that could only fail is not worth the trigger — so this applies
    // to a sell stop as much as to a sell order.
    const kind = this.#currentKind();
    const trigger = kind === 'stop' || kind === 'stop_limit';
    if (side === 'sell' && qty > this.#freeShares()) {
      setOrderStatus(`insufficient shares: ${this.#freeShares()} free to sell`, true);
      return;
    }
    const tif = this.#currentTif();
    const clientOrderId = this.#clientOrderId.value.trim();
    const base = { trader_id: trader.id, side, qty, tif };

    if (trigger) {
      await this.#submitStop(base, kind, clientOrderId);
      return;
    }

    let order: OrderRequest;
    if (kind === 'limit') {
      const priceCents = cents(this.#price);
      if (priceCents === null) return;
      order = { ...base, type: 'limit', price_cents: priceCents };
    } else {
      order = { ...base, type: 'market' };
    }
    if (clientOrderId !== '') order.client_order_id = clientOrderId;
    if (this.#postOnly.checked) order.post_only = true;
    if (this.#day.checked) order.day = true;

    const display = count(this.#displayQty);
    if (!this.#displayQty.disabled && this.#displayQty.value.trim() !== '') {
      if (display === null || display > qty) {
        setOrderStatus(`show qty must be between 1 and ${qty}`, true);
        return;
      }
      order.display_qty = display;
    }

    const expiry = this.#expiryMs();
    if (expiry === 'invalid') return;
    if (expiry !== null) order.expires_at_ms = expiry;

    await this.#actions.submitOrder(order);
  }

  /**
   * The simulated instant a resting remainder should be withdrawn at.
   *
   * The field asks for minutes because that is what a person means, and they
   * are *simulated* minutes: the server's clock runs at `FEHU_TIME_SCALE`, so
   * counting from the last instant seen is the only reading that agrees with
   * the one the server will check it against.
   */
  #expiryMs(): number | 'invalid' | null {
    if (this.#expiresMins.disabled || this.#expiresMins.value.trim() === '') return null;
    const mins = count(this.#expiresMins);
    const now = this.#store.state.simNow;
    if (mins === null) {
      setOrderStatus('expires in: whole minutes, at least one', true);
      return 'invalid';
    }
    if (now === null) {
      setOrderStatus('no simulated time seen yet: cannot date an expiry', true);
      return 'invalid';
    }
    return now + mins * 60_000;
  }

  /** Arm a trigger rather than send an order. */
  async #submitStop(
    base: { trader_id: number; side: Side; qty: number; tif: TimeInForce },
    kind: Kind,
    clientOrderId: string,
  ): Promise<void> {
    const stopPriceCents = cents(this.#stopPrice);
    if (stopPriceCents === null || stopPriceCents <= 0) {
      setOrderStatus('a stop needs a trigger price', true);
      return;
    }
    // A trigger the market has already passed would fire on arrival, which
    // is a market order with extra steps. The server refuses it; say so here.
    const quote = this.#store.currentQuote();
    if (quote !== null) {
      const behind =
        base.side === 'buy'
          ? stopPriceCents <= quote.price_cents
          : stopPriceCents >= quote.price_cents;
      if (behind) {
        const where = base.side === 'buy' ? 'above' : 'below';
        setOrderStatus(
          `a ${base.side} stop must trigger ${where} the market, which is at ` +
            `${fmtPrice(quote.price_cents)}`,
          true,
        );
        return;
      }
    }
    const stop: StopRequest = { ...base, stop_price_cents: stopPriceCents };
    if (kind === 'stop_limit') {
      const limitCents = cents(this.#price);
      if (limitCents === null) {
        setOrderStatus('a stop limit needs the limit its order will carry', true);
        return;
      }
      stop.limit_price_cents = limitCents;
    }
    if (clientOrderId !== '') stop.client_order_id = clientOrderId;
    await this.#actions.placeStop(stop);
  }
}
