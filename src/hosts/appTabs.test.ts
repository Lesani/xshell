import { describe, expect, it, vi } from "vitest";
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(), Channel: class {} }));

import { isRemoteDataHost, listsForReconcile, persistableTabs, restorableTabs, restoreGroups, withHostData } from "./appTabs";
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

  it("#4/G: migrated Terminals merge with the cached Local ones in the Daemon's order, the live entry winning", () => {
    const migrated = [info("l2", 5, { sessionId: "fresh" }), info("m0", 3), info("m9", 9)];
    const tabs = restorableTabs({ saved: [], hosts: [H], localDaemon: true, cached: get, migrated });
    expect(tabs.map(t => t.id)).toEqual(["remote-r1", "remote-l1", "remote-m0", "remote-l2", "remote-m9"]);
    expect(tabs[3].sessionId).toBe("fresh");
  });

  it("#4: in in-process mode `migrated` is ignored and `saved` restores exactly as before", () => {
    const a = restorableTabs({ saved: [inproc], hosts: [H], localDaemon: false, cached: get, migrated: [info("m1", 1)] });
    const b = restorableTabs({ saved: [inproc], hosts: [H], localDaemon: false, cached: get });
    expect(a).toEqual(b);
    expect(a.map(t => t.id)).toEqual(["terminal-s1-a", "remote-r1"]);
  });

  it("restores retained Ring cache with no saved SSH or Local Daemon", () => {
    const r = `r_${"b".repeat(64)}`;
    expect(restorableTabs({ saved: [], hosts: [r], localDaemon: false, cached: () => [info("offline", 1)] })[0])
      .toMatchObject({ host: r, terminal: "offline" });
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

describe("shared names in cached/reconnect reconstruction", () => {
  it("cache_restores_separate_titles_and_absent_keys_restore_current_fallback", async () => {
    const { emptyCache, putTerminals, parseCache } = await import("./cache");
    const { reconcile, applyTabDelta } = await import("./reconcile");
    const named = { ...info("u", 0), meta: { title: "Old", customTitle: "Shared" } };
    let cached = putTerminals(emptyCache(), "local", [named], 1);
    const restore = () => {
      const disk = parseCache(JSON.parse(JSON.stringify(cached)));
      return restorableTabs({ saved: [], hosts: [], localDaemon: true, cached: h => disk.hosts[h]?.terminals ?? null });
    };
    const before = restore();
    expect(before[0]).toMatchObject({ title: "Old", customTitle: "Shared" });
    // A combined {title:null,customTitle:null} patch removes both keys in the list.
    const cleared = { ...named, meta: {} };
    const current = applyTabDelta(before, reconcile(before, undefined, [cleared], new Set()));
    expect(current[0].title).toBe("Session"); expect(current[0]).not.toHaveProperty("customTitle");
    cached = putTerminals(cached, "local", [cleared], 2);
    expect(restore()).toEqual(current);
  });
});

describe("in-process custom name persistence round trip", () => {
  it("eligible_agents_keep_independent_custom_and_automatic_names_and_group_identity", () => {
    const agent = { ...inproc, title: "Latest automatic", customTitle: "Local custom", groupId: "g" };
    const second = { ...inproc, id: "second", sessionId: "s2", groupId: "g" };
    const group = split("g", agent.id, second.id, "Personal split");
    const shell: Tab = { id: "raw", type: "terminal", title: "Bash", customTitle: "Named raw", shellMode: "raw", projectPath: "/p" };
    const newChat: Tab = { id: "new", type: "terminal", title: "New", customTitle: "Named new", projectPath: "/p" };
    const daemon: Tab = { ...inproc, id: "remote-local", terminal: "local", customTitle: "Never persist Daemon cache here" };
    const saved: Tab[] = JSON.parse(JSON.stringify(persistableTabs([agent, second, shell, newChat, daemon])));
    expect(saved.map(t => t.id)).toEqual([agent.id, second.id, "raw", "new"]);
    const restored = restoreGroups(restorableTabs({ saved, hosts: [], localDaemon: false, cached: () => null }), JSON.parse(JSON.stringify([group])));
    expect(restored.tabs).toEqual([agent, second]); expect(restored.groups).toEqual([group]);
    const cleared: Tab = { ...agent }; delete cleared.customTitle;
    const reload = restorableTabs({ saved: JSON.parse(JSON.stringify(persistableTabs([cleared]))), hosts: [], localDaemon: false, cached: () => null });
    expect(reload[0]).not.toHaveProperty("customTitle"); expect(reload[0].title).toBe("Latest automatic");
  });
  it("live_list_name_or_clear_overrides_cached_daemon_custom_name", () => {
    const cached = { ...info("local", 1), meta: { title: "Auto", customTitle: "Old cache" } };
    for (const value of ["New live", undefined]) {
      const live = { ...cached, meta: { title: "Latest auto", customTitle: value } };
      const restored = restorableTabs({ saved: [], hosts: [], localDaemon: true, cached: () => [cached], migrated: [live] });
      expect(restored[0].customTitle).toBe(value); expect(restored[0].title).toBe("Latest auto");
    }
  });
});

import { registry } from "./registry";
it("restorableTabs deduplicates verified r/h aliases and excludes proven Local while retaining offline UUIDs", async () => {
  const r = `r_${"c".repeat(64)}`, local = `r_${"d".repeat(64)}`;
  registry.applyAvailable({ sequence: 1, hosts: [
    { id: H, aliases: [H, r], name: "SSH equivalent", color: null, daemonKey: "key", routes: ["ssh", "relay"], activeRoute: "ssh", reachability: { kind: "unknown" } },
    { id: "local", aliases: ["local", local], name: "Local", color: null, daemonKey: "local-key", routes: ["local", "relay"], activeRoute: "local", reachability: { kind: "unknown" } },
  ] });
  const tabs = restorableTabs({ saved: [], hosts: [H, r, local], localDaemon: true,
    cached: host => host === "local" ? [info("local", 1)] : [info("retained", 2)] });
  expect(tabs.map(t => [t.terminal, t.host])).toEqual([["retained", r], ["local", undefined]]);
});
