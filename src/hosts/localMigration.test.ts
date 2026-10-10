import { beforeEach, describe, expect, it, vi } from "vitest";
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(), Channel: class {} }));

import {
  CONFIRM_MS, LOCK_WAIT_MS, NO_MIGRATION, REFUSAL_CHECK_MS,
  _resetMigrationOnce, applyLocalMigration, dedupeBySession, groupsToPersist, isRefusal, migrateLocalTabs, migrateLocalTabsOnce, migrationNotice, openTabsToPersist, planLocalMigration, renderedGroups, runLocalMigration, sessionConflict,
  type Journal, type MigrationDeps, type MigrationOp, type OpResult, type StoreState,
} from "./localMigration";
import { persistableTabs, restorableTabs, restoreGroups } from "./appTabs";
import { collectLeafIds } from "../layout";
import { S } from "./strings";
import type { Group, Tab } from "../types";
import { SESSION_CLOSING, SESSION_OPEN, type HostError, type TerminalInfo } from "./types";

const legacy = (id: string, sid: string, over: Partial<Tab> = {}): Tab =>
  ({ id, type: "terminal", title: `T ${id}`, sessionId: sid, projectPath: "/p", projectName: "p", lastActiveAt: 50, ...over });
const split = (id: string, a: string, b: string): Group =>
  ({ id, name: "Group 1", layout: { kind: "split", direction: "row", ratio: 0.5, children: [{ kind: "leaf", tabId: a }, { kind: "leaf", tabId: b }] } });
const info = (uuid: string, createdAtMs: number, over: Partial<TerminalInfo["spec"]> = {}): TerminalInfo =>
  ({ terminal: uuid, spec: { cwd: "/p", agent: "claude", ...over }, meta: { title: uuid }, createdAtMs, pid: 1, exitCode: null });
const uuidOf = (tabs: Tab[]) => Object.fromEntries(tabs.map(t => [t.id, `u-${t.id}`]));
const settings = { fullscreenRendering: true, forceSyncOutput: false };
const hostErr = (code: HostError["code"], message = "x"): HostError => ({ code, message });

describe("planLocalMigration", () => {
  it("an agent Tab becomes a wrapper-less resume of its session, with its title and project", () => {
    const t = legacy("terminal-s1-a", "s1", { createdAt: 7, shellId: "zsh" });
    const p = planLocalMigration({ saved: [t], live: [], uuidOf: uuidOf([t]), now: 99, ...settings });
    expect(p.ops).toEqual([{
      fromId: "terminal-s1-a", uuid: "u-terminal-s1-a",
      spec: { agent: "claude", sessionId: "s1", cwd: "/p", shellMode: "claude", shellId: null, shellCommand: null, fullscreenRendering: true, forceSyncOutput: false },
      meta: { title: "T terminal-s1-a", projectName: "p", createdAt: 7 },
    }]);
    expect(p.ops[0].spec).not.toHaveProperty("skipPermissions");
    expect(p.target).toEqual({ "terminal-s1-a": "u-terminal-s1-a" });
  });

  it("carries skipPermissions only when set; createdAt falls back to lastActiveAt, then now", () => {
    const a = legacy("a", "s1", { skipPermissions: true });
    const b = legacy("b", "s2", { lastActiveAt: undefined, skipPermissions: false });
    const p = planLocalMigration({ saved: [a, b], live: [], uuidOf: uuidOf([a, b]), now: 99, ...settings });
    expect(p.ops[0].spec.skipPermissions).toBe(true);
    expect(p.ops[0].meta.createdAt).toBe(50);
    expect(p.ops[1].spec).not.toHaveProperty("skipPermissions");
    expect(p.ops[1].meta.createdAt).toBe(99);
  });

  it("keeps Codex, Cursor and OpenCode agents; a missing agent is Claude", () => {
    const tabs = [legacy("c", "s1", { agent: "codex" }), legacy("u", "s2", { agent: "cursor" }), legacy("o", "s3", { agent: "opencode" }), legacy("n", "s4")];
    const p = planLocalMigration({ saved: tabs, live: [], uuidOf: uuidOf(tabs), now: 1, ...settings });
    expect(p.ops.map(o => o.spec.agent)).toEqual(["codex", "cursor", "opencode", "claude"]);
  });

  it("raw shells, session-less chats, Remote and Daemon Tabs give no op", () => {
    const tabs: Tab[] = [
      { id: "terminal-shell-1", type: "terminal", title: "bash", projectPath: "/p", shellMode: "raw", shellId: "bash" },
      { id: "terminal-new-1", type: "terminal", title: "New Chat", projectPath: "/p", agent: "codex" },
      legacy("remote-r", "s1", { host: "h_ab12cd34", terminal: "r" }),
      legacy("remote-l", "s2", { terminal: "l" }),
    ];
    const p = planLocalMigration({ saved: tabs, live: [], uuidOf: uuidOf(tabs), now: 1, ...settings });
    expect(p.ops).toEqual([]);
    expect(p.target).toEqual({});
  });

  it("a UUID already listed is adopted without an open (re-run after a crash)", () => {
    const t = legacy("a", "s1");
    const p = planLocalMigration({ saved: [t], live: [info("u-a", 5, { sessionId: "s1" })], uuidOf: uuidOf([t]), now: 1, ...settings });
    expect(p.ops).toEqual([]);
    expect(p.target).toEqual({ a: "u-a" });
  });

  it("M6/M8: a session already listed under another UUID maps to that Terminal; a missing agent is Claude", () => {
    const t = legacy("a", "s1");
    const p = planLocalMigration({ saved: [t], live: [info("x", 5, { sessionId: "s1", agent: null })], uuidOf: uuidOf([t]), now: 1, ...settings });
    expect(p.ops).toEqual([]);
    expect(p.target).toEqual({ a: "x" });
    // Another agent's session with the same id is a different session.
    const c = legacy("c", "s1", { agent: "codex" });
    expect(planLocalMigration({ saved: [c], live: [info("x", 5, { sessionId: "s1" })], uuidOf: uuidOf([c]), now: 1, ...settings }).ops).toHaveLength(1);
  });

  it("M6: two saved Tabs on one session open it once; the second maps to the first", () => {
    const a = legacy("a", "s1"), b = legacy("b", "s1", { agent: "claude" });
    const p = planLocalMigration({ saved: [a, b], live: [], uuidOf: uuidOf([a, b]), now: 1, ...settings });
    expect(p.ops.map(o => o.fromId)).toEqual(["a"]);
    expect(p.target).toEqual({ a: "u-a", b: "u-a" });
  });
});

