import { describe, expect, it, vi } from "vitest";
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(), Channel: class {} }));

import { isRemoteDataHost, listsForReconcile, needsCache, persistableTabs, restorableTabs, restoreGroups, withHostData } from "./appTabs";
import type { Group, ProjectInfo, SessionInfo, Tab } from "../types";
import type { TerminalInfo } from "./types";

const H = "h_ab12cd34";
const info = (uuid: string, createdAtMs: number, over: Partial<TerminalInfo["spec"]> = {}): TerminalInfo =>
  ({ terminal: uuid, spec: { cwd: "/p", agent: "claude", ...over }, meta: { title: uuid }, createdAtMs, pid: 1, exitCode: null });
const inproc: Tab = { id: "terminal-s1-a", type: "terminal", title: "A", sessionId: "s1", projectPath: "/p" };
const split = (id: string, a: string, b: string, name = "Group 1"): Group =>
  ({ id, name, layout: { kind: "split", direction: "row", ratio: 0.5, children: [{ kind: "leaf", tabId: a }, { kind: "leaf", tabId: b }] } } as Group);

describe("persistableTabs", () => {
  it("drops Daemon Tabs (Remote and Local) and keeps identity when there are none", () => {
    const local: Tab = { id: "remote-l", type: "terminal", title: "L", terminal: "l" };
    const remote: Tab = { id: "remote-r", type: "terminal", title: "R", host: H, terminal: "r" };
    expect(persistableTabs([inproc, local, remote])).toEqual([inproc]);
    const only = [inproc];
    expect(persistableTabs(only)).toBe(only);
  });
});

describe("restorableTabs", () => {
  const cached: Record<string, TerminalInfo[]> = { [H]: [info("r1", 2)], local: [info("l2", 5), info("l1", 1, { shellMode: "raw", shellId: "bash", agent: null })] };
  const get = (h: string) => cached[h] ?? null;

  it("in-process saved Tabs with a session, then Remote, then (Daemon mode) Local Daemon Tabs by age", () => {
    const saved: Tab[] = [inproc, { id: "terminal-new-1", type: "terminal", title: "New Chat", projectPath: "/p" }, { id: "remote-x", type: "terminal", title: "X", terminal: "x", sessionId: "s", projectPath: "/p" }];
    const tabs = restorableTabs({ saved, hosts: [H], localDaemon: true, cached: get });
    expect(tabs.map(t => t.id)).toEqual(["terminal-s1-a", "remote-r1", "remote-l1", "remote-l2"]);
    expect(tabs[1].host).toBe(H);
    // Local raw shells and session-less chats come back, with no host.
    expect(tabs[2]).toMatchObject({ terminal: "l1", shellMode: "raw", shellId: "bash" });
    expect(tabs[2].host).toBeUndefined();
    expect(tabs[3].host).toBeUndefined();
  });

  it("M7: in in-process mode the cached Local list is never mounted (and not read)", () => {
    const seen: string[] = [];
    const tabs = restorableTabs({ saved: [inproc], hosts: [H], localDaemon: false, cached: h => { seen.push(h); return get(h); } });
    expect(tabs.map(t => t.id)).toEqual(["terminal-s1-a", "remote-r1"]);
    expect(seen).toEqual([H]);
    // The cache itself is untouched.
    expect(cached.local).toHaveLength(2);
  });

  it("needs the cache with Remote Hosts or in local Daemon mode (M6)", () => {
    expect(needsCache(0, false)).toBe(false);
    expect(needsCache(1, false)).toBe(true);
    expect(needsCache(0, true)).toBe(true);
  });
});

describe("M6: local-only restart keeps the groups of Local Daemon Tabs", () => {
  it("a saved split of two cached Local Daemon Tabs survives, with groupIds restored", () => {
    // Before quitting: two Local Daemon Tabs in a split (not in open_tabs), one in-process Tab.
    const before: Tab[] = [inproc, { id: "remote-l1", type: "terminal", title: "l1", terminal: "l1", groupId: "g1" }, { id: "remote-l2", type: "terminal", title: "l2", terminal: "l2", groupId: "g1" }];
    const saved = persistableTabs(before);
    const groups = [split("g1", "remote-l1", "remote-l2"), split("g2", "terminal-s1-a", "remote-gone", "Group 2")];
    const cached = { local: [info("l1", 1), info("l2", 2)] } as Record<string, TerminalInfo[]>;
    const restorable = restorableTabs({ saved, hosts: [], localDaemon: true, cached: h => cached[h] ?? null });
    const r = restoreGroups(restorable, groups);
    expect(r.groups.map(g => g.id)).toEqual(["g1"]);
    expect(r.tabs.filter(t => t.groupId === "g1").map(t => t.id)).toEqual(["remote-l1", "remote-l2"]);
    // Without the cache (not loaded) the group would be dropped.
    expect(restoreGroups(restorableTabs({ saved, hosts: [], localDaemon: true, cached: () => null }), groups).groups).toEqual([]);
  });

  it("scrubs a groupId of a group that is not kept", () => {
    const r = restoreGroups([{ ...inproc, groupId: "gone" }], []);
    expect(r.tabs[0].groupId).toBeUndefined();
  });
});

describe("listsForReconcile", () => {
  it("maps the wire id \"local\" to the Tab host undefined", () => {
    const l: TerminalInfo[] = [];
    expect(listsForReconcile([["local", l], [H, l]])).toEqual([[undefined, l], [H, l]]);
  });
});

describe("M4: \"local\" never enters the Remote data path", () => {
  const project: ProjectInfo = { name: "p", path: "/p", encoded_name: "-p", session_count: 1, last_active: "" };
  it("a local usable-status transition leaves the local Projects and their keys as they are", () => {
    const prev: Record<string, ProjectInfo[]> = { local: [project] };
    expect(isRemoteDataHost("local")).toBe(false);
    expect(isRemoteDataHost(H)).toBe(true);
    const after = withHostData(prev, "local", [{ ...project, name: "from the link" }]);
    expect(after).toBe(prev);
    expect(after.local[0].host).toBeUndefined();
  });

  it("a Remote Host's data is stamped with its host", () => {
    const sessions = withHostData<SessionInfo>({}, H, [{ id: "s" } as SessionInfo]);
    expect(sessions[H][0].host).toBe(H);
    const prev = { local: [project] };
    expect(withHostData(prev, H, null)).toBe(prev);
  });
});
