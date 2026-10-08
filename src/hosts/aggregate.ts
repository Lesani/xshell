import type { ClaudeCostSummary, CodexUsage, GlobalRateLimits, SessionInfo } from "../types";
import { hostInvokeLive } from "./hostInvoke";
import type { HostMethod } from "./methods";
import { registry } from "./registry";
import { usableHosts } from "./useHosts";
import type { HostId } from "./types";

// Account-wide widgets across Hosts (same account everywhere): rate limits show the freshest
// reading, cost sums the Hosts. Only live results count — never cached ones (amendment 24).
// With a single source every function returns it unchanged, so a Desktop with no Remote Hosts
// renders exactly as before.

export interface Sourced<T> { host?: HostId; value: T }

// Local + every usable Remote Host; failures are skipped.
export async function fanOutSourced<T>(method: HostMethod): Promise<Sourced<T>[]> {
  const hosts: (HostId | undefined)[] = [undefined, ...usableHosts(registry.getSnapshot())];
  const results = await Promise.all(hosts.map(async (host): Promise<Sourced<T> | null> => {
    try { return { host, value: await hostInvokeLive<T>(host, method) }; } catch (_) { return null; }
  }));
  return results.filter((r): r is Sourced<T> => r !== null);
}

export async function fanOut<T>(method: HostMethod): Promise<T[]> {
  return (await fanOutSourced<T>(method)).map(r => r.value);
}

const isoTime = (iso: string | null | undefined): number => {
  if (!iso) return Number.NEGATIVE_INFINITY;
  const t = new Date(iso).getTime();
  return Number.isNaN(t) ? Number.NEGATIVE_INFINITY : t;
};

// The reading with the newest `last_update_iso` (readings without one lose to any that has one).
export function freshestRateLimits(xs: GlobalRateLimits[]): GlobalRateLimits | null {
  if (xs.length === 0) return null;
  let best = xs[0];
  for (const x of xs.slice(1)) if (isoTime(x.last_update_iso) > isoTime(best.last_update_iso)) best = x;
  return best;
}

function sumByDate<T extends { date: string }>(lists: T[][], value: (x: T) => number, make: (date: string, v: number) => T): T[] {
  const m = new Map<string, number>();
  for (const l of lists) for (const x of l) m.set(x.date, (m.get(x.date) ?? 0) + value(x));
  return [...m.entries()].sort((a, b) => a[0].localeCompare(b[0])).map(([d, v]) => make(d, v));
}

// connected = any Host connected; daily cost summed per date, ascending.
export function sumCostSummaries(xs: ClaudeCostSummary[]): ClaudeCostSummary {
  if (xs.length === 1) return xs[0];
  return {
    connected: xs.some(x => x.connected),
    daily: sumByDate(xs.map(x => x.daily), d => d.usd, (date, usd) => ({ date, usd })),
  };
}

// Rate windows from the Host with the newest `rate_limits_updated_iso`; sessions per day summed.
export function mergeCodexUsage(xs: CodexUsage[]): CodexUsage {
  if (xs.length === 1) return xs[0];
  if (xs.length === 0) return { present: false, primary: null, secondary: null, plan_type: null, rate_limits_updated_iso: null, daily_sessions: [] };
  let best = xs[0];
  for (const x of xs.slice(1)) if (isoTime(x.rate_limits_updated_iso) > isoTime(best.rate_limits_updated_iso)) best = x;
  return {
    present: xs.some(x => x.present),
    primary: best.primary,
    secondary: best.secondary,
    plan_type: best.plan_type,
    rate_limits_updated_iso: best.rate_limits_updated_iso,
    daily_sessions: sumByDate(xs.map(x => x.daily_sessions), d => d.count, (date, count) => ({ date, count })),
  };
}

// Merge several Hosts' recent-session lists (Local first): newest first, capped. When only
// the first list has entries it is returned as-is, so a Desktop with no Remote Hosts keeps
// exactly the Local order.
export function mergeRecent(lists: SessionInfo[][], limit = 100): SessionInfo[] {
  if (lists.length === 0) return [];
  const [first, ...rest] = lists;
  if (rest.every(l => l.length === 0)) return first;
  return lists.flat().sort((a, b) => b.timestamp.localeCompare(a.timestamp)).slice(0, limit);
}

// Sessions for the usage counters: this computer plus only those Remote Hosts whose list was
// fetched live and that are usable now — cached lists stay browsable but are not counted
// (amendment 24). With no Remote Hosts this is the Local list itself.
export function liveRecentSessions(byHost: Record<string, SessionInfo[]>, isLive: (host: HostId) => boolean, limit = 100): SessionInfo[] {
  const remote = Object.entries(byHost).filter(([k]) => k !== "local" && isLive(k));
  return mergeRecent([byHost.local ?? [], ...remote.map(([, v]) => v)], limit);
}
