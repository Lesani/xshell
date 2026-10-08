import { describe, expect, it, vi } from "vitest";
const invoke = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...a: unknown[]) => invoke(...a) }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
vi.mock("@tauri-apps/plugin-store", () => ({ load: vi.fn() }));

import { fanOutSourced, freshestRateLimits, mergeCodexUsage, mergeRecent, sumCostSummaries } from "./aggregate";
import type { CodexUsage, GlobalRateLimits, SessionInfo } from "../types";

const rl = (pct: number, iso: string | null): GlobalRateLimits => ({ five_hour_pct: pct, seven_day_pct: null, five_hour_resets_at: null, seven_day_resets_at: null, last_update_iso: iso });
const cx = (iso: string | null, days: [string, number][], pct = 1): CodexUsage => ({ present: true, primary: { used_percent: pct, window_minutes: 300, resets_at: null }, secondary: null, plan_type: "plus", rate_limits_updated_iso: iso, daily_sessions: days.map(([date, count]) => ({ date, count })) });
const s = (id: string, ts: string) => ({ id, timestamp: ts } as SessionInfo);

describe("aggregate", () => {
  it("freshest by iso (nulls ignored)", () => {
    expect(freshestRateLimits([])).toBeNull();
    expect(freshestRateLimits([rl(1, null), rl(2, "2026-10-01T10:00:00Z"), rl(3, "2026-10-01T09:00:00Z")])?.five_hour_pct).toBe(2);
    expect(freshestRateLimits([rl(5, null), rl(6, null)])?.five_hour_pct).toBe(5);
    const only = rl(9, null);
    expect(freshestRateLimits([only])).toBe(only);
  });

  it("costs summed per date and sorted", () => {
    const one = { connected: false, daily: [{ date: "2026-10-02", usd: 1 }] };
    expect(sumCostSummaries([one])).toBe(one);
    expect(sumCostSummaries([one, { connected: true, daily: [{ date: "2026-10-01", usd: 2 }, { date: "2026-10-02", usd: 0.5 }] }])).toEqual({
      connected: true, daily: [{ date: "2026-10-01", usd: 2 }, { date: "2026-10-02", usd: 1.5 }],
    });
  });

  it("codex merge: windows from the freshest, sessions summed", () => {
    const a = cx("2026-10-01T00:00:00Z", [["2026-10-01", 2]], 10);
    const b = cx("2026-10-02T00:00:00Z", [["2026-10-01", 1], ["2026-10-02", 4]], 50);
    expect(mergeCodexUsage([a])).toBe(a);
    const m = mergeCodexUsage([a, b]);
    expect(m.primary?.used_percent).toBe(50);
    expect(m.rate_limits_updated_iso).toBe("2026-10-02T00:00:00Z");
    expect(m.daily_sessions).toEqual([{ date: "2026-10-01", count: 3 }, { date: "2026-10-02", count: 4 }]);
  });

  it("mergeRecent order and limit", () => {
    const local = [s("a", "2026-10-03"), s("b", "2026-10-01")];
    expect(mergeRecent([local])).toBe(local);
    expect(mergeRecent([local, []])).toBe(local);
    const merged = mergeRecent([local, [s("r", "2026-10-02")]], 2);
    expect(merged.map(x => x.id)).toEqual(["a", "r"]);
  });

  it("fanOut with no hosts calls only the local command, exactly as before", async () => {
    invoke.mockResolvedValueOnce({ present: false });
    const r = await fanOutSourced("get_codex_usage");
    expect(invoke.mock.calls).toEqual([["get_codex_usage"]]);
    expect(r).toEqual([{ host: undefined, value: { present: false } }]);
  });
});