// A fake local Daemon: opens create listed Terminals with increasing createdAtMs.
class FakeDaemon {
  list: TerminalInfo[] = [];
  opened: string[] = [];
  clock = 1000;
  // Per UUID: how the open behaves.
  behave: Record<string, { error?: HostError; create?: boolean; appearAfterMs?: number; createdAtMs?: number }> = {};
  open = async (op: MigrationOp) => {
    this.opened.push(op.uuid);
    const b = this.behave[op.uuid] ?? {};
    const create = b.create ?? !b.error;
    if (create) {
      if (this.list.some(i => i.terminal === op.uuid)) throw hostErr("remote", `terminal ${op.uuid} already exists`);
      this.list.push({ terminal: op.uuid, spec: op.spec, meta: { ...op.meta }, createdAtMs: b.createdAtMs ?? this.clock++, pid: 1, exitCode: null });
    }
    if (b.error) throw b.error;
  };
  // A Terminal that shows up `appearAfterMs` after the open counts only if the wait is that long.
  listed = async (uuid: string, ms: number) => this.list.some(i => i.terminal === uuid) && (this.behave[uuid]?.appearAfterMs ?? 0) <= ms;
  live = () => this.list.filter(i => (this.behave[i.terminal]?.appearAfterMs ?? 0) <= CONFIRM_MS);
  close(uuid: string) { this.list = this.list.filter(i => i.terminal !== uuid); }
}

describe("runLocalMigration", () => {
  const ops = (...ids: string[]): MigrationOp[] => ids.map(id => ({ fromId: id, uuid: `u-${id}`, spec: { cwd: "/p" }, meta: {} }));

  it("opens one after another, in order", async () => {
    const d = new FakeDaemon();
    let inFlight = 0, maxInFlight = 0;
    const open = async (op: MigrationOp) => { inFlight++; maxInFlight = Math.max(maxInFlight, inFlight); await new Promise(r => setTimeout(r, 1)); await d.open(op); inFlight--; };
    const r = await runLocalMigration(ops("a", "b", "c"), { open, listed: d.listed });
    expect(d.opened).toEqual(["u-a", "u-b", "u-c"]);
    expect(maxInFlight).toBe(1);
    expect(r).toEqual({ "u-a": "ok", "u-b": "ok", "u-c": "ok" });
  });

  it("M2: \"already exists\" or a timeout counts as ok once listed, also beyond 3 s", async () => {
    const d = new FakeDaemon();
    d.list.push(info("u-a", 1));
    d.behave["u-b"] = { error: hostErr("timeout"), create: true, appearAfterMs: 5000 };
    const r = await runLocalMigration(ops("a", "b"), d);
    expect(r).toEqual({ "u-a": "ok", "u-b": "ok" });
  });

  it("M2: open succeeded but the attach failed: listed, so ok", async () => {
    const d = new FakeDaemon();
    d.behave["u-a"] = { error: hostErr("remote", "unknown terminal"), create: true };
    expect(await runLocalMigration(ops("a"), d)).toEqual({ "u-a": "ok" });
  });

  it("a refusal of the Daemon is failed; the next Tabs still open", async () => {
    const d = new FakeDaemon();
    d.behave["u-a"] = { error: hostErr("remote", "terminal list too large (9 bytes, max 1)") };
    const seen: number[] = [];
    const r = await runLocalMigration(ops("a", "b"), { open: d.open, listed: (u, ms) => { seen.push(ms); return d.listed(u, ms); } });
    expect(r).toEqual({ "u-a": "failed", "u-b": "ok" });
    expect(seen[0]).toBe(REFUSAL_CHECK_MS);
  });

  it("M2: an unconfirmed open is unresolved, and the rest are not sent (so they cannot exist)", async () => {
    const d = new FakeDaemon();
    d.behave["u-a"] = { error: hostErr("offline", "link closed"), create: false };
    const r = await runLocalMigration(ops("a", "b", "c"), d);
    expect(r).toEqual({ "u-a": "unresolved", "u-b": "failed", "u-c": "failed" });
    expect(d.opened).toEqual(["u-a"]);
  });

  it("xshell#41: a session another client runs is held, never failed; its listed owner is reported", async () => {
    const d = new FakeDaemon();
    const owner = "0f0e0d0c-0000-4000-8000-000000000001";
    d.list.push(info(owner, 1, { sessionId: "s1" }));
    d.behave["u-a"] = { error: hostErr("remote", `${SESSION_OPEN}: ${owner}`) };
    d.behave["u-b"] = { error: hostErr("remote", SESSION_CLOSING) };
    d.behave["u-c"] = { error: hostErr("remote", `${SESSION_OPEN}: 0f0e0d0c-0000-4000-8000-0000000000ff`) };
    const owners: Record<string, string> = {};
    const r = await runLocalMigration(ops("a", "b", "c", "d"), d, owners);
    expect(r).toEqual({ "u-a": "held", "u-b": "held", "u-c": "held", "u-d": "ok" });
    // Only a Terminal confirmed in the list is the owner.
    expect(owners).toEqual({ "u-a": owner });
  });

  it("classifies refusals", () => {
    expect(isRefusal(hostErr("remote", SESSION_OPEN))).toBe(false);
    expect(isRefusal(hostErr("remote", `${SESSION_OPEN}: 0f0e0d0c-0000-4000-8000-000000000001`))).toBe(false);
    expect(isRefusal(hostErr("remote", SESSION_CLOSING))).toBe(false);
    expect(sessionConflict(hostErr("remote", `${SESSION_OPEN}: 0F0E0D0C-0000-4000-8000-000000000001`))).toEqual({ owner: "0f0e0d0c-0000-4000-8000-000000000001" });
    expect(sessionConflict(hostErr("remote", SESSION_OPEN))).toEqual({ owner: null });
    expect(sessionConflict(hostErr("remote", "no such dir"))).toBeNull();
    expect(isRefusal(hostErr("invalid"))).toBe(true);
    expect(isRefusal(hostErr("unknown-host"))).toBe(true);
    expect(isRefusal(hostErr("remote", "cannot save the terminal list: x"))).toBe(true);
    expect(isRefusal(hostErr("remote", "terminal u already exists"))).toBe(false);
    for (const c of ["timeout", "offline", "busy", "incompatible", "indeterminate"] as const) expect(isRefusal(hostErr(c))).toBe(false);
    expect(isRefusal(new Error("boom"))).toBe(false);
  });
});

