import { describe, expect, it } from "vitest";
import { applyReconcile, reconcile, restoreGroupIds, tabFromTerminal, remoteTabId } from "./reconcile";
import type { Group, Tab } from "../types";
import type { TerminalInfo } from "./types";

const H = "h_ab12cd34";
const H2 = "h_zz12cd34";

function info(uuid: string, over: Partial<TerminalInfo> = {}, meta: Record<string, unknown> = {}): TerminalInfo {
  return { terminal: uuid, spec: { cwd: "/home/u/proj", agent: "claude", sessionId: `s-${uuid}`, shellMode: "claude" }, meta, createdAtMs: 100, pid: 1, exitCode: null, ...over };
}
const state = (tabs: Tab[], groups: Group[] = [], activeLeafByGroup: Record<string, string> = {}) => ({ tabs, groups, activeLeafByGroup });

describe("reconcile", () => {
  it("adds new terminals with mapped fields", () => {
    const d = reconcile([], H, [info("u1", {}, { title: "Fix bug", projectName: "proj", createdAt: 42 })], new Set());
    expect(d.add).toHaveLength(1);
    const t = d.add[0];
    expect(t).toMatchObject({ id: "remote-u1", host: H, terminal: "u1", projectPath: "/home/u/proj", projectName: "proj", title: "Fix bug", sessionId: "s-u1", agent: "claude", shellMode: "claude", createdAt: 42 });
  });

  it("maps fallbacks: basename, raw shell name, createdAtMs, home", () => {
    const raw = tabFromTerminal(H, { terminal: "r", spec: { cwd: "", shellMode: "raw", shellId: "bash" }, meta: {}, createdAtMs: 7, pid: null, exitCode: null });
    expect(raw).toMatchObject({ title: "Bash", projectName: "~", shellMode: "raw", shellId: "bash", createdAt: 7 });
    const agent = tabFromTerminal(H, info("a"));
    expect(agent).toMatchObject({ title: "Session", projectName: "proj", createdAt: 100 });
  });

  it("removes gone terminals and their group leaves", () => {
    const a = tabFromTerminal(H, info("a"));
    const b = tabFromTerminal(H, info("b"));
    const g: Group = { id: "g1", name: "Group 1", layout: { kind: "split", direction: "col", ratio: 0.5, children: [{ kind: "leaf", tabId: a.id }, { kind: "leaf", tabId: b.id }] } };
    const tabs = [{ ...a, groupId: "g1" }, { ...b, groupId: "g1" }];
    const d = reconcile(tabs, H, [info("b")], new Set());
    expect(d.remove).toEqual([a.id]);
    const r1 = applyReconcile(state(tabs, [g]), d);
    expect(r1.tabs.map(t => t.id)).toEqual([b.id]);
    expect(r1.groups[0].layout).toEqual({ kind: "leaf", tabId: b.id });
    // last leaf gone → layout null → group dropped
    const d2 = reconcile(r1.tabs, H, [], new Set());
    const r2 = applyReconcile(r1, d2);
    expect(r2.tabs).toEqual([]);
    expect(r2.groups).toEqual([]);
  });

  it("keeps existing tabs, preserving groupId/lastActiveAt, updating title/sessionId from list", () => {
    const t = { ...tabFromTerminal(H, info("a", {}, { title: "Old" })), groupId: "g1", lastActiveAt: 99 };
    const d = reconcile([t], H, [info("a", { spec: { cwd: "/home/u/proj", sessionId: "s-new", agent: "claude" } }, { title: "New" })], new Set());
    expect(d.update).toHaveLength(1);
    const out = applyReconcile(state([t]), d).tabs[0];
    expect(out).toMatchObject({ id: t.id, title: "New", sessionId: "s-new", groupId: "g1", lastActiveAt: 99 });
  });

  it("does not overwrite fields with a local edit in flight", () => {
    const t = tabFromTerminal(H, info("a", {}, { title: "Local edit" }));
    const d = reconcile([t], H, [info("a", {}, { title: "Old" })], new Set(), (_id, f) => f === "title");
    expect(d.update).toEqual([]);
  });

  it("pending opens are kept even when absent", () => {
    const opening = tabFromTerminal(H, info("p1"));
    const failed = tabFromTerminal(H, info("p2"));
    const d = reconcile([opening, failed], H, [], new Set(["p1", "p2"]));
    expect(d.remove).toEqual([]);
    const d2 = reconcile([opening], H, [info("p1")], new Set(["p1"]));
    expect(d2.confirmed).toEqual(["p1"]);
  });

  it("ignores local tabs and other hosts' tabs", () => {
    const local: Tab = { id: "terminal-x", type: "terminal", title: "L", projectPath: "/home/u/proj", sessionId: "s-a" };
    const other = tabFromTerminal(H2, info("o"));
    const d = reconcile([local, other], H, [], new Set());
    expect(d).toEqual({ add: [], remove: [], update: [], confirmed: [] });
  });

  it("exited terminals stay", () => {
    const t = tabFromTerminal(H, info("a"));
    const d = reconcile([t], H, [info("a", { exitCode: 0, pid: null })], new Set());
    expect(d.remove).toEqual([]);
  });

  it("new tabs appended in createdAtMs order", () => {
    const existing: Tab = { id: "terminal-x", type: "terminal", title: "L" };
    const d = reconcile([existing], H, [info("late", { createdAtMs: 300 }), info("early", { createdAtMs: 100 }), info("mid", { createdAtMs: 200 })], new Set());
    const out = applyReconcile(state([existing]), d).tabs;
    expect(out.map(t => t.id)).toEqual(["terminal-x", remoteTabId("early"), remoteTabId("mid"), remoteTabId("late")]);
  });

  it("idempotent", () => {
    const list = [info("a", {}, { title: "T" }), info("b", {}, { title: "U" })];
    const s1 = applyReconcile(state([]), reconcile([], H, list, new Set()));
    const d2 = reconcile(s1.tabs, H, list, new Set());
    expect(d2.add).toEqual([]);
    expect(d2.remove).toEqual([]);
    expect(d2.update).toEqual([]);
    const s2 = applyReconcile(s1, d2);
    expect(s2.tabs).toBe(s1.tabs);
    expect(s2.groups).toBe(s1.groups);
  });
});

