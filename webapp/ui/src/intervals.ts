import type { Interval } from './types.js';

/** Length of each candle interval in milliseconds. */
export const INTERVAL_MS: Readonly<Record<Interval, number>> = {
  M1: 60_000,
  M5: 300_000,
  H1: 3_600_000,
  D1: 86_400_000,
};

/** Intervals in the order the toolbar offers them, with their button labels. */
export const INTERVALS: ReadonlyArray<{ interval: Interval; label: string }> = [
  { interval: 'M1', label: '1m' },
  { interval: 'M5', label: '5m' },
  { interval: 'H1', label: '1h' },
  { interval: 'D1', label: '1d' },
];

export function isInterval(s: string): s is Interval {
  return s in INTERVAL_MS;
}

/**
 * Start of the bar `ts` falls into. Uses a floored modulo so pre-epoch
 * timestamps bucket downwards too, matching the server's aggregation.
 */
export function bucketOf(ts: number, interval: Interval): number {
  const ms = INTERVAL_MS[interval];
  return ts - (((ts % ms) + ms) % ms);
}