describe("applyLocalMigration", () => {
  const run = (saved: Tab[], groups: Group[], live: TerminalInfo[], results: Record<string, OpResult>, zoom: Record<string, number> = {}, planLive: TerminalInfo[] = []) => {
    const plan = planLocalMigration({ saved, live: planLive, uuidOf: uuidOf(saved), now: 1, ...settings });
    return applyLocalMigration({ saved, groups, zoom, plan, results, live });
  };

  it("rewrites group leaves; a split of two migrated Tabs survives restoreGroups", () => {
    const a = legacy("a", "s1", { groupId: "g" }), b = legacy("b", "s2", { groupId: "g" });
    const r = run([a, b], [split("g", "a", "b")], [info("u-a", 10, { sessionId: "s1" }), info("u-b", 11, { sessionId: "s2" })], { "u-a": "ok", "u-b": "ok" });
    expect(r.migrated.map(t => t.terminal)).toEqual(["u-a", "u-b"]);
    expect(collectLeafIds(r.groups[0].layout)).toEqual(["remote-u-a", "remote-u-b"]);
    const restored = restoreGroups(restorableTabs({ saved: r.inProcess, hosts: [], localDaemon: true, cached: () => null, migrated: r.migrated }), r.groups);
    expect(restored.groups.map(g => g.id)).toEqual(["g"]);
    expect(restored.tabs.map(t => [t.id, t.groupId])).toEqual([["remote-u-a", "g"], ["remote-u-b", "g"]]);
  });

  it("mixed result: the refused Tab stays in-process with its old id, in open_tabs, and the group is kept", () => {
    const a = legacy("a", "s1", { groupId: "g" }), b = legacy("b", "s2", { groupId: "g" });
    const r = run([a, b], [split("g", "a", "b")], [info("u-a", 10, { sessionId: "s1" })], { "u-a": "ok", "u-b": "failed" });
    expect(r.inProcess).toEqual([b]);
    expect(r.heldBack).toEqual([]);
    expect(collectLeafIds(r.groups[0].layout)).toEqual(["remote-u-a", "b"]);
    const restorable = restorableTabs({ saved: r.inProcess, hosts: [], localDaemon: true, cached: () => null, migrated: r.migrated });
    const restored = restoreGroups(restorable, r.groups);
    expect(restored.groups).toHaveLength(1);
    expect(persistableTabs(restored.tabs).map(t => t.id)).toEqual(["b"]);
  });

  it("M2/F: an unresolved Tab is held back (not run, not dropped); its leaf stays in the saved layout, pruned only where shown", () => {
    const a = legacy("a", "s1"), b = legacy("b", "s2"), c = legacy("c", "s3");
    const groups = [{ id: "g", name: "Group 1", layout: { kind: "split", direction: "row", ratio: 0.5, children: [{ kind: "leaf", tabId: "a" }, split("x", "b", "c").layout] } } as Group];
    const r = run([a, b, c], groups, [info("u-a", 10), info("u-c", 12)], { "u-a": "ok", "u-b": "unresolved", "u-c": "ok" });
    expect(r.heldBack).toEqual([b]);
    expect(r.inProcess).toEqual([]);
    expect(collectLeafIds(r.groups[0].layout)).toEqual(["remote-u-a", "b", "remote-u-c"]);
    expect(renderedGroups(r.groups, r.heldBack).map(g => collectLeafIds(g.layout))).toEqual([["remote-u-a", "remote-u-c"]]);
  });

  it("M8: a Tab mapped to a listed Terminal of its session takes its place; a duplicate leaf is removed", () => {
    const a = legacy("a", "s1"), m = legacy("m", "s2");
    const live = [info("x", 5, { sessionId: "s1" })];
    const r = run([a, m], [split("g", "a", "m")], [...live, info("u-m", 10, { sessionId: "s2" })], { "u-m": "ok" }, { a: 18 }, live);
    expect(collectLeafIds(r.groups[0].layout)).toEqual(["remote-x", "remote-u-m"]);
    expect(r.migrated.map(t => t.terminal)).toEqual(["x", "u-m"]);
    expect(r.zoom).toEqual({ "remote-x": 18 });
    // Two saved Tabs on one session in one group: one leaf remains.
    const b = legacy("b", "s1");
    const d = run([a, b], [split("g", "a", "b")], [info("u-a", 10, { sessionId: "s1" })], { "u-a": "ok" });
    expect(d.migrated.map(t => t.terminal)).toEqual(["u-a"]);
    expect(collectLeafIds(d.groups[0].layout)).toEqual(["remote-u-a"]);
    // The target already a leaf of another group: the moved leaf is removed, not duplicated.
    const e = run([a], [split("g1", "remote-x", "remote-y"), split("g2", "a", "remote-z")], live, {}, {}, live);
    expect(e.groups.map(g => collectLeafIds(g.layout))).toEqual([["remote-x", "remote-y"], ["remote-z"]]);
  });

  it("a duplicate of a refused session waits instead of running a second process", () => {
    const a = legacy("a", "s1"), b = legacy("b", "s1");
    const r = run([a, b], [], [], { "u-a": "failed" });
    expect(r.inProcess).toEqual([a]);
    expect(r.heldBack).toEqual([b]);
  });

  it("zoom moves from the old id to the new one", () => {
    const a = legacy("a", "s1"), b = legacy("b", "s2");
    const r = run([a, b], [], [info("u-a", 10)], { "u-a": "ok", "u-b": "failed" }, { a: 18, b: 12, other: 9 });
    expect(r.zoom).toEqual({ "remote-u-a": 18, b: 12, other: 9 });
  });

  it("M9: order is the Daemon's createdAtMs, ties by UUID, never the saved order", () => {
    const a = legacy("a", "s1"), b = legacy("b", "s2"), c = legacy("c", "s3");
    const r = run([c, b, a], [], [info("u-c", 20), info("u-b", 10), info("u-a", 10)], { "u-a": "ok", "u-b": "ok", "u-c": "ok" });
    expect(r.migrated.map(t => t.terminal)).toEqual(["u-a", "u-b", "u-c"]);
  });
});

