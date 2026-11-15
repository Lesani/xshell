import { describe, it, expect, vi } from "vitest";
import { activeUpdateHost, HostUpdates, skipReason, updateAction } from "./hostUpdate";
import type { HostStatus, HostConfig } from "./types";
import type { LayoutNode } from "../types";

const host: HostConfig = { id: "h_aaaaaaaa", name: "Linux", sshTarget: "fixture" };
const status = (patch: Partial<HostStatus> = {}): HostStatus => ({ host: host.id, status: "upgrade-pending", phase: null, lastError: null, errorHint: null, daemonVersion: "1.4.0", desktopVersion: "1.5.0", protocol: 1, os: "linux", arch: "x86_64", incompatibleReason: null, nextRetryAt: null, sinceMs: 1, configGeneration: 2, linkGeneration: 3, updateAction: "update", ...patch });

describe("Host update targeting and eligibility", () => {
  it("uses backend semver eligibility, not incompatible protocol direction", () => {
    expect(updateAction(status())).toBe("update");
    expect(updateAction(status({ updateAction: "replace" }))).toBe("replace");
    expect(updateAction(status({ updateAction: null, status: "incompatible", incompatibleReason: "daemon-older" }))).toBeNull();
    expect(updateAction(status({ os: "windows", updateUnavailable: "windows-owner" }))).toBeNull();
    expect(updateAction(status({ status: "offline" }))).toBeNull();
  });
  it("targets the selected Project's Host on Home, never global Home or Settings", () => {
    const projectA = { host: "h_a" };
    const projectB = { host: "h_b" };
    expect(activeUpdateHost("home", [], [], {}, projectA)).toBe("h_a");
    expect(activeUpdateHost("home", [], [], {}, projectB)).toBe("h_b");
    expect(activeUpdateHost("home", [], [], {}, null)).toBeUndefined();
    expect(activeUpdateHost("home", [], [], {}, {})).toBe("local");
    expect(activeUpdateHost("settings", [], [], {}, projectA)).toBeUndefined();
    expect(activeUpdateHost("settings", [], [], {}, {})).toBeUndefined();
  });
  it("keeps Terminal precedence over a stale Project, including Local and unresolved Terminals", () => {
    const tabs = [{ id: "a", host: "h_a" }, { id: "b", host: "h_b" }, { id: "local" }];
    const projectA = { host: "h_a" };
    expect(activeUpdateHost("b", tabs, [], {}, projectA)).toBe("h_b");
    expect(activeUpdateHost("local", tabs, [], {}, projectA)).toBeUndefined();
    expect(activeUpdateHost("missing", tabs, [], {}, projectA)).toBeUndefined();
  });
  it("targets a resolved Local Daemon Tab but never an in-process Tab or unresolved leaf", () => {
    const tabs = [{ id: "daemon", terminal: "local-terminal" }, { id: "in-process" }, { id: "remote", host: "h_a", terminal: "remote-terminal" }];
    const projectB = { host: "h_b" };
    expect(activeUpdateHost("daemon", tabs, [], {}, projectB)).toBe("local");
    expect(activeUpdateHost("in-process", tabs, [], {}, projectB)).toBeUndefined();
    expect(activeUpdateHost("missing", tabs, [], {}, projectB)).toBeUndefined();
    expect(activeUpdateHost("remote", tabs, [], {}, {})).toBe("h_a");
  });
  it("targets a mixed split's focused Host with the existing first-leaf fallback", () => {
    const tabs = [{ id: "daemon", terminal: "local-terminal" }, { id: "remote", host: "h_a", terminal: "remote-terminal" }];
    const groups: { id: string; layout: LayoutNode }[] = [{ id: "g", layout: { kind: "split", direction: "col", ratio: 0.5, children: [{ kind: "leaf", tabId: "daemon" }, { kind: "leaf", tabId: "remote" }] } }];
    const projectB = { host: "h_b" };
    expect(activeUpdateHost("g", tabs, groups, { g: "daemon" }, projectB)).toBe("local");
    expect(activeUpdateHost("g", tabs, groups, { g: "remote" }, {})).toBe("h_a");
    expect(activeUpdateHost("g", tabs, groups, {}, projectB)).toBe("local");
    expect(activeUpdateHost("g", tabs, groups, { g: "missing" }, projectB)).toBe("local");
    expect(activeUpdateHost("g", tabs.filter(t => t.id !== "daemon"), groups, { g: "daemon" }, projectB)).toBeUndefined();
    expect(activeUpdateHost("g", tabs.filter(t => t.id !== "daemon"), groups, {}, projectB)).toBeUndefined();
  });
  it("targets the focused split leaf, with first-leaf fallback, instead of a stale Project", () => {
    const tabs = [{ id: "a", host: "h_a" }, { id: "b", host: "h_b" }];
    const groups: { id: string; layout: LayoutNode }[] = [{ id: "g", layout: { kind: "split", direction: "col", ratio: 0.5, children: [{ kind: "leaf", tabId: "a" }, { kind: "leaf", tabId: "b" }] } }];
    expect(activeUpdateHost("g", tabs, groups, { g: "b" }, { host: "h_a" })).toBe("h_b");
    expect(activeUpdateHost("g", tabs, groups, { g: "missing" }, { host: "h_b" })).toBe("h_a");
    expect(activeUpdateHost("g", tabs, groups, {}, { host: "h_b" })).toBe("h_a");
  });
  it("never falls back to a stale Project when a split leaf is Local or unresolved", () => {
    const tabs = [{ id: "a", host: "h_a" }, { id: "local" }];
    const groups: { id: string; layout: LayoutNode }[] = [{ id: "g", layout: { kind: "split", direction: "col", ratio: 0.5, children: [{ kind: "leaf", tabId: "a" }, { kind: "leaf", tabId: "local" }] } }];
    const projectB = { host: "h_b" };
    expect(activeUpdateHost("g", tabs, groups, { g: "local" }, projectB)).toBeUndefined();
    expect(activeUpdateHost("g", tabs.filter(t => t.id !== "a"), groups, { g: "a" }, projectB)).toBeUndefined();
    expect(activeUpdateHost("g", tabs.filter(t => t.id !== "a"), groups, { g: "missing" }, projectB)).toBeUndefined();
  });
  it("binds one confirmation across surfaces, cancel has no effect, and keeps invocation errors", async () => {
    const invoke = vi.fn().mockRejectedValue({ message: "stage failed" });
    const updates = new HostUpdates(invoke);
    updates.ask(host, status());
    updates.ask({ ...host, id: "h_b", name: "Windows" }, status());
    expect(updates.getSnapshot().confirmation?.host.name).toBe("Linux");
    updates.cancel();
    expect(invoke).not.toHaveBeenCalled();
    updates.ask(host, status());
    const current = updates.getSnapshot().confirmation;
    await Promise.all([updates.confirm(), updates.confirm()]);
    expect(invoke).toHaveBeenCalledTimes(1);
    expect(invoke).toHaveBeenCalledWith("host_upgrade", { host: host.id, target: current?.target });
    expect(updates.getSnapshot().errors[host.id]).toBe("stage failed");
    expect(updates.getSnapshot().confirmation).toBeNull();
  });
  it("remembers exact success per invocation, not per Host forever", async () => {
    let reject!: (error: Error) => void;
    const updates = new HostUpdates(vi.fn(() => new Promise<never>((_, fail) => { reject = fail; })));
    const build = { commit: "85ee920a1b2c", dirty: null, time: 7 };
    const healthy = status({ status: "connected", updateAction: null, daemonVersion: "1.5.0", daemonBuild: build, desktopBuild: build, linkGeneration: 4 });
    updates.ask(host, status({ desktopBuild: build }));
    const pending = updates.confirm();
    updates.reconcile(healthy);
    updates.reconcile({ ...healthy, status: "offline" });
    reject(new Error("acknowledgement lost"));
    await pending;
    expect(updates.getSnapshot().errors[host.id]).toBe("");
    // A later invocation on the same Host still reports its own genuine failure.
    updates.ask(host, status({ desktopBuild: build }));
    const retry = updates.confirm();
    reject(new Error("new stage failure"));
    await retry;
    expect(updates.getSnapshot().errors[host.id]).toBe("new stage failure");
  });

});

