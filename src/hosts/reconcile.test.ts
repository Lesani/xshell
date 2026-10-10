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

// xshell#41: an open answered with the Terminal that already runs its session.
import { adoptTab, dissolveSmallGroups, focusOf, keepsFocusWhenListed } from "./reconcile";
describe("adoptTab", () => {
  const leaf = (tabId: string) => ({ kind: "leaf" as const, tabId });
  const split = (a: any, b: any) => ({ kind: "split" as const, direction: "col" as const, ratio: 0.5, children: [a, b] as [any, any] });
  const pending = (uuid: string): Tab => ({ id: remoteTabId(uuid), type: "terminal", title: "T", host: H, terminal: uuid, sessionId: "s1" });

  it("the shown pending Tab gives way to the owner's standalone Tab", () => {
    const owner = tabFromTerminal(H, info("o"));
    const other = tabFromTerminal(H, info("x"));
    const r = adoptTab(state([owner, other, pending("p")]), remoteTabId("p"), "p", "o");
    expect(r.tabs.map(t => t.id)).toEqual([owner.id, other.id]);
    expect(r.removed).toEqual([remoteTabId("p")]);
    expect(r.shown).toBe(true);
    expect(r.focus).toEqual({ activeTabId: owner.id });
  });

  it("A3: a grouped owner is shown in its group, as the group's active leaf", () => {
    const owner = { ...tabFromTerminal(H, info("o")), groupId: "g1" };
    const mate = { ...tabFromTerminal(H, info("m")), groupId: "g1" };
    const g: Group = { id: "g1", name: "Group 1", layout: split(leaf(mate.id), leaf(owner.id)) };
    const r = adoptTab(state([mate, owner, pending("p")], [g], { g1: mate.id }), remoteTabId("p"), "p", "o");
    expect(r.focus).toEqual({ activeTabId: "g1", leaf: { groupId: "g1", tabId: owner.id } });
    expect(r.groups).toEqual([g]);
    expect(focusOf(owner)).toEqual(r.focus);
  });

  it("a pending leaf of a group is removed with its leaf; focus moves within the group", () => {
    const a = { ...tabFromTerminal(H, info("a")), groupId: "g1" };
    const p = { ...pending("p"), groupId: "g1" };
    const b = { ...tabFromTerminal(H, info("b")), groupId: "g1" };
    const g: Group = { id: "g1", name: "Group 1", layout: split(leaf(a.id), split(leaf(p.id), leaf(b.id))) };
    const r = adoptTab(state([a, p, b], [g], { g1: p.id }), "g1", "p", "o");
    expect(r.shown).toBe(true);
    expect(r.groups[0].layout).toEqual(split(leaf(a.id), leaf(b.id)));
    expect(r.activeLeafByGroup.g1).toBe(a.id);
    // The owner is not listed yet: Home stands in until the Host lists it.
    expect(r.focus).toEqual({ activeTabId: "home" });
    expect(r.focusWhenListed).toBe("o");
  });

  // Sol diff review P2: the App's steps after an adoption, in order. Removing a pending pane
  // dissolves its two-pane group; that automatic selection must not cancel the deferred focus.
  for (const grouped of [false, true]) {
    it(`a removed pending pane whose group dissolves still lands on the ${grouped ? "grouped" : "standalone"} owner once listed`, () => {
      const a = { ...tabFromTerminal(H, info("a")), groupId: "g1" };
      const p = { ...pending("p"), groupId: "g1" };
      const g1: Group = { id: "g1", name: "Group 1", layout: split(leaf(p.id), leaf(a.id)) };
      let s = state([p, a], [g1], { g1: p.id });
      let active = "g1";
      // 1. The open of p adopts o, which is not listed yet.
      const r = adoptTab(s, active, "p", "o");
      s = r;
      if (r.focus) active = r.focus.activeTabId;
      let want = r.focusWhenListed;
      if (!keepsFocusWhenListed(active)) want = null;
      // 2. The group, down to one pane, dissolves.
      const d = dissolveSmallGroups(s.tabs, s.groups, active);
      expect(d?.dissolved).toEqual(["g1"]);
      s = { ...s, tabs: d!.tabs, groups: d!.groups };
      active = d!.activeTabId;
      if (!keepsFocusWhenListed(active)) want = null;
      // 3. The Host lists o, inside a restored group or alone.
      const mate = { ...tabFromTerminal(H, info("m")), groupId: grouped ? "g2" : undefined };
      const owner = { ...tabFromTerminal(H, info("o")), groupId: grouped ? "g2" : undefined };
      s = { ...s, tabs: [...s.tabs, mate, owner] };
      // 4. The listed owner takes focus.
      expect(want).toBe("o");
      const t = s.tabs.find(x => x.id === remoteTabId(want!))!;
      expect(focusOf(t)).toEqual(grouped ? { activeTabId: "g2", leaf: { groupId: "g2", tabId: owner.id } } : { activeTabId: owner.id });
    });
  }

  it("a background open never steals focus", () => {
    const owner = tabFromTerminal(H, info("o"));
    const r = adoptTab(state([owner, pending("p")]), "home", "p", "o");
    expect(r.shown).toBe(false);
    expect(r.focus).toBeNull();
    expect(r.tabs).toEqual([owner]);
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

// ADR-0005: a Daemon's Terminals are the source of truth whichever Host it runs on; the
// Local Host is `host` undefined.
describe.each([["Local", undefined], ["Remote", H]] as const)("Host-agnostic reconcile (%s Host)", (_, host) => {
  const other = host ? undefined : H;
  const leaf = (tabId: string) => ({ kind: "leaf" as const, tabId });
  const split = (a: any, b: any) => ({ kind: "split" as const, direction: "col" as const, ratio: 0.5, children: [a, b] as [any, any] });

  it("adds a Tab per new Terminal", () => {
    const d = reconcile([], host, [info("u1")], new Set());
    expect(d.add).toHaveLength(1);
    const t = d.add[0];
    expect(t).toMatchObject({ id: "remote-u1", terminal: "u1" });
    if (host) expect(t.host).toBe(host);
    else expect("host" in t).toBe(false);
  });

  it("removes Tabs whose Terminal is gone, with their group leaves", () => {
    const a = { ...tabFromTerminal(host, info("a")), groupId: "g1" };
    const b = { ...tabFromTerminal(host, info("b")), groupId: "g1" };
    const g: Group = { id: "g1", name: "Group 1", layout: split(leaf(a.id), leaf(b.id)) };
    const d = reconcile([a, b], host, [info("b")], new Set());
    expect(d.remove).toEqual([a.id]);
    const s = applyReconcile(state([a, b], [g]), d);
    expect(s.tabs.map(t => t.id)).toEqual([b.id]);
    expect(s.groups[0].layout).toEqual(leaf(b.id));
  });

  it("keeps the rest, preserving groupId/lastActiveAt and updating title/sessionId", () => {
    const t = { ...tabFromTerminal(host, info("a", {}, { title: "Old" })), groupId: "g1", lastActiveAt: 99 };
    const d = reconcile([t], host, [info("a", { spec: { cwd: "/home/u/proj", sessionId: "s-new", agent: "claude" } }, { title: "New" })], new Set());
    expect(d.update).toHaveLength(1);
    const out = applyReconcile(state([t]), d).tabs[0];
    expect(out).toMatchObject({ id: t.id, title: "New", sessionId: "s-new", groupId: "g1", lastActiveAt: 99 });
    expect(out.host).toBe(host);
  });

  it("keeps pending opens absent from the list and confirms them when listed", () => {
    const opening = tabFromTerminal(host, info("p1"));
    expect(reconcile([opening], host, [], new Set(["p1"])).remove).toEqual([]);
    expect(reconcile([opening], host, [info("p1")], new Set(["p1"])).confirmed).toEqual(["p1"]);
  });

  it("never touches in-process local Tabs (no terminal)", () => {
    const local: Tab = { id: "terminal-x", type: "terminal", title: "L", projectPath: "/home/u/proj", sessionId: "s-a" };
    const d = reconcile([local], host, [], new Set());
    expect(d).toEqual({ add: [], remove: [], update: [], confirmed: [] });
    expect(applyReconcile(state([local]), reconcile([local], host, [info("n")], new Set())).tabs[0]).toBe(local);
  });

  it("leaves the other Host's Daemon Tabs alone", () => {
    const theirs = tabFromTerminal(other, info("o"));
    const d = reconcile([theirs], host, [], new Set());
    expect(d).toEqual({ add: [], remove: [], update: [], confirmed: [] });
  });

  it("is idempotent", () => {
    const list = [info("a", {}, { title: "T" }), info("b", {}, { title: "U" })];
    const s1 = applyReconcile(state([]), reconcile([], host, list, new Set()));
    const d2 = reconcile(s1.tabs, host, list, new Set());
    expect(d2).toEqual({ add: [], remove: [], update: [], confirmed: [] });
    const s2 = applyReconcile(s1, d2);
    expect(s2.tabs).toBe(s1.tabs);
    expect(s2.groups).toBe(s1.groups);
  });

  it("restoreGroupIds restores the groupId of a cached Daemon Tab", () => {
    const t = tabFromTerminal(host, info("r"));
    const local: Tab = { id: "terminal-l", type: "terminal", title: "L", groupId: "g1" };
    const g: Group = { id: "g1", name: "Group 1", layout: split(leaf(local.id), leaf(t.id)) };
    const out = restoreGroupIds([local, t], [g]);
    expect(out[0]).toBe(local);
    expect(out[1].groupId).toBe("g1");
  });
});

describe("reconcileHosts with the Local Host", () => {
  const leaf = (tabId: string) => ({ kind: "leaf" as const, tabId });
  const split = (a: any, b: any) => ({ kind: "split" as const, direction: "col" as const, ratio: 0.5, children: [a, b] as [any, any] });

  it("applies a Local and a Remote list in one transaction", () => {
    const a = { ...tabFromTerminal(undefined, info("a")), groupId: "g1" };
    const b = { ...tabFromTerminal(H, info("b")), groupId: "g1" };
    const c = { ...tabFromTerminal(undefined, info("c")), groupId: "g1" };
    const d: Tab = { id: "terminal-d", type: "terminal", title: "D", groupId: "g1" };
    const g: Group = { id: "g1", name: "Group 1", layout: split(split(leaf(a.id), leaf(b.id)), split(leaf(c.id), leaf(d.id))) };
    const r = reconcileHosts([a, b, c, d], [[undefined, [info("c")]], [H, []]], { pending: new Set() });
    expect(r.removed.sort()).toEqual([a.id, b.id].sort());
    const s = applyHostsReconcile({ tabs: [a, b, c, d], groups: [g], activeLeafByGroup: { g1: a.id } }, r);
    expect(s.tabs.map(t => t.id)).toEqual([c.id, d.id]);
    expect(s.groups[0].layout).toEqual(split(leaf(c.id), leaf(d.id)));
    expect([c.id, d.id]).toContain(s.activeLeafByGroup.g1);
  });
});