// ── Orchestrator ────────────────────────────────────────────────────

interface Disk { open_tabs?: Tab[]; open_groups?: Group[]; terminal_zoom?: Record<string, number>; journal?: Journal }
const lockState = { holder: null as string | null };

function instance(name: string, disk: Disk, d: FakeDaemon, over: Partial<MigrationDeps> = {}) {
  const writes: string[] = [];
  let guarded = false;
  const deps: MigrationDeps = {
    lock: async () => { if (lockState.holder && lockState.holder !== name) return false; lockState.holder = name; return true; },
    unlock: async () => { if (lockState.holder === name) lockState.holder = null; },
    guard: async () => { guarded = true; },
    read: async () => structuredClone({ openTabs: disk.open_tabs, openGroups: disk.open_groups, zoom: disk.terminal_zoom }),
    write: async (l) => { writes.push("settings"); disk.open_tabs = l.openTabs; disk.open_groups = l.openGroups; disk.terminal_zoom = l.zoom; },
    readJournal: async () => structuredClone(disk.journal ?? null),
    writeJournal: async (j) => { writes.push(j.base ? "journal" : `journal sent=${j.sent.join(",")}`); disk.journal = structuredClone(j); },
    clearJournal: async () => { if (disk.journal) writes.push("clear"); delete disk.journal; },
    ready: async () => d.live(),
    cached: () => null,
    live: () => d.live(),
    listed: d.listed,
    open: d.open,
    uuid: async (id) => `u-${id}`,
    now: () => 1,
    ...over,
  };
  const snapshot = (): StoreState => structuredClone({ openTabs: disk.open_tabs, openGroups: disk.open_groups, zoom: disk.terminal_zoom });
  return { deps, writes, snapshot, isGuarded: () => guarded, start: (snap = snapshot()) => migrateLocalTabs(snap, settings, deps) };
}

// Restore as App does it, from an outcome.
const restoreFrom = (o: Awaited<ReturnType<typeof migrateLocalTabs>>, cached: TerminalInfo[] | null, saved: Group[] = []) => {
  const durable = o.groups ?? saved;
  const r = restoreGroups(restorableTabs({ saved: o.inProcess, hosts: [], localDaemon: true, cached: () => cached, migrated: o.migrated }), renderedGroups(durable, o.heldBack));
  return { ...r, durable };
};

