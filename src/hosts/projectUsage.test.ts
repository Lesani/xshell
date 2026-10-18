import { afterEach, expect, it, vi } from "vitest";
import { dailyCostSeries, dailyTokensSeries, hasRecordedCost, projectUsage } from "./projectUsage";
import type { SessionInfo } from "../types";

const session = (over: Partial<SessionInfo> = {}): SessionInfo => ({
  id: "custom", agent: "pi", title: "Pi session", project_name: "p", project_path: "/p", timestamp: "2026-10-18T00:00:00Z", git_branch: "", claude_version: "", message_count: 1, tool_use_count: 0, duration_ms: 0, model: "", context_tokens: 30, context_limit: 0, cost_usd: 1.25, is_authoritative_stats: false, daily_cost: { "2026-10-18": 1.25 }, daily_tokens: { "2026-10-18": [10, 20, 30, 40] }, total_input_tokens: 10, total_cache_creation_tokens: 20, total_cache_read_tokens: 30, total_output_tokens: 40, rate_limit_5h_pct: null, rate_limit_7d_pct: null, ...over,
});
afterEach(() => vi.useRealTimers());

it("Pi lifetime usage reaches project token and cost charts", () => {
  vi.useFakeTimers();
  vi.setSystemTime(new Date("2026-10-18T12:00:00Z"));
  const s = session();
  expect(hasRecordedCost(s)).toBe(true);
  expect(projectUsage([s])).toEqual({ totalCost: 1.25, costSessionCount: 1, tokenTotals: [10, 20, 30, 40], totalTokens: 100, costLabel: "Recorded cost (Pi)", hasCostSource: true, offersCost: true });
  expect(dailyCostSeries([s], 2)).toEqual([{ date: "2026-10-17", values: [0] }, { date: "2026-10-18", values: [1.25] }]);
  expect(dailyTokensSeries([s], 1)).toEqual([{ date: "2026-10-18", values: [30, 20, 10, 40] }]);
  expect(s.is_authoritative_stats).toBe(false);
  expect(s.context_limit).toBe(0);
});

it("mixed cost labels name recorded sources and exclude unconnected Claude", () => {
  const pi = session();
  const claude = session({ agent: "claude", cost_usd: 9 });
  expect(projectUsage([pi, claude])).toMatchObject({ totalCost: 1.25, costLabel: "Recorded cost (Pi)", costSessionCount: 1 });
  expect(projectUsage([pi, { ...claude, is_authoritative_stats: true }])).toMatchObject({ totalCost: 10.25, costLabel: "Recorded cost (Pi + Claude Code)", costSessionCount: 2 });
  vi.useFakeTimers();
  vi.setSystemTime(new Date("2026-10-18T12:00:00Z"));
  expect(dailyCostSeries([pi, claude], 1)[0].values).toEqual([1.25]);
});

it("missing Pi cost stays zero; tokens stay available without invented limits", () => {
  expect(projectUsage([session({ cost_usd: 0, daily_cost: {} })])).toMatchObject({ totalCost: 0, totalTokens: 100, costLabel: "Recorded cost", costSessionCount: 0 });
});
