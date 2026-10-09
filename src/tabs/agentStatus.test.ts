import { describe, expect, it } from "vitest";
import { AGENT_STATUSES, agentStatusAria, agentStatusLabel, agentStatusOf, agentStatusShortLabel, agentStatusTooltip, wrapLines, type AgentStatusContext } from "./agentStatus";
import { S } from "../hosts/strings";
import type { Tab } from "../types";
import type { AgentStatus, HostStatus, TerminalInfo } from "../hosts/types";

const H = "h_ab12cd34";
const local: Tab = { id: "terminal-s1-a", type: "terminal", title: "T", sessionId: "s1", agent: "claude", projectPath: "/p" };
const remote: Tab = { id: "remote-u1", type: "terminal", title: "T", host: H, terminal: "u1", agent: "claude", projectPath: "/p" };

const status = (s: HostStatus["status"], caps: string[] = ["call", "term", "agent.status"]): HostStatus => ({
  host: H, status: s, phase: null, lastError: null, errorHint: null, daemonVersion: "1.5.0", desktopVersion: "1.5.0",
  protocol: 1, os: "linux", arch: "x86_64", incompatibleReason: null, nextRetryAt: null, sinceMs: 0,
  daemonCapabilities: s === "connected" || s === "upgrade-pending" ? caps : [],
});
const info = (agentStatus?: unknown): TerminalInfo => ({
  terminal: "u1", spec: { cwd: "/p", agent: "claude" }, meta: {}, createdAtMs: 1, pid: 2, exitCode: null,
  agentStatus: agentStatus as AgentStatus | undefined,
});
const ctx = (over: Partial<AgentStatusContext> = {}): AgentStatusContext => ({ live: undefined, status: undefined, local: new Map(), ...over });

describe("agentStatusOf", () => {
  it("local tab reads the local store", () => {
    expect(agentStatusOf(local, ctx())).toBeNull();
    expect(agentStatusOf(local, ctx({ local: new Map([[local.id, "needs-you"]]) }))).toEqual({ status: "needs-you", stale: false });
    expect(agentStatusOf({ ...local, agent: "codex" }, ctx({ local: new Map([[local.id, "working"]]) }))).toEqual({ status: "working", stale: false });
    // Claude is the default agent; other Tabs' entries do not leak.
    expect(agentStatusOf({ ...local, agent: undefined }, ctx({ local: new Map([[local.id, "finished"]]) }))).toEqual({ status: "finished", stale: false });
    expect(agentStatusOf(local, ctx({ local: new Map([["other", "finished"]]) }))).toBeNull();
  });

  it("remote tab shows agentStatus when the Daemon has agent.status", () => {
    for (const s of AGENT_STATUSES) {
      expect(agentStatusOf(remote, ctx({ live: [info(s)], status: status("connected") }))).toEqual({ status: s, stale: false });
    }
    expect(agentStatusOf(remote, ctx({ live: [info("ended")], status: status("upgrade-pending") }))).toEqual({ status: "ended", stale: false });
    expect(agentStatusOf(remote, ctx({ live: [info()], status: status("connected") }))).toBeNull();
    expect(agentStatusOf(remote, ctx({ live: [info(null)], status: status("connected") }))).toBeNull();
    // Another Terminal's entry, or none at all.
    expect(agentStatusOf({ ...remote, terminal: "u2" }, ctx({ live: [info("working")], status: status("connected") }))).toBeNull();
    expect(agentStatusOf(remote, ctx({ live: null, status: status("connected") }))).toBeNull();
  });

  it("connected Daemon without the capability shows nothing", () => {
    expect(agentStatusOf(remote, ctx({ live: [info("working")], status: status("connected", ["call", "term"]) }))).toBeNull();
  });

  it("offline host keeps last known status as stale", () => {
    for (const s of ["offline", "reconnecting", "incompatible"] as const) {
      expect(agentStatusOf(remote, ctx({ live: [info("needs-you")], status: status(s) }))).toEqual({ status: "needs-you", stale: true });
    }
    expect(agentStatusOf(remote, ctx({ live: [info("needs-you")], status: undefined }))).toEqual({ status: "needs-you", stale: true });
  });

  it("raw shells and hookless agents show nothing", () => {
    const m = new Map<string, AgentStatus>([[local.id, "working"]]);
    expect(agentStatusOf({ ...local, shellMode: "raw" }, ctx({ local: m }))).toBeNull();
    for (const agent of ["cursor", "opencode", "antigravity"] as const) {
      expect(agentStatusOf({ ...local, agent }, ctx({ local: m }))).toBeNull();
      expect(agentStatusOf({ ...remote, agent }, ctx({ live: [info("working")], status: status("connected") }))).toBeNull();
    }
  });

  it("unknown status values are ignored", () => {
    for (const v of ["thinking", "", 7, {}]) {
      expect(agentStatusOf(remote, ctx({ live: [info(v)], status: status("connected") }))).toBeNull();
      expect(agentStatusOf(remote, ctx({ live: [info(v)], status: status("offline") }))).toBeNull();
    }
    expect(agentStatusOf(local, ctx({ local: new Map([[local.id, "bogus" as AgentStatus]]) }))).toBeNull();
  });
});

describe("agent status text", () => {
  it("every status has a string", () => {
    expect(AGENT_STATUSES.map(agentStatusShortLabel)).toEqual(["Working", "Needs you", "Finished", "Ended"]);
    expect(AGENT_STATUSES.map(agentStatusLabel)).toEqual([
      S["tab.agentStatus.working"], S["tab.agentStatus.needsYou"], S["tab.agentStatus.finished"], S["tab.agentStatus.ended"],
    ]);
    for (const s of AGENT_STATUSES) expect(agentStatusLabel(s)).toBeTruthy();
  });

  it("tooltip and aria use the binding copy", () => {
    const live = { status: "needs-you" as const, stale: false };
    expect(agentStatusTooltip(live, "Dev")).toBe("Needs you: waiting for permission or an answer");
    expect(agentStatusAria(live, "Dev")).toBe("Agent status: Needs you");
    const stale = { status: "finished" as const, stale: true };
    expect(agentStatusTooltip(stale, "Dev")).toBe("Finished (last known)\nDev is not connected");
    expect(agentStatusAria(stale, "Dev")).toBe("Agent status: Finished (last known), Dev is not connected");
  });

  it("long host names wrap at 60 characters per line", () => {
    const host = "build-server ".repeat(8).trim();
    const t = agentStatusTooltip({ status: "working", stale: true }, host);
    const lines = t.split("\n");
    expect(lines[0]).toBe("Working (last known)");
    expect(lines.length).toBeGreaterThan(2);
    for (const l of lines) expect(l.length).toBeLessThanOrEqual(60);
    expect(lines.slice(1).join(" ")).toBe(`${host} is not connected`);
    // A single word longer than a line stays whole.
    expect(wrapLines("x".repeat(70))).toBe("x".repeat(70));
  });
});

describe("agentStatusOf: local Daemon Tabs", () => {
  const ld: Tab = { id: "remote-u1", type: "terminal", title: "T", terminal: "u1", agent: "claude", projectPath: "/p" };
  it("read the live list, not the in-process store", () => {
    expect(agentStatusOf(ld, ctx({ live: [info("working")], status: status("connected"), local: new Map([[ld.id, "finished"]]) }))).toEqual({ status: "working", stale: false });
    expect(agentStatusOf(ld, ctx({ live: undefined, local: new Map([[ld.id, "finished"]]) }))).toBeNull();
  });
});