it("route limitation blocks updates independently of Desktop/Windows ownership, leaving a real SSH control eligible", () => {
  const eligible = { status: "build-differs", updateAction: "replace", phase: null } as HostStatus;
  for (const routeUpdateUnavailable of ["ring-update-unsupported", "identity-bound-alias"] as const) {
    expect(updateAction({ ...eligible, routeUpdateUnavailable })).toBeNull();
    for (const updateUnavailable of ["desktop-owned", "windows-owner"] as const) {
      expect(updateAction({ ...eligible, routeUpdateUnavailable, updateUnavailable })).toBeNull();
    }
  }
  expect(updateAction(eligible)).toBe("replace");
});

describe("Update all", () => {
  const build = { commit: "85ee920a1b2c", dirty: null, time: 7 };
  const a = { id: "r_a", name: "box-a" };
  const b = { id: "r_b", name: "box-b" };
  const c = { id: "r_c", name: "box-c" };
  const older = (id: string, gen = 3): HostStatus => status({ host: id, desktopBuild: build, linkGeneration: gen });
  const healthy = (id: string, gen: number): HostStatus => status({ host: id, status: "connected", updateAction: null, daemonVersion: "1.5.0", daemonBuild: build, desktopBuild: build, linkGeneration: gen });

  function fixture(initial: Record<string, HostStatus>, settleMs = 1000) {
    const statuses: Record<string, HostStatus> = { ...initial };
    const listeners = new Set<() => void>();
    const push = (s: HostStatus) => { statuses[s.host] = s; for (const l of [...listeners]) l(); };
    const calls: string[] = [];
    const replies = new Map<string, () => Promise<unknown>>();
    const invoke = vi.fn(async (_cmd: string, args?: { host: string }) => { calls.push(args!.host); return (replies.get(args!.host) ?? (async () => null))(); });
    const updates = new HostUpdates(invoke as never, id => statuses[id], l => { listeners.add(l); return () => { listeners.delete(l); }; }, settleMs);
    return { updates, push, calls, replies, statuses };
  }

  it("lists what it leaves out and why", () => {
    expect(skipReason(older("x"))).toBeNull();
    expect(skipReason(healthy("x", 4))).toBe("current");
    expect(skipReason(status({ status: "offline" }))).toBe("offline");
    expect(skipReason(undefined)).toBe("offline");
    expect(skipReason(status({ routeUpdateUnavailable: "ring-update-unsupported", updateAction: null }))).toBe("unsupported");
    expect(skipReason(status({ phase: "upgrading" }))).toBe("busy");
  });

  it("updates one machine after another and reports each", async () => {
    const f = fixture({ r_a: older("r_a"), r_b: older("r_b"), r_c: healthy("r_c", 9) });
    // a comes back with this Desktop's build; b's update is rolled back.
    f.replies.set("r_a", async () => { setTimeout(() => f.push(healthy("r_a", 4)), 10); return null; });
    f.replies.set("r_b", async () => {
      setTimeout(() => {
        f.push({ ...older("r_b"), phase: "upgrading" });
        f.push({ ...older("r_b", 4), phase: null, lastError: "The update failed. box-b is back on version 1.4.0. Try again.", errorHint: "update-failed" });
      }, 10);
      return null;
    });
    f.updates.askAll([a, b, c].map(h => ({ host: h, status: f.statuses[h.id] })));
    const run = f.updates.getSnapshot().all!;
    expect(run.queue.map(q => q.host.id)).toEqual(["r_a", "r_b"]);
    expect(run.skipped).toEqual([{ host: c, outcome: "skipped", reason: "current" }]);
    expect(f.calls).toEqual([]);
    const order: string[] = [];
    f.updates.subscribe(() => { const cur = f.updates.getSnapshot().all?.current?.id; if (cur && order[order.length - 1] !== cur) order.push(cur); });
    await f.updates.confirmAll();
    expect(order).toEqual(["r_a", "r_b"]);
    expect(f.calls).toEqual(["r_a", "r_b"]);
    const done = f.updates.getSnapshot().all!;
    expect(done.done).toBe(true);
    expect(done.results).toEqual([
      { host: a, outcome: "ok", version: "1.5.0" },
      { host: b, outcome: "failed", error: "The update failed. box-b is back on version 1.4.0. Try again." },
    ]);
  });

  it("stops waiting for a machine at the time limit and goes on with the next", async () => {
    const f = fixture({ r_a: older("r_a"), r_b: older("r_b") }, 50);
    f.replies.set("r_b", async () => { setTimeout(() => f.push(healthy("r_b", 4)), 5); return null; });
    f.updates.askAll([a, b].map(h => ({ host: h, status: f.statuses[h.id] })));
    await f.updates.confirmAll();
    const r = f.updates.getSnapshot().all!.results;
    expect(r[0]).toEqual({ host: a, outcome: "failed", error: "box-a hasn't reconnected after the update. Check that machine, then try connecting again." });
    expect(r[1]).toEqual({ host: b, outcome: "ok", version: "1.5.0" });
  });

  it("a refused update is that machine's failure; an older update error does not count", async () => {
    const f = fixture({ r_a: { ...older("r_a"), lastError: "old failure", errorHint: "update-failed" }, r_b: older("r_b") });
    f.replies.set("r_a", async () => { throw { message: "not online" }; });
    f.replies.set("r_b", async () => { setTimeout(() => f.push(healthy("r_b", 4)), 5); return null; });
    f.updates.askAll([a, b].map(h => ({ host: h, status: f.statuses[h.id] })));
    await f.updates.confirmAll();
    expect(f.updates.getSnapshot().all!.results).toEqual([
      { host: a, outcome: "failed", error: "not online" },
      { host: b, outcome: "ok", version: "1.5.0" },
    ]);
  });

  it("does nothing without an eligible machine, and cancel starts nothing", async () => {
    const f = fixture({ r_c: healthy("r_c", 2) });
    f.updates.askAll([{ host: c, status: f.statuses.r_c }]);
    expect(f.updates.getSnapshot().all).toBeNull();
    const g = fixture({ r_a: older("r_a") });
    g.updates.askAll([{ host: a, status: g.statuses.r_a }]);
    g.updates.cancelAll();
    expect(g.updates.getSnapshot().all).toBeNull();
    expect(g.calls).toEqual([]);
  });
});
