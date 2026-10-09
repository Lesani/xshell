import { describe, expect, it } from "vitest";
import { skipPermsOn, skipPermsState, type SkipPermsContext } from "./skipPermissions";
import { AGENTS, AGENT_IDS } from "../agents";
import { fmt } from "../hosts/strings";
import type { Tab } from "../types";
import type { HostStatus, TerminalInfo } from "../hosts/types";

const H = "h_ab12cd34";
const local: Tab = { id: "terminal-s1-a", type: "terminal", title: "T", sessionId: "s1", agent: "claude", projectPath: "/p" };
const remote: Tab = { id: "remote-u1", type: "terminal", title: "T", host: H, terminal: "u1", sessionId: "stale", agent: "claude", projectPath: "/p" };

const status = (s: HostStatus["status"], caps: string[] = ["call", "term", "term.relaunch"]): HostStatus => ({
  host: H, status: s, phase: null, lastError: null, errorHint: null, daemonVersion: "1.5.0", desktopVersion: "1.5.0",
  protocol: 1, os: "linux", arch: "x86_64", incompatibleReason: null, nextRetryAt: null, sinceMs: 0, daemonCapabilities: caps,
});
const info = (over: Partial<TerminalInfo["spec"]> = {}, exitCode: number | null = null): TerminalInfo => ({
  terminal: "u1", spec: { cwd: "/p", agent: "claude", sessionId: "s-live", ...over }, meta: {}, createdAtMs: 1, pid: 2, exitCode,
});
const ctx = (over: Partial<SkipPermsContext> = {}): SkipPermsContext => ({
  live: undefined, status: undefined, hostName: "Dev", localEnded: false, busy: false, ...over,
});
const disabled = (reason: string) => ({ kind: "disabled", reason });

describe("skipPermsState: local tabs", () => {
  it("is available for claude and codex with a session, on from the tab", () => {
    expect(skipPermsState(local, ctx())).toEqual({ kind: "available", on: false });
    expect(skipPermsState({ ...local, skipPermissions: true }, ctx())).toEqual({ kind: "available", on: true });
    expect(skipPermsState({ ...local, agent: "codex" }, ctx())).toEqual({ kind: "available", on: false });
    // Claude is the default agent.
    expect(skipPermsState({ ...local, agent: undefined }, ctx())).toEqual({ kind: "available", on: false });
  });

  it("is hidden for raw shells and agents without a flag", () => {
    expect(skipPermsState({ ...local, shellMode: "raw" }, ctx())).toEqual({ kind: "hidden" });
    for (const agent of ["cursor", "opencode", "antigravity"] as const) {
      expect(skipPermsState({ ...local, agent }, ctx())).toEqual({ kind: "hidden" });
    }
  });

  it("is disabled while busy, once ended, and before a session exists", () => {
    expect(skipPermsState(local, ctx({ busy: true }))).toEqual(disabled(fmt("tab.skipPerms.button.busy")));
    expect(skipPermsState(local, ctx({ localEnded: true }))).toEqual(disabled(fmt("tab.skipPerms.disabled.ended")));
    expect(skipPermsState({ ...local, sessionId: undefined }, ctx())).toEqual(disabled(fmt("tab.skipPerms.disabled.noSession")));
  });
});

describe("skipPermsState: remote tabs", () => {
  it("reads the session and the value from the live list, not the tab", () => {
    const c = ctx({ status: status("connected"), live: [info({ skipPermissions: true })] });
    expect(skipPermsState({ ...remote, skipPermissions: false }, c)).toEqual({ kind: "available", on: true });
    const off = ctx({ status: status("upgrade-pending"), live: [info()] });
    expect(skipPermsState({ ...remote, skipPermissions: true }, off)).toEqual({ kind: "available", on: false });
    const noSession = ctx({ status: status("connected"), live: [info({ sessionId: null })] });
    expect(skipPermsState(remote, noSession)).toEqual(disabled(fmt("tab.skipPerms.disabled.noSession")));
  });

  it("is hidden when the Daemon cannot relaunch", () => {
    expect(skipPermsState(remote, ctx({ status: status("connected", ["call", "term"]), live: [info()] }))).toEqual({ kind: "hidden" });
    expect(skipPermsState(remote, ctx({ status: { ...status("connected"), daemonCapabilities: undefined }, live: [info()] }))).toEqual({ kind: "hidden" });
  });

  it("is disabled while the Host is not usable, the Terminal is unlisted or has ended", () => {
    const unavailable = disabled(fmt("tab.skipPerms.disabled.hostUnavailable", { host: "Dev" }));
    expect(skipPermsState(remote, ctx({ status: status("offline", []), live: [info()] }))).toEqual(unavailable);
    expect(skipPermsState(remote, ctx({ status: undefined }))).toEqual(unavailable);
    expect(skipPermsState(remote, ctx({ status: status("connected"), live: [] }))).toEqual(unavailable);
    expect(skipPermsState(remote, ctx({ status: status("connected"), live: [info({}, 0)] }))).toEqual(disabled(fmt("tab.skipPerms.disabled.ended")));
    expect(skipPermsState(remote, ctx({ status: status("connected"), live: [info()], busy: true }))).toEqual(disabled(fmt("tab.skipPerms.button.busy")));
  });

  it("skipPermsOn follows the same source", () => {
    expect(skipPermsOn({ ...remote, skipPermissions: true }, [info()])).toBe(false);
    expect(skipPermsOn(remote, [info({ skipPermissions: true })])).toBe(true);
    expect(skipPermsOn({ ...local, skipPermissions: true }, [])).toBe(true);
  });
});

describe("agents: bypassFlag", () => {
  // Pinned to permission_flag in src-tauri/crates/core/src/launch.rs.
  it("is set exactly for the agents core has a flag for", () => {
    expect(AGENT_IDS.filter(a => AGENTS[a].bypassFlag)).toEqual(["claude", "codex"]);
  });
});