describe("migrateLocalTabs", () => {
  beforeEach(() => { lockState.holder = null; _resetMigrationOnce(); });

  it("does nothing (no lock, no wait) without saved in-process Tabs or a journal", async () => {
    const d = new FakeDaemon();
    const lock = vi.fn(async () => true);
    const i = instance("A", { open_tabs: [{ id: "terminal-shell-1", type: "terminal", title: "bash", shellMode: "raw" }] }, d, { lock });
    expect(await i.start()).toBe(NO_MIGRATION);
    expect(lock).not.toHaveBeenCalled();
  });

  it("migrates groups and splits: journal first, then the settings, journal cleared once applied; the next start restores the same Tabs", async () => {
    const d = new FakeDaemon();
    const a = legacy("a", "s1", { groupId: "g" }), b = legacy("b", "s2", { groupId: "g" }), c = legacy("c", "s3");
    const disk: Disk = { open_tabs: [a, b, c, { id: "terminal-new-1", type: "terminal", title: "New Chat", projectPath: "/p" }], open_groups: [split("g", "a", "b")], terminal_zoom: { a: 16 } };
    const A = instance("A", disk, d);
    const o = await A.start();
    expect(d.opened).toEqual(["u-a", "u-b", "u-c"]);
    expect(o.migrated.map(i => i.terminal)).toEqual(["u-a", "u-b", "u-c"]);
    expect(o.inProcess).toEqual([]);
    expect(o.ownsOpenTabs).toBe(false);
    expect(lockState.holder).toBe("A"); // held until settled (4)
    expect(A.writes).toEqual(["journal", "settings"]);
    expect(disk.journal?.base?.openTabs).toHaveLength(4); // still there until restore applied it
    const r1 = restoreFrom(o, null);
    await o.settle();
    await o.settle(); // once
    expect(lockState.holder).toBeNull();
    expect(A.writes).toEqual(["journal", "settings", "clear"]);
    expect(disk).toEqual({ open_tabs: [], open_groups: [split("g", "remote-u-a", "remote-u-b")], terminal_zoom: { "remote-u-a": 16 } });
    expect(r1.tabs.map(t => [t.id, t.groupId])).toEqual([["remote-u-a", "g"], ["remote-u-b", "g"], ["remote-u-c", undefined]]);
    expect(openTabsToPersist(persistableTabs(r1.tabs), o)).toBeNull();
    expect(groupsToPersist(r1.groups, r1.tabs, r1.durable, o)).toEqual(r1.groups);
    // Next restart: nothing to migrate; the cached list restores the same ids and groups, no duplicates.
    const o2 = await instance("A2", disk, d).start();
    expect(o2).toBe(NO_MIGRATION);
    const r2 = restoreFrom(o2, d.list, disk.open_groups);
    expect(r2.tabs.map(t => [t.id, t.groupId])).toEqual(r1.tabs.map(t => [t.id, t.groupId]));
    expect(d.opened).toHaveLength(3);
  });

  it("partial: a refused Tab runs in-process, keeps the lock and open_tabs; the next start moves it, merged in the Daemon's order (G)", async () => {
    const d = new FakeDaemon();
    const a = legacy("a", "s1"), b = legacy("b", "s2"), c = legacy("c", "s3");
    d.behave["u-b"] = { error: hostErr("remote", "terminal list too large") };
    const disk: Disk = { open_tabs: [a, b, c], open_groups: [split("g", "a", "b")] };
    const o = await instance("A", disk, d).start();
    await o.settle();
    expect(o.inProcess).toEqual([b]);
    expect(o.failed).toBe(1);
    expect(migrationNotice(o.failed)).toBe(S["notice.localMigration.partialOne"]);
    expect(o.ownsOpenTabs).toBe(true);
    expect(lockState.holder).toBe("A");
    expect(disk.open_tabs).toEqual([b]);
    expect(disk.journal).toBeUndefined(); // refused: no Terminal can exist
    expect(collectLeafIds(disk.open_groups![0].layout)).toEqual(["remote-u-a", "b"]);
    expect(openTabsToPersist([b], o)).toEqual([b]);
    lockState.holder = null;
    delete d.behave["u-b"];
    // b's Terminal is created at the same createdAtMs as a's: the tie goes by UUID.
    d.behave["u-b"] = { createdAtMs: d.list[0].createdAtMs };
    const o2 = await instance("A2", disk, d).start();
    expect(d.opened).toEqual(["u-a", "u-b", "u-c", "u-b"]); // the refused open, then the retry
    expect(o2.inProcess).toEqual([]);
    expect(o2.migrated.map(i => i.terminal)).toEqual(["u-b"]);
    const cached = d.list.filter(i => i.terminal !== "u-b");
    const r = restoreFrom(o2, cached);
    expect(r.tabs.map(t => t.id)).toEqual(["remote-u-a", "remote-u-b", "remote-u-c"]);
    expect(r.groups.map(g => collectLeafIds(g.layout))).toEqual([["remote-u-a", "remote-u-b"]]);
  });

  it("A1: a Mobile opens the session between the snapshot and the open: the Tab becomes the Mobile's Terminal, never in-process", async () => {
    const d = new FakeDaemon();
    const a = legacy("a", "s1", { groupId: "g" }), b = legacy("b", "s2", { groupId: "g" });
    const owner = "0f0e0d0c-0000-4000-8000-000000000001";
    const disk: Disk = { open_tabs: [a, b], open_groups: [split("g", "a", "b")] };
    // The snapshot's list is empty; the Mobile's open lands just before the migration's.
    const open = async (op: MigrationOp) => {
      if (op.uuid === "u-a") {
        d.list.push(info(owner, 5, { sessionId: "s1" }));
        throw hostErr("remote", `${SESSION_OPEN}: ${owner}`);
      }
      await d.open(op);
    };
    const o = await instance("A", disk, d, { open }).start();
    await o.settle();
    expect(o.inProcess).toEqual([]);
    expect(o.heldBack).toEqual([]);
    expect(o.failed).toBe(0);
    expect(o.migrated.map(i => i.terminal)).toEqual([owner, "u-b"]);
    expect(disk.open_groups).toEqual([split("g", `remote-${owner}`, "remote-u-b")]);
    const r = restoreFrom(o, null);
    expect(r.tabs.map(t => t.id)).toEqual([`remote-${owner}`, "remote-u-b"]);
    expect(d.list.filter(i => i.spec.sessionId === "s1")).toHaveLength(1);
  });

  it("A1: a session still closing elsewhere is held back for the next start, never in-process", async () => {
    const d = new FakeDaemon();
    const a = legacy("a", "s1"), b = legacy("b", "s2");
    d.behave["u-a"] = { error: hostErr("remote", SESSION_CLOSING) };
    const disk: Disk = { open_tabs: [a, b] };
    const o = await instance("A", disk, d).start();
    await o.settle();
    expect(o.inProcess).toEqual([]);
    expect(o.heldBack).toEqual([a]);
    expect(o.migrated.map(i => i.terminal)).toEqual(["u-b"]);
    expect(disk.journal?.sent).toEqual(["a"]);
    expect(disk.open_tabs).toEqual([a]);
    // The next start finds the session free and moves the Tab.
    lockState.holder = null;
    delete d.behave["u-a"];
    const o2 = await instance("A2", disk, d).start();
    expect(o2.migrated.map(i => i.terminal)).toEqual(["u-a"]);
  });

  it("deferred without a journal: one in-process Tab per session, the duplicates wait; the lock stays held", async () => {
    const d = new FakeDaemon();
    const a = legacy("a", "s1"), b = legacy("b", "s1"), c = legacy("c", "s2");
    const disk: Disk = { open_tabs: [a, b, c] };
    const A = instance("A", disk, d, { ready: async () => null });
    const o = await A.start();
    expect(o).toMatchObject({ inProcess: [a, c], heldBack: [b], migrated: [], ownsOpenTabs: true, failed: 0 });
    expect(d.opened).toEqual([]);
    expect(A.writes).toEqual([]);
    expect(lockState.holder).toBe("A");
    expect(openTabsToPersist([a, c], o)).toEqual([a, c, b]);
    expect(dedupeBySession([a, b, c])).toEqual({ keep: [a, c], dups: [b] });
  });

  it("C: deferred after a crashed run: Tabs the journal says may exist are held back, never run in-process", async () => {
    const d = new FakeDaemon();
    const a = legacy("a", "s1"), b = legacy("b", "s2");
    const disk: Disk = { open_tabs: [a, b], open_groups: [split("g", "a", "b")] };
    // The run crashed after the write-ahead journal and a's open.
    await instance("A", disk, d, { open: async (op) => { await d.open(op); throw new Error("crash"); }, listed: async () => { throw new Error("crash"); } }).start();
    lockState.holder = null;
    expect(disk.journal?.sent).toEqual(["a", "b"]);
    // Meanwhile the settings were rewritten to anything: the journal's base wins.
    disk.open_tabs = [];
    const o = await instance("A2", disk, d, { ready: async () => null }).start();
    expect(o.inProcess).toEqual([]);
    expect(o.heldBack).toEqual([a, b]);
    expect(o.groups).toEqual([split("g", "a", "b")]);
    expect(openTabsToPersist([], o)).toEqual([a, b]);
    const r = restoreFrom(o, null);
    expect(r.tabs).toEqual([]);
    // F: the saved layout keeps the held-back leaves.
    expect(groupsToPersist(r.groups, r.tabs, r.durable, o)).toEqual([split("g", "a", "b")]);
  });

  it("M5/D: B while A runs a deferred legacy Tab leaves it alone and never writes the saved state", async () => {
    const d = new FakeDaemon();
    const disk: Disk = { open_tabs: [legacy("a", "s1"), legacy("b", "s2")], open_groups: [split("g", "a", "b")] };
    await instance("A", disk, d, { ready: async () => null }).start();
    const lockWaits: number[] = [];
    const B = instance("B", disk, d, { lock: async (ms) => { lockWaits.push(ms); return false; } });
    const o = await B.start();
    expect(lockWaits).toEqual([LOCK_WAIT_MS]);
    expect(B.isGuarded()).toBe(true);
    expect(o.inProcess).toEqual([]);
    expect(o.ownsOpenTabs).toBe(false);
    expect(o.writesLayout).toBe(false);
    expect(openTabsToPersist([], o)).toBeNull();
    const r = restoreFrom(o, null);
    expect(groupsToPersist(r.groups, r.tabs, r.durable, o)).toBeNull();
    expect(d.opened).toEqual([]);
    expect(disk.open_tabs).toHaveLength(2);
    expect(disk.open_groups).toEqual([split("g", "a", "b")]);
  });

  it("M5/E: a stale B after A migrated and closed a Tab reopens nothing (the key is read fresh)", async () => {
    const d = new FakeDaemon();
    const disk: Disk = { open_tabs: [legacy("a", "s1"), legacy("b", "s2")] };
    const stale = structuredClone({ openTabs: disk.open_tabs, openGroups: undefined, zoom: undefined });
    const oa = await instance("A", disk, d).start();
    await oa.settle();
    d.close("u-a"); // A's user closes one migrated Tab
    // A deletes the key altogether between B's initial load and B's locked read.
    delete disk.open_tabs;
    const o = await instance("B", disk, d).start(stale);
    expect(d.opened).toEqual(["u-a", "u-b"]);
    expect(o.inProcess).toEqual([]);
    expect(o.migrated).toEqual([]);
  });

  it("M3: a crash after the settings write, before restore: the restart re-applies the journal against the live list, with an empty cache keeps the groups", async () => {
    const d = new FakeDaemon();
    const disk: Disk = { open_tabs: [legacy("a", "s1"), legacy("b", "s2")], open_groups: [split("g", "a", "b")] };
    await instance("A", disk, d).start(); // never settled: the app died before restore
    lockState.holder = null;
    expect(disk.journal?.base).toBeDefined();
    const A2 = instance("A2", disk, d);
    const o = await A2.start();
    expect(A2.writes).toEqual(["journal", "settings"]);
    expect(disk.open_groups).toEqual([split("g", "remote-u-a", "remote-u-b")]);
    const r = restoreFrom(o, null);
    expect(r.groups.map(g => g.id)).toEqual(["g"]);
    expect(r.tabs.map(t => t.id)).toEqual(["remote-u-a", "remote-u-b"]);
    expect(d.opened).toEqual(["u-a", "u-b"]);
    await o.settle();
    expect(disk.journal).toBeUndefined();
  });

  it("M3: a crash between two opens re-runs safely from the journal: each session opens once", async () => {
    const d = new FakeDaemon();
    const disk: Disk = { open_tabs: [legacy("a", "s1"), legacy("b", "s2")], open_groups: [split("g", "a", "b")] };
    const open = async (op: MigrationOp) => { if (op.uuid === "u-b") throw new Error("crash"); return d.open(op); };
    const crashed = instance("A", disk, d, { open, listed: async (u, ms) => { if (u === "u-b") throw new Error("crash"); return d.listed(u, ms); } });
    expect((await crashed.start()).inProcess).toEqual([]);
    expect(crashed.writes).toEqual(["journal"]);
    expect(disk.open_tabs).toHaveLength(2);
    lockState.holder = null;
    const o = await instance("A2", disk, d).start();
    expect(d.opened).toEqual(["u-a", "u-b"]);
    expect(o.migrated.map(i => i.terminal)).toEqual(["u-a", "u-b"]);
    expect(disk.open_groups).toEqual([split("g", "remote-u-a", "remote-u-b")]);
  });

  it("re-run idempotence: after a migration every Tab is listed, so planning again gives zero ops", async () => {
    const d = new FakeDaemon();
    const tabs = [legacy("a", "s1"), legacy("b", "s2")];
    await instance("A", { open_tabs: tabs }, d).start();
    expect(planLocalMigration({ saved: tabs, live: d.list, uuidOf: uuidOf(tabs), now: 1, ...settings }).ops).toEqual([]);
  });

  it("M2: an unresolved Tab is held back, kept in open_tabs and the journal; a later deferred start still holds it back; the next ready one adopts it", async () => {
    const d = new FakeDaemon();
    const a = legacy("a", "s1"), b = legacy("b", "s2");
    d.behave["u-a"] = { error: hostErr("timeout"), create: true, appearAfterMs: CONFIRM_MS + 1 };
    const disk: Disk = { open_tabs: [a, b], open_groups: [split("g", "a", "b")] };
    const A = instance("A", disk, d);
    const o = await A.start();
    await o.settle();
    expect(o.heldBack).toEqual([a]);
    expect(o.inProcess).toEqual([b]); // never sent, so it runs in-process this run
    expect(o.ownsOpenTabs).toBe(true);
    expect(disk.open_tabs).toEqual([a, b]);
    expect(disk.journal).toEqual({ version: 1, sent: ["a"] });
    expect(openTabsToPersist([b], o)).toEqual([b, a]);
    expect(collectLeafIds(disk.open_groups![0].layout)).toEqual(["a", "b"]);
    lockState.holder = null;
    const od = await instance("A2", disk, d, { ready: async () => null }).start();
    expect(od.heldBack).toEqual([a]);
    expect(od.inProcess).toEqual([b]);
    lockState.holder = null;
    d.behave["u-a"] = {};
    const o2 = await instance("A3", disk, d).start();
    await o2.settle();
    expect(d.opened).toEqual(["u-a", "u-b"]);
    expect(o2.migrated.map(i => i.terminal)).toEqual(["u-a", "u-b"]);
    expect(disk.journal).toBeUndefined();
  });

  it("1: settings.json unreadable (torn) with a journal: the keys are rebuilt from its base and the run continues", async () => {
    const d = new FakeDaemon();
    const disk: Disk = { open_tabs: [legacy("a", "s1")], open_groups: [] };
    await instance("A", disk, d).start(); // crashed before restore
    lockState.holder = null;
    const torn = instance("A2", disk, d, { read: async () => { throw new Error("EOF while parsing"); } });
    const o = await torn.start({ openTabs: undefined, openGroups: undefined, zoom: undefined });
    expect(torn.writes).toEqual(["settings", "journal", "settings"]);
    expect(o.migrated.map(i => i.terminal)).toEqual(["u-a"]);
    expect(d.opened).toEqual(["u-a"]);
    await o.settle();
    expect(disk.journal).toBeUndefined();
    expect(disk.open_tabs).toEqual([]);
  });

  it("1: a journal that cannot be read is an error: nothing restored, nothing written", async () => {
    const d = new FakeDaemon();
    const A = instance("A", { open_tabs: [legacy("a", "s1")] }, d, { readJournal: async () => { throw new Error("EIO"); } });
    const o = await A.start();
    expect(o).toMatchObject({ inProcess: [], ownsOpenTabs: false, writesLayout: false });
    expect(d.opened).toEqual([]);
    expect(A.writes).toEqual([]);
  });

  it("3: deferred with a cached Local list: a session it runs waits, the others run in-process", async () => {
    const d = new FakeDaemon();
    const a = legacy("a", "s1"), b = legacy("b", "s2");
    const A = instance("A", { open_tabs: [a, b] }, d, { ready: async () => null, cached: () => [info("x", 1, { sessionId: "s1" })] });
    const o = await A.start();
    expect(o.inProcess).toEqual([b]);
    expect(o.heldBack).toEqual([a]);
  });

  it("4/5: a failing settlement leaves the journal and the lock in place for the next start", async () => {
    const d = new FakeDaemon();
    const disk: Disk = { open_tabs: [legacy("a", "s1")] };
    const A = instance("A", disk, d, { clearJournal: async () => { throw new Error("EIO"); } });
    const o = await A.start();
    await expect(o.settle()).rejects.toThrow("EIO");
    expect(disk.journal?.base).toBeDefined();
    expect(lockState.holder).toBe("A");
  });

  it("a failing store read shows no saved Tab and never writes the saved state", async () => {
    const d = new FakeDaemon();
    const A = instance("A", { open_tabs: [legacy("a", "s1")] }, d, { read: async () => { throw new Error("io"); } });
    const o = await A.start();
    expect(o).toMatchObject({ inProcess: [], ownsOpenTabs: false, writesLayout: false });
    expect(A.isGuarded()).toBe(true);
    expect(lockState.holder).toBeNull();
  });

  it("M7: overlapping starts share one run, also when its result is delayed", async () => {
    const d = new FakeDaemon();
    let release!: () => void;
    const gate = new Promise<void>(r => { release = r; });
    const disk: Disk = { open_tabs: [legacy("a", "s1")] };
    const A = instance("A", disk, d, { ready: async () => { await gate; return d.live(); } });
    const first = migrateLocalTabsOnce(A.snapshot(), settings, A.deps);
    const second = migrateLocalTabsOnce(A.snapshot(), settings, A.deps);
    release();
    const [o1, o2] = await Promise.all([first, second]);
    expect(o1).toBe(o2);
    expect(d.opened).toEqual(["u-a"]);
    expect(A.writes).toEqual(["journal", "settings"]);
  });
});

