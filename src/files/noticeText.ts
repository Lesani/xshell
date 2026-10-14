import { S, type StringKey } from "../hosts/strings";

// Explorer notices stay within NOTICE_MAX characters (binding copy note: bound or abbreviate
// interpolated names, paths and errors; keep the full text in the tooltip).
export const NOTICE_MAX = 140;

const clip = (s: string, n: number) => (s.length <= n ? s : n <= 1 ? "…" : `${s.slice(0, n - 1)}…`);

// Fill `key`'s template so the result is at most `max` characters. Numbers are kept whole;
// strings share what the template's own text leaves: each gets an equal share, and a string
// shorter than its share hands the rest to the others. Returns the shown text and the full one.
export function fitNotice(key: StringKey, params: Record<string, string | number>, max = NOTICE_MAX): { text: string; full: string } {
  const template: string = S[key];
  const fill = (p: Record<string, string | number>) =>
    template.replace(/\{(\w+)\}/g, (m, name: string) => (name in p ? String(p[name]) : m));
  const full = fill(params);
  if (full.length <= max) return { text: full, full };
  // Placeholders used, with how often each appears.
  const uses = new Map<string, number>();
  for (const m of template.matchAll(/\{(\w+)\}/g)) if (m[1] in params) uses.set(m[1], (uses.get(m[1]) ?? 0) + 1);
  let budget = max - template.replace(/\{(\w+)\}/g, (m, name: string) => (name in params ? "" : m)).length;
  const strings: string[] = [];
  for (const [name, n] of uses) {
    const v = params[name];
    if (typeof v === "number") budget -= String(v).length * n;
    else strings.push(name);
  }
  const limits: Record<string, number> = {};
  let open = [...strings].sort((a, b) => String(params[a]).length - String(params[b]).length);
  while (open.length > 0) {
    const weight = open.reduce((n, name) => n + (uses.get(name) ?? 1), 0);
    const share = Math.max(1, Math.floor(budget / Math.max(1, weight)));
    const first = open[0];
    const len = String(params[first]).length;
    if (len <= share) {
      limits[first] = len;
      budget -= len * (uses.get(first) ?? 1);
      open = open.slice(1);
      continue;
    }
    for (const name of open) limits[name] = share;
    break;
  }
  const shown: Record<string, string | number> = { ...params };
  for (const name of strings) shown[name] = clip(String(params[name]), limits[name]);
  return { text: fill(shown), full };
}
