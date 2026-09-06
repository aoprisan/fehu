/**
 * Canvas has no cascade, so the chart reads the same custom properties the
 * CSS uses. Colours stay defined in one place: `styles.css`.
 */

export interface ChartTheme {
  up: string;
  down: string;
  upFill: string;
  downFill: string;
  grid: string;
  gridFaint: string;
  gridStrong: string;
  text: string;
  muted: string;
  accent: string;
  amber: string;
  backdrop: string;
}

const VARS: Readonly<Record<keyof ChartTheme, [name: string, fallback: string]>> = {
  up: ['--up', '#3ddc97'],
  down: ['--down', '#ff6b6b'],
  upFill: ['--up-fill', 'rgba(61, 220, 151, 0.35)'],
  downFill: ['--down-fill', 'rgba(255, 107, 107, 0.35)'],
  grid: ['--line', '#242b38'],
  gridFaint: ['--line-faint', '#1c2230'],
  gridStrong: ['--line-strong', '#2f3848'],
  text: ['--text', '#d7dce5'],
  muted: ['--muted', '#7d8797'],
  accent: ['--accent', '#6ea8fe'],
  amber: ['--amber', '#f5b942'],
  backdrop: ['--backdrop', '#0b0f16'],
};

export function readTheme(root: Element = document.documentElement): ChartTheme {
  const style = getComputedStyle(root);
  const theme = {} as ChartTheme;
  for (const [key, [name, fallback]] of Object.entries(VARS) as Array<
    [keyof ChartTheme, [string, string]]
  >) {
    theme[key] = style.getPropertyValue(name).trim() || fallback;
  }
  return theme;
}