describe("groupsToPersist (F)", () => {
  const held = [legacy("h", "s9")];
  const o = { heldBack: held, writesLayout: true };
  const three: Group = { id: "g", name: "Group 1", layout: { kind: "split", direction: "row", ratio: 0.5, children: [{ kind: "leaf", tabId: "a" }, split("x", "b", "h").layout] } };
  const tab = (id: string): Tab => ({ id, type: "terminal", title: id });

  it("writes the saved group while its shown part is unchanged", () => {
    const shown = renderedGroups([three], held);
    expect(shown.map(g => collectLeafIds(g.layout))).toEqual([["a", "b"]]);
    expect(groupsToPersist(shown, [tab("a"), tab("b")], [three], o)).toEqual([three]);
  });

  it("6: ratio and direction edits of the shown splits merge into the saved tree", () => {
    const shown: Group = { ...split("g", "a", "b"), layout: { ...split("g", "a", "b").layout, ratio: 0.3, direction: "col" } as Group["layout"] };
    const out = groupsToPersist([shown], [tab("a"), tab("b")], [three], o)!;
    expect(collectLeafIds(out[0].layout)).toEqual(["a", "b", "h"]);
    expect(out[0].layout).toMatchObject({ ratio: 0.3, direction: "col" });
  });

  it("6: a restructured shown group keeps its new layout with the held-back leaves appended", () => {
    const out = groupsToPersist([split("g", "a", "c")], [tab("a"), tab("c")], [three], o)!;
    expect(collectLeafIds(out[0].layout)).toEqual(["a", "c", "h"]);
    expect((out[0].layout as { children: Group["layout"][] }).children[0]).toEqual(split("g", "a", "c").layout);
  });

  it("6: a saved group whose shown leaves are gone keeps its held-back leaves when two or more", () => {
    const two = [legacy("h", "s9"), legacy("i", "s8")];
    const g: Group = { id: "g", name: "Group 1", layout: { kind: "split", direction: "row", ratio: 0.5, children: [{ kind: "leaf", tabId: "a" }, split("x", "h", "i").layout] } };
    expect(groupsToPersist([], [], [g], { heldBack: two, writesLayout: true })!.map(x => collectLeafIds(x.layout))).toEqual([["h", "i"]]);
  });

  it("a group dissolved for lack of shown leaves stays saved while its leaf is open and ungrouped", () => {
    const g2 = split("g2", "a", "h");
    expect(renderedGroups([g2], held)).toEqual([]);
    expect(groupsToPersist([], [tab("a")], [g2], o)).toEqual([g2]);
    expect(groupsToPersist([], [], [g2], o)).toEqual([]); // the user closed a
  });

  it("nothing without the lock; the shown groups when nothing is held back", () => {
    expect(groupsToPersist([three], [], [three], { heldBack: held, writesLayout: false })).toBeNull();
    const shown = [split("g", "a", "b")];
    expect(groupsToPersist(shown, [], [], { heldBack: [], writesLayout: true })).toBe(shown);
  });
});

describe("migrationNotice", () => {
  it("none, one, several", () => {
    expect(migrationNotice(0)).toBeNull();
    expect(migrationNotice(1)).toBe("1 restored tab couldn't move to xshell's local terminal service. It'll run the old way for now. xshell retries next start.");
    expect(migrationNotice(3)).toBe("3 restored tabs couldn't move to xshell's local terminal service. They'll run the old way for now. xshell retries next start.");
  });
});
