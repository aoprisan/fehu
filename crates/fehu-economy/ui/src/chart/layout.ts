/** Pure geometry for the price chart. No DOM, no state — easy to reason about. */

import type { Candle } from '../types.js';

export interface Padding {
  readonly left: number;
  readonly right: number;
  readonly top: number;
  readonly bottom: number;
}

/** Room for the price axis on the right and the time axis at the bottom. */
export const PADDING: Padding = { left: 8, right: 68, top: 14, bottom: 22 };

/** Share of the plot height given to the volume histogram. */
const VOLUME_SHARE = 0.18;

/** Gap between the candle pane and the volume pane. */
const PANE_GAP = 8;

/** Vertical head-room above the highest high and below the lowest low. */
const PRICE_PADDING = 0.06;

export interface Layout {
  readonly width: number;
  readonly height: number;
  readonly plotWidth: number;
  readonly priceHeight: number;
  readonly volumeHeight: number;
  /** Top of the volume pane. */
  readonly volumeTop: number;
  /** The tail of the series that fits, oldest first. */
  readonly bars: readonly Candle[];
  /** Index in `bars` of the first drawn bar within the full series. */
  readonly offset: number;
}

/** Lay out the panes and pick the window of bars that fits at this pitch. */
export function computeLayout(
  width: number,
  height: number,
  pitch: number,
  bars: readonly Candle[],
): Layout {
  const plotWidth = width - PADDING.left - PADDING.right;
  const inner = height - PADDING.top - PADDING.bottom;
  const volumeHeight = Math.max(0, Math.floor(inner * VOLUME_SHARE));
  const priceHeight = Math.max(1, inner - volumeHeight - PANE_GAP);
  const visible = Math.max(1, Math.floor(plotWidth / pitch));
  const offset = Math.max(0, bars.length - visible);
  return {
    width,
    height,
    plotWidth,
    priceHeight,
    volumeHeight,
    volumeTop: PADDING.top + priceHeight + PANE_GAP,
    bars: bars.slice(offset),
    offset,
  };
}

export interface PriceRange {
  readonly lo: number;
  readonly hi: number;
  readonly maxVolume: number;
}

/** Price extent of the window, padded, plus the volume peak to scale bars by. */
export function priceRange(bars: readonly Candle[]): PriceRange {
  let lo = Infinity;
  let hi = -Infinity;
  let maxVolume = 1;
  for (const b of bars) {
    if (b.low < lo) lo = b.low;
    if (b.high > hi) hi = b.high;
    if (b.volume > maxVolume) maxVolume = b.volume;
  }
  if (!Number.isFinite(lo) || !Number.isFinite(hi)) return { lo: 0, hi: 1, maxVolume };
  const pad = Math.max(1, (hi - lo) * PRICE_PADDING);
  return { lo: lo - pad, hi: hi + pad, maxVolume };
}

/** A 1-2-5-10 grid step that gives roughly `count` lines over `range`. */
export function niceStep(range: number, count: number): number {
  const raw = range / Math.max(1, count);
  if (!(raw > 0)) return 1;
  const magnitude = 10 ** Math.floor(Math.log10(raw));
  const r = raw / magnitude;
  const factor = r >= 5 ? 10 : r >= 2 ? 5 : r >= 1 ? 2 : 1;
  return factor * magnitude;
}

/** Maps a bar index and a price onto canvas coordinates. */
export class Projection {
  readonly #lo: number;
  readonly #span: number;
  readonly #priceHeight: number;
  readonly #pitch: number;

  constructor(range: PriceRange, priceHeight: number, pitch: number) {
    this.#lo = range.lo;
    this.#span = Math.max(1e-9, range.hi - range.lo);
    this.#priceHeight = priceHeight;
    this.#pitch = pitch;
  }

  /** Canvas y for a price in cents. */
  y(priceCents: number): number {
    return PADDING.top + ((this.#lo + this.#span - priceCents) / this.#span) * this.#priceHeight;
  }

  /** Canvas x for the centre of the i-th drawn bar. */
  x(index: number): number {
    return PADDING.left + index * this.#pitch + this.#pitch / 2;
  }

  /** Index of the bar under a canvas x, which may be outside the window. */
  indexAt(x: number): number {
    return Math.floor((x - PADDING.left) / this.#pitch);
  }
}
