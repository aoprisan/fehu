/**
 * The price chart: candles, a volume histogram, event markers, the trader's
 * own fills and a hover crosshair, drawn on a 2-D canvas.
 *
 * Redraws are coalesced into one animation frame, so a burst of stream
 * messages costs a single paint.
 */

import { byId } from '../dom.js';
import { fmtPrice, fmtTs, fmtVol, pad2 } from '../format.js';
import { INTERVAL_MS, bucketOf } from '../intervals.js';
import type { Store } from '../store.js';
import { readTheme, type ChartTheme } from '../theme.js';
import type { Candle, EventRecord, FillRecord } from '../types.js';

import { PADDING, Projection, computeLayout, niceStep, priceRange, type Layout } from './layout.js';

/** Minimum pixels between two time-axis labels. */
const LABEL_SPACING = 80;

/** Share of the pitch a candle body occupies. */
const BODY_WIDTH = 0.66;

/** Below this body width only the wick is drawn. */
const MIN_BODY_WIDTH = 3;

export interface ChartCallbacks {
  onHover(index: number | null): void;
  onZoom(pitch: number): void;
}

export class Chart {
  readonly #store: Store;
  readonly #callbacks: ChartCallbacks;
  readonly #wrap: HTMLElement;
  readonly #canvas: HTMLCanvasElement;
  readonly #tip: HTMLElement;
  readonly #ohlc: HTMLElement;
  #theme: ChartTheme;
  #frame: number | null = null;
  #layout: Layout | null = null;

  constructor(store: Store, callbacks: ChartCallbacks) {
    this.#store = store;
    this.#callbacks = callbacks;
    this.#wrap = byId('chart-wrap');
    this.#canvas = byId('chart', HTMLCanvasElement);
    this.#tip = byId('tip');
    this.#ohlc = byId('ohlc');
    this.#theme = readTheme();
    this.#bindPointer();
    new ResizeObserver(() => this.schedule()).observe(this.#wrap);
  }

  /** Request a redraw on the next frame. Safe to call many times per frame. */
  schedule(): void {
    if (this.#frame !== null) return;
    this.#frame = requestAnimationFrame(() => {
      this.#frame = null;
      this.#draw();
    });
  }

  /** Re-read the CSS custom properties, e.g. after a theme change. */
  refreshTheme(): void {
    this.#theme = readTheme();
    this.schedule();
  }

  // --- input ---------------------------------------------------------------

