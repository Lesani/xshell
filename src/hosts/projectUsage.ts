import { AGENTS } from "../agents";
import type { SessionInfo } from "../types";

export type SeriesPoint = { date: string; values: number[] };

// Pi costs are recorded in session usage, not Claude hook authority. Never estimate them.
export const hasRecordedCost = (s: SessionInfo): boolean => s.agent === "pi" || s.is_authoritative_stats;

export function projectUsage(sessions: SessionInfo[]) {
  const costSessions = sessions.filter(hasRecordedCost);
  const tokenTotals = sessions.reduce((a, s) => {
    a[0] += s.total_input_tokens || 0;
    a[1] += s.total_cache_creation_tokens || 0;
    a[2] += s.total_cache_read_tokens || 0;
    a[3] += s.total_output_tokens || 0;
    return a;
  }, [0, 0, 0, 0]);
  const sources = [...new Set(costSessions.filter(s => s.cost_usd > 0 || Object.values(s.daily_cost).some(v => v > 0)).map(s => AGENTS[s.agent].label))];
  return {
    totalCost: costSessions.reduce((n, s) => n + s.cost_usd, 0),
    costSessionCount: costSessions.filter(s => s.cost_usd > 0).length,
    tokenTotals,
    totalTokens: tokenTotals.reduce((a, b) => a + b, 0),
    costLabel: sources.length ? `Recorded cost (${sources.join(" + ")})` : "Recorded cost",
    hasCostSource: sessions.some(s => s.agent === "pi" || (s.agent === "claude" && s.is_authoritative_stats)),
    offersCost: sessions.some(s => s.agent === "pi" || s.agent === "claude"),
  };
}

export function dailyCostSeries(sessions: SessionInfo[], days: number): SeriesPoint[] {
  const byDate = new Map<string, number>();
  for (const s of sessions) {
    if (!hasRecordedCost(s) || !s.daily_cost) continue;
    for (const [date, usd] of Object.entries(s.daily_cost)) {
      byDate.set(date, (byDate.get(date) || 0) + usd);
    }
  }
  return paddedSeries(byDate, days, (v) => [v ?? 0]);
}

// Same shape as dailyCostSeries but with four token bands per day. Source is the JSONL-derived
// daily_tokens map on each session — independent of the xshell-stats hook, so it works even
// for users without the hook installed. Reorder Rust's [input, cache_creation, cache_read,
// output] tuple into [cache_read, cache_creation, input, output] for visual cost-impact order.
export function dailyTokensSeries(sessions: SessionInfo[], days: number): SeriesPoint[] {
  const byDate = new Map<string, [number, number, number, number]>();
  for (const s of sessions) {
    if (!s.daily_tokens) continue;
    for (const [date, t] of Object.entries(s.daily_tokens)) {
      const cur = byDate.get(date) || [0, 0, 0, 0];
      // Rust order: [input, cache_creation, cache_read, output] →
      // chart order: [cache_read, cache_creation, input, output]
      cur[0] += t[2] || 0;
      cur[1] += t[1] || 0;
      cur[2] += t[0] || 0;
      cur[3] += t[3] || 0;
      byDate.set(date, cur);
    }
  }
  return paddedSeries(byDate, days, (v) => v ?? [0, 0, 0, 0]);
}

// Common pad-with-zeros loop shared by both daily series.
function paddedSeries<T>(byDate: Map<string, T>, days: number, fill: (v: T | undefined) => number[]): SeriesPoint[] {
  const out: SeriesPoint[] = [];
  const today = new Date();
  today.setUTCHours(0, 0, 0, 0);
  for (let i = days - 1; i >= 0; i--) {
    const d = new Date(today);
    d.setUTCDate(today.getUTCDate() - i);
    const key = d.toISOString().slice(0, 10);
    out.push({ date: key, values: fill(byDate.get(key)) });
  }
  return out;
}
