/** Display helpers. Prices are integer cents everywhere in the API. */

const priceFormat = new Intl.NumberFormat(undefined, {
  minimumFractionDigits: 2,
  maximumFractionDigits: 2,
});

/** `123456` → `"1,234.56"`. */
export function fmtPrice(cents: number): string {
  return priceFormat.format(cents / 100);
}

/** Signed price, for P&L: `"+12.34"` / `"-12.34"`. */
export function fmtSignedPrice(cents: number): string {
  return (cents >= 0 ? '+' : '') + fmtPrice(cents);
}

/** Compact share counts: `1_250_000` → `"1.25M"`. */
export function fmtVol(v: number): string {
  if (v >= 1e9) return `${(v / 1e9).toFixed(2)}B`;
  if (v >= 1e6) return `${(v / 1e6).toFixed(2)}M`;
  if (v >= 1e3) return `${(v / 1e3).toFixed(1)}K`;
  return String(v);
}

export function fmtPct(pct: number, digits = 2): string {
  return `${pct >= 0 ? '+' : ''}${pct.toFixed(digits)}%`;
}

export function pad2(n: number): string {
  return String(n).padStart(2, '0');
}

/**
 * Simulated timestamps are always rendered in UTC: the market calendar the
 * simulator runs on is UTC, so a local-time axis would misplace the session.
 */
export function fmtTs(ms: number, withDate = false): string {
  const d = new Date(ms);
  const time = `${pad2(d.getUTCHours())}:${pad2(d.getUTCMinutes())}:${pad2(d.getUTCSeconds())}`;
  if (!withDate) return time;
  const date = `${d.getUTCFullYear()}-${pad2(d.getUTCMonth() + 1)}-${pad2(d.getUTCDate())}`;
  return `${date} ${time}`;
}

/** Class name for a number that reads as up or down. */
export function trendClass(n: number | null | undefined): '' | 'up' | 'down' {
  if (n == null) return '';
  return n >= 0 ? 'up' : 'down';
}