  #bindPointer(): void {
    this.#wrap.addEventListener('mousemove', (ev) => this.#onMove(ev));
    this.#wrap.addEventListener('mouseleave', () => {
      this.#tip.style.display = 'none';
      this.#callbacks.onHover(null);
    });
    this.#wrap.addEventListener(
      'wheel',
      (ev) => {
        ev.preventDefault();
        const factor = ev.deltaY < 0 ? 1.15 : 0.87;
        this.#callbacks.onZoom(this.#store.state.pitch * factor);
      },
      { passive: false },
    );
  }

  #onMove(ev: MouseEvent): void {
    const layout = this.#layout;
    if (layout === null) return;
    const rect = this.#wrap.getBoundingClientRect();
    const state = this.#store.state;
    const projection = new Projection(priceRange(layout.bars), layout.priceHeight, state.pitch);
    const index = projection.indexAt(ev.clientX - rect.left);
    const bar = index >= 0 && index < layout.bars.length ? layout.bars[index] : undefined;
    this.#callbacks.onHover(bar === undefined ? null : index);

    const hits = bar === undefined ? [] : this.#eventsIn(bar);
    if (hits.length === 0) {
      this.#tip.style.display = 'none';
      return;
    }
    this.#tip.replaceChildren(
      ...hits.map((e) => {
        const line = document.createElement('div');
        const strong = document.createElement('b');
        strong.textContent = `#${e.id} ${e.kind} `;
        line.append(strong, e.summary.join(' · '));
        return line;
      }),
    );
    this.#tip.style.display = 'block';
    this.#positionTip(ev.clientX - rect.left, ev.clientY - rect.top, rect);
  }

  /** Keep the tooltip inside the chart, flipping it around the cursor. */
  #positionTip(x: number, y: number, rect: DOMRect): void {
    const gap = 12;
    const { offsetWidth: width, offsetHeight: height } = this.#tip;
    const left = x + gap + width > rect.width ? Math.max(0, x - gap - width) : x + gap;
    const top = y + gap + height > rect.height ? Math.max(0, y - gap - height) : y + gap;
    this.#tip.style.left = `${left}px`;
    this.#tip.style.top = `${top}px`;
  }

  #eventsIn(bar: Candle): EventRecord[] {
    const { events, symbol, interval } = this.#store.state;
    if (symbol === null) return [];
    return events.filter(
      (e) => e.symbols.includes(symbol) && bucketOf(e.at_ms, interval) === bar.open_ts,
    );
  }

  // --- drawing -------------------------------------------------------------

  #draw(): void {
    const state = this.#store.state;
    const width = this.#wrap.clientWidth;
    const height = this.#wrap.clientHeight;
    const layout = computeLayout(width, height, state.pitch, state.bars);
    this.#layout = layout;

    const dpr = window.devicePixelRatio || 1;
    const pixelWidth = Math.round(width * dpr);
    const pixelHeight = Math.round(height * dpr);
    if (this.#canvas.width !== pixelWidth || this.#canvas.height !== pixelHeight) {
      this.#canvas.width = pixelWidth;
      this.#canvas.height = pixelHeight;
    }
    const ctx = this.#canvas.getContext('2d');
    if (ctx === null) return;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, width, height);
    ctx.font = '11px system-ui, sans-serif';
    ctx.textBaseline = 'middle';

    const bars = layout.bars;
    if (bars.length === 0) {
      ctx.fillStyle = this.#theme.muted;
      ctx.fillText(state.barsLoading ? 'loading…' : 'no bars', PADDING.left + 8, PADDING.top + 20);
      this.#ohlc.replaceChildren();
      return;
    }

    const range = priceRange(bars);
    const projection = new Projection(range, layout.priceHeight, state.pitch);
    this.#drawPriceGrid(ctx, layout, projection, range.lo, range.hi);
    this.#drawTimeAxis(ctx, layout, projection);
    this.#drawVolume(ctx, layout, projection, range.maxVolume);
    this.#drawCandles(ctx, layout, projection);
    this.#drawEventMarkers(ctx, layout, projection);
    this.#drawFills(ctx, layout, projection);
    this.#drawLastPrice(ctx, layout, projection);
    this.#drawCrosshair(ctx, layout, projection);
    this.#renderReadout(layout);
  }

  #drawPriceGrid(
    ctx: CanvasRenderingContext2D,
    layout: Layout,
    projection: Projection,
    lo: number,
    hi: number,
  ): void {
    const step = niceStep(hi - lo, 6);
    ctx.strokeStyle = this.#theme.grid;
    ctx.fillStyle = this.#theme.muted;
    ctx.lineWidth = 1;
    ctx.textAlign = 'left';
    for (let price = Math.ceil(lo / step) * step; price <= hi; price += step) {
      const y = Math.round(projection.y(price)) + 0.5;
      ctx.beginPath();
      ctx.moveTo(PADDING.left, y);
      ctx.lineTo(PADDING.left + layout.plotWidth, y);
      ctx.stroke();
      ctx.fillText(fmtPrice(price), PADDING.left + layout.plotWidth + 6, y);
    }
  }

  /**
   * Time labels sit at least `LABEL_SPACING` apart, and every day boundary
   * (month boundary on the daily chart) is labelled and drawn brighter.
   */
  #drawTimeAxis(ctx: CanvasRenderingContext2D, layout: Layout, projection: Projection): void {
    const { interval, pitch } = this.#store.state;
    const daily = INTERVAL_MS[interval] >= INTERVAL_MS.D1;
    const bars = layout.bars;

    const marks: Array<{ index: number; boundary: boolean }> = [];
    let previousKey: number | null = null;
    for (const [index, bar] of bars.entries()) {
      const d = new Date(bar.open_ts);
      const key = daily ? d.getUTCMonth() : d.getUTCDate();
      if (previousKey !== null && key !== previousKey) marks.push({ index, boundary: true });
      previousKey = key;
    }
    const every = Math.max(1, Math.ceil(LABEL_SPACING / pitch));
    for (let i = 0; i < bars.length; i += every) {
      if (!marks.some((m) => Math.abs(m.index - i) < every)) marks.push({ index: i, boundary: false });
    }
    marks.sort((a, b) => a.index - b.index);

    ctx.textAlign = 'center';
    for (const { index, boundary } of marks) {
      const bar = bars[index];
      if (bar === undefined) continue;
      const d = new Date(bar.open_ts);
      const x = Math.round(projection.x(index)) + 0.5;
      ctx.strokeStyle = boundary ? this.#theme.gridStrong : this.#theme.gridFaint;
      ctx.beginPath();
      ctx.moveTo(x, PADDING.top);
      ctx.lineTo(x, layout.volumeTop + layout.volumeHeight);
      ctx.stroke();
      const monthDay = `${pad2(d.getUTCMonth() + 1)}-${pad2(d.getUTCDate())}`;
      const label = daily
        ? boundary
          ? `${d.getUTCFullYear()}-${pad2(d.getUTCMonth() + 1)}`
          : monthDay
        : boundary
          ? monthDay
          : `${pad2(d.getUTCHours())}:${pad2(d.getUTCMinutes())}`;
      ctx.fillStyle = boundary ? this.#theme.text : this.#theme.muted;
      ctx.fillText(label, x, layout.height - 10);
    }
  }

  #drawVolume(
    ctx: CanvasRenderingContext2D,
    layout: Layout,
    projection: Projection,
    maxVolume: number,
  ): void {
    const bodyWidth = Math.max(1, this.#store.state.pitch * BODY_WIDTH);
    const base = layout.volumeTop + layout.volumeHeight;
    for (const [index, bar] of layout.bars.entries()) {
      const h = (bar.volume / maxVolume) * layout.volumeHeight;
      ctx.fillStyle = bar.close >= bar.open ? this.#theme.upFill : this.#theme.downFill;
      ctx.fillRect(projection.x(index) - bodyWidth / 2, base - h, bodyWidth, h);
    }
  }

  #drawCandles(ctx: CanvasRenderingContext2D, layout: Layout, projection: Projection): void {
    const bodyWidth = Math.max(1, this.#store.state.pitch * BODY_WIDTH);
    for (const [index, bar] of layout.bars.entries()) {
      const colour = bar.close >= bar.open ? this.#theme.up : this.#theme.down;
      const x = Math.round(projection.x(index)) + 0.5;
      ctx.strokeStyle = colour;
      ctx.fillStyle = colour;
      ctx.beginPath();
      ctx.moveTo(x, projection.y(bar.high));
      ctx.lineTo(x, projection.y(bar.low));
      ctx.stroke();
      if (bodyWidth < MIN_BODY_WIDTH) continue;
      const top = projection.y(Math.max(bar.open, bar.close));
      const bottom = projection.y(Math.min(bar.open, bar.close));
      ctx.fillRect(x - bodyWidth / 2, top, bodyWidth, Math.max(1, bottom - top));
    }
  }

  /** Index of the bar a timestamp falls in, or `null` if it is off-window. */
  #indexOf(tsMs: number, bars: readonly Candle[]): number | null {
    const interval = this.#store.state.interval;
    const first = bars[0];
    const last = bars.at(-1);
    if (first === undefined || last === undefined) return null;
    const bucket = bucketOf(tsMs, interval);
    if (bucket < first.open_ts || bucket > last.open_ts) return null;
    const index = Math.round((bucket - first.open_ts) / INTERVAL_MS[interval]);
    // Sessions have gaps, so the arithmetic index can miss; verify it.
    return bars[index]?.open_ts === bucket ? index : null;
  }

  #drawEventMarkers(ctx: CanvasRenderingContext2D, layout: Layout, projection: Projection): void {
    const { events, symbol } = this.#store.state;
    if (symbol === null) return;
    for (const e of events) {
      if (!e.symbols.includes(symbol)) continue;
      const index = this.#indexOf(e.at_ms, layout.bars);
      if (index === null) continue;
      const x = projection.x(index);
      ctx.fillStyle = e.kind.startsWith('game:') ? this.#theme.amber : this.#theme.accent;
      ctx.beginPath();
      ctx.moveTo(x, PADDING.top + 2);
      ctx.lineTo(x - 5, PADDING.top - 6);
      ctx.lineTo(x + 5, PADDING.top - 6);
      ctx.closePath();
      ctx.fill();
    }
  }

  /** The trader's own executions: the portfolio's history plus live fills. */
  #drawFills(ctx: CanvasRenderingContext2D, layout: Layout, projection: Projection): void {
    const state = this.#store.state;
    if (state.symbol === null) return;
    const fills: FillRecord[] = [...(state.trader?.fills ?? []), ...state.liveFills];
    const seen = new Set<string>();
    for (const fill of fills) {
      if (fill.symbol !== state.symbol) continue;
      const key = `${fill.id}:${fill.ts_ms}`;
      if (seen.has(key)) continue;
      seen.add(key);
      const index = this.#indexOf(fill.ts_ms, layout.bars);
      if (index === null) continue;
      const x = projection.x(index);
      const y = projection.y(fill.price_cents);
      const buy = fill.side === 'buy';
      ctx.fillStyle = buy ? this.#theme.up : this.#theme.down;
      ctx.strokeStyle = this.#theme.backdrop;
      ctx.lineWidth = 1;
      ctx.beginPath();
      if (buy) {
        ctx.moveTo(x, y - 4);
        ctx.lineTo(x - 4, y + 3);
        ctx.lineTo(x + 4, y + 3);
      } else {
        ctx.moveTo(x, y + 4);
        ctx.lineTo(x - 4, y - 3);
        ctx.lineTo(x + 4, y - 3);
      }
      ctx.closePath();
      ctx.fill();
      ctx.stroke();
    }
  }

  #drawLastPrice(ctx: CanvasRenderingContext2D, layout: Layout, projection: Projection): void {
    const last = layout.bars.at(-1);
    if (last === undefined) return;
    const y = Math.round(projection.y(last.close)) + 0.5;
    const colour = last.close >= last.open ? this.#theme.up : this.#theme.down;
    ctx.setLineDash([3, 3]);
    ctx.strokeStyle = colour;
    ctx.beginPath();
    ctx.moveTo(PADDING.left, y);
    ctx.lineTo(PADDING.left + layout.plotWidth, y);
    ctx.stroke();
    ctx.setLineDash([]);
    ctx.fillStyle = colour;
    ctx.fillRect(PADDING.left + layout.plotWidth + 2, y - 8, 62, 16);
    ctx.fillStyle = this.#theme.backdrop;
    ctx.textAlign = 'left';
    ctx.fillText(fmtPrice(last.close), PADDING.left + layout.plotWidth + 6, y);
  }

  #drawCrosshair(ctx: CanvasRenderingContext2D, layout: Layout, projection: Projection): void {
    const hover = this.#store.state.hover;
    if (hover === null || hover >= layout.bars.length) return;
    const x = Math.round(projection.x(hover)) + 0.5;
    ctx.strokeStyle = `${this.#theme.accent}88`;
    ctx.setLineDash([4, 3]);
    ctx.beginPath();
    ctx.moveTo(x, PADDING.top);
    ctx.lineTo(x, layout.volumeTop + layout.volumeHeight);
    ctx.stroke();
    ctx.setLineDash([]);
  }

  /** The OHLC line in the toolbar: the hovered bar, or the last one. */
  #renderReadout(layout: Layout): void {
    const hover = this.#store.state.hover;
    const bar =
      hover !== null && hover < layout.bars.length ? layout.bars[hover] : layout.bars.at(-1);
    if (bar === undefined) return;
    const change = (bar.close / bar.open - 1) * 100;
    const changeEl = document.createElement('span');
    changeEl.className = change >= 0 ? 'up' : 'down';
    changeEl.textContent = `${change >= 0 ? '+' : ''}${change.toFixed(2)}%`;
    this.#ohlc.replaceChildren(
      `${fmtTs(bar.open_ts, true)}  O ${fmtPrice(bar.open)} H ${fmtPrice(bar.high)} ` +
        `L ${fmtPrice(bar.low)} C ${fmtPrice(bar.close)} `,
      changeEl,
      `  V ${fmtVol(bar.volume)}${bar.ticks > 0 ? ` · ${bar.ticks} ticks` : ' · coarse'}`,
    );
  }
}