describe("groups with remote leaves (amendment 22)", () => {
  const leaf = (tabId: string) => ({ kind: "leaf" as const, tabId });
  const split = (a: any, b: any) => ({ kind: "split" as const, direction: "col" as const, ratio: 0.5, children: [a, b] as [any, any] });

  it("startup restores groupId for cached remote tabs in a mixed group", () => {
    const local: Tab = { id: "terminal-l", type: "terminal", title: "L", groupId: "g1", sessionId: "s", projectPath: "/p" };
    const remote = tabFromTerminal(H, info("r"));
    const g: Group = { id: "g1", name: "Group 1", layout: split(leaf(local.id), leaf(remote.id)) };
    const out = restoreGroupIds([local, remote], [g]);
    expect(out[0]).toBe(local);
    expect(out[1].groupId).toBe("g1");
  });

  it("startup restores an all-remote group", () => {
    const a = tabFromTerminal(H, info("a"));
    const b = tabFromTerminal(H2, info("b"));
    const g: Group = { id: "g2", name: "Group 2", layout: split(leaf(a.id), leaf(b.id)) };
    expect(restoreGroupIds([a, b], [g]).map(t => t.groupId)).toEqual(["g2", "g2"]);
  });

  it("focused remote leaf removed moves focus to a survivor atomically", () => {
    const a = { ...tabFromTerminal(H, info("a")), groupId: "g1" };
    const b = { ...tabFromTerminal(H, info("b")), groupId: "g1" };
    const c: Tab = { id: "terminal-c", type: "terminal", title: "C", groupId: "g1" };
    const g: Group = { id: "g1", name: "Group 1", layout: split(leaf(a.id), split(leaf(b.id), leaf(c.id))) };
    const s = applyReconcile(state([a, b, c], [g], { g1: a.id }), reconcile([a, b, c], H, [info("b")], new Set()));
    expect(s.activeLeafByGroup.g1).toBe(b.id);
    const s2 = applyReconcile({ ...s, activeLeafByGroup: { g1: b.id } }, reconcile(s.tabs, H, [], new Set()));
    expect(s2.activeLeafByGroup.g1).toBe(c.id);
    expect(s2.groups[0].layout).toEqual(leaf(c.id));
  });
});

