import { afterEach, describe, expect, it, vi } from "vitest";
import { normalizePath, processSessions, timeAgo } from "./utils";
import type { SessionInfo } from "./types";

function session(overrides: Partial<SessionInfo>): SessionInfo {
  return {
    id: "id",
    title: "Title",
    timestamp: "2026-01-01T00:00:00Z",
    message_count: 0,
    project_name: "proj",
    project_path: "/proj",
    git_branch: "",
    claude_version: "",
    tool_use_count: 0,
    duration_ms: 0,
    model: "",
    context_tokens: 0,
    context_limit: 0,
    cost_usd: 0,
    is_authoritative_stats: false,
    daily_cost: {},
    rate_limit_5h_pct: null,
    rate_limit_7d_pct: null,
    total_input_tokens: 0,
    total_cache_creation_tokens: 0,
    total_cache_read_tokens: 0,
    total_output_tokens: 0,
    daily_tokens: {},
    agent: "claude",
    ...overrides,
  };
}

describe("normalizePath", () => {
  it("unifies separators, trailing slashes and case", () => {
    expect(normalizePath("C:/Users/A/Proj//")).toBe("c:\\users\\a\\proj");
  });
});

describe("timeAgo", () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  it("buckets", () => {
    const now = new Date("2026-06-15T12:00:00Z");
    vi.useFakeTimers();
    vi.setSystemTime(now);
    const ago = (ms: number) => new Date(now.getTime() - ms).toISOString();
    const MIN = 60_000;
    const HOUR = 60 * MIN;
    const DAY = 24 * HOUR;

    expect(timeAgo("")).toBe("");
    expect(timeAgo(new Date(now.getTime() + MIN).toISOString())).toBe("");
    expect(timeAgo("not a date")).toBe("");
    expect(timeAgo(ago(30_000))).toBe("just now");
    expect(timeAgo(ago(5 * MIN))).toBe("5m ago");
    expect(timeAgo(ago(3 * HOUR))).toBe("3h ago");
    expect(timeAgo(ago(2 * DAY))).toBe("2d ago");
    expect(timeAgo(ago(65 * DAY))).toBe("2mo ago");
  });
});

describe("processSessions", () => {
  it("puts named sessions first, newest first", () => {
    const result = processSessions([
      session({ id: "old", title: "Old", timestamp: "2026-01-01T00:00:00Z" }),
      session({ id: "unnamed", title: "", timestamp: "2026-03-01T00:00:00Z" }),
      session({ id: "new", title: "New", timestamp: "2026-02-01T00:00:00Z" }),
    ]);
    expect(result.map((s) => s.id)).toEqual(["new", "old", "unnamed"]);
  });

  it("renames unnamed sessions to Unnamed #N in recency order", () => {
    const result = processSessions([
      session({ id: "u1", title: "Session abcdef12", timestamp: "2026-01-01T00:00:00Z" }),
      session({ id: "u2", title: "  ", timestamp: "2026-02-01T00:00:00Z" }),
    ]);
    expect(result.map((s) => [s.id, s.title])).toEqual([
      ["u2", "Unnamed #1"],
      ["u1", "Unnamed #2"],
    ]);
  });

  it("does not mutate its input", () => {
    const input = [
      session({ id: "a", title: "Session abcdef12", timestamp: "2026-01-01T00:00:00Z" }),
      session({ id: "b", title: "B", timestamp: "2026-01-02T00:00:00Z" }),
    ];
    const before = structuredClone(input);
    processSessions(input);
    expect(input).toEqual(before);
  });
});