// Sol finding 3: removals from several Hosts are one transaction — focus never lands on a
// pane another Host's list removed in the same pass.
import { applyHostsReconcile, reconcileHosts } from "./reconcile";
describe("multi-host reconcile transaction", () => {
  const leaf = (tabId: string) => ({ kind: "leaf" as const, tabId });
  const split = (a: any, b: any) => ({ kind: "split" as const, direction: "col" as const, ratio: 0.5, children: [a, b] as [any, any] });

  it("removals from two hosts in one group keep focus on a survivor", () => {
    const a = { ...tabFromTerminal(H, info("a")), groupId: "g1" };
    const b = { ...tabFromTerminal(H2, info("b")), groupId: "g1" };
    const c: Tab = { id: "terminal-c", type: "terminal", title: "C", groupId: "g1" };
    const d: Tab = { id: "terminal-d", type: "terminal", title: "D", groupId: "g1" };
    // a, b first so a naive per-host pass would hand focus from a to b
    const g: Group = { id: "g1", name: "Group 1", layout: split(split(leaf(a.id), leaf(b.id)), split(leaf(c.id), leaf(d.id))) };
    const r = reconcileHosts([a, b, c, d], [[H, []], [H2, []]], { pending: new Set() });
    expect(r.removed.sort()).toEqual([a.id, b.id].sort());
    const s = applyHostsReconcile({ tabs: [a, b, c, d], groups: [g], activeLeafByGroup: { g1: a.id } }, r);
    expect(s.tabs.map(t => t.id)).toEqual([c.id, d.id]);
    expect(s.groups[0].layout).toEqual(split(leaf(c.id), leaf(d.id)));
    expect([c.id, d.id]).toContain(s.activeLeafByGroup.g1);
  });

  it("closing terminals are never re-added; other hosts still reconcile", () => {
    const r = reconcileHosts([], [[H, [info("x")]], [H2, [info("y")]]], { pending: new Set(), isClosing: u => u === "x" });
    expect(r.deltas.flatMap(d => d.add.map(t => t.terminal))).toEqual(["y"]);
  });
});

describe("reconcile: skip permissions", () => {
  it("maps the spec's value onto new tabs", () => {
    expect(tabFromTerminal(H, info("a", { spec: { ...info("a").spec, skipPermissions: true } })).skipPermissions).toBe(true);
    expect(tabFromTerminal(H, info("b")).skipPermissions).toBeUndefined();
  });

  it("propagates true → false and false → true", () => {
    const on = { ...info("a"), spec: { ...info("a").spec, skipPermissions: true } };
    const tab = tabFromTerminal(H, on);
    expect(reconcile([tab], H, [on], new Set()).update).toEqual([]);
    const d = reconcile([tab], H, [info("a")], new Set());
    expect(d.update).toEqual([{ ...tab, skipPermissions: false }]);
    const back = reconcile(d.update, H, [on], new Set());
    expect(back.update).toEqual([{ ...tab, skipPermissions: true }]);
    // A spec without the field (older Daemon) is off.
    expect(reconcile(d.update, H, [info("a")], new Set()).update).toEqual([]);
  });
});
