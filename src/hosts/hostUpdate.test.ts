import { describe, it, expect, vi, afterEach } from "vitest";
import { activeUpdateHost, exactUpdateConnected, HostUpdates, SETTLE_MS, skipReason, updateAction, updateSource } from "./hostUpdate";
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
    // A machine with the xshell app (Windows included) is not "too old": it is updated there.
    expect(skipReason(status({ routeUpdateUnavailable: "ring-update-unsupported", updateUnavailable: "desktop-owned", updateAction: null }))).toBe("desktop");
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

describe("Updates from the latest release", () => {
  const build = { commit: "85ee920a1b2c", dirty: null, time: 7 };
  const other = { commit: "3b9c9a89c10e", dirty: null, time: 9 };
  const a = { id: "r_a", name: "box-a" };
  // A Host that downloads the latest release (1.6.0) itself; this Desktop runs 1.5.0.
  const release = (id: string, gen = 3): HostStatus => status({ host: id, daemonVersion: "1.5.0", desktopVersion: "1.5.0", desktopBuild: build, linkGeneration: gen, updateSource: "release", updateVersion: "1.6.0" });
  const healthy = (id: string, gen: number, patch: Partial<HostStatus> = {}): HostStatus => status({ host: id, status: "connected", updateAction: null, updateSource: null, updateVersion: null, daemonVersion: "1.6.0", daemonBuild: other, desktopBuild: build, linkGeneration: gen, ...patch });
  function fixture(initial: Record<string, HostStatus>, settleMs = 1000) {
    const statuses: Record<string, HostStatus> = { ...initial };
    const listeners = new Set<() => void>();
    const push = (s: HostStatus) => { statuses[s.host] = s; for (const l of [...listeners]) l(); };
    const replies = new Map<string, () => Promise<unknown>>();
    const invoke = vi.fn(async (_cmd: string, args?: { host: string }) => (replies.get(args!.host) ?? (async () => null))());
    const updates = new HostUpdates(invoke as never, id => statuses[id], l => { listeners.add(l); return () => { listeners.delete(l); }; }, settleMs);
    return { updates, push, replies, statuses, invoke };
  }
  const run = async (f: ReturnType<typeof fixture>) => {
    f.updates.askAll([{ host: a, status: f.statuses.r_a }]);
    await f.updates.confirmAll();
    return f.updates.getSnapshot().all!.results;
  };

  it("healthy means connected with nothing offered, whatever version this Desktop runs", () => {
    const newer = status({ status: "connected", updateAction: null, daemonVersion: "1.7.0", desktopVersion: "1.5.0", daemonBuild: other, desktopBuild: build });
    expect(exactUpdateConnected(newer)).toBe(true);
    expect(skipReason(newer)).toBe("current");
    expect(exactUpdateConnected({ ...newer, updateAction: "update" })).toBe(false);
    expect(exactUpdateConnected({ ...newer, phase: "upgrading" })).toBe(false);
    expect(exactUpdateConnected({ ...newer, lastError: "x", errorHint: "update-failed" })).toBe(false);
  });

  it("a confirmation expects the offered version and no build for a release", () => {
    const updates = new HostUpdates(vi.fn());
    updates.ask(host, release(host.id));
    expect(updates.getSnapshot().confirmation).toMatchObject({ source: "release", expectedVersion: "1.6.0", expectedBuild: null });
  });

  it("an older backend without a source is read as this Desktop's build", () => {
    const old = status({ desktopBuild: build });
    expect(old.updateSource).toBeUndefined();
    expect(updateSource(old)).toBe("desktop");
    expect(updateSource(status({ updateAction: null }))).toBeNull();
    const updates = new HostUpdates(vi.fn());
    updates.ask(host, old);
    expect(updates.getSnapshot().confirmation).toMatchObject({ source: "desktop", expectedVersion: "1.5.0", expectedBuild: build });
  });

  it("a release succeeds on the offered version without comparing builds", async () => {
    const f = fixture({ r_a: release("r_a") });
    f.replies.set("r_a", async () => { setTimeout(() => f.push(healthy("r_a", 4)), 5); return null; });
    expect(await run(f)).toEqual([{ host: a, outcome: "ok", version: "1.6.0" }]);
  });

  it("a release that comes back on another version is not a success", async () => {
    const f = fixture({ r_a: release("r_a") }, 50);
    f.replies.set("r_a", async () => { setTimeout(() => f.push(healthy("r_a", 4, { daemonVersion: "1.5.5" })), 5); return null; });
    expect((await run(f))[0]).toMatchObject({ host: a, outcome: "failed" });
  });

  it("a Host that was sent this Desktop's build instead of the release succeeds on this Desktop's version", async () => {
    // The Host could not get 1.6.0 itself; the Desktop (1.5.0) sent its own build. 1.6.0 stays offered.
    const sent = status({ host: "r_a", status: "upgrade-pending", updateAction: "update", updateSource: "release", updateVersion: "1.6.0", daemonVersion: "1.5.0", daemonBuild: build, desktopBuild: build, linkGeneration: 4 });
    expect(sent.desktopVersion).toBe("1.5.0");
    const f = fixture({ r_a: { ...release("r_a"), daemonVersion: "1.4.0" } }, 60_000);
    f.replies.set("r_a", async () => { setTimeout(() => f.push(sent), 5); return null; });
    expect(await run(f)).toEqual([{ host: a, outcome: "ok", version: "1.5.0" }]);
    // Sol: the result names the version it arrived on, whatever the status says later.
    const g = fixture({ r_a: { ...release("r_a"), daemonVersion: "1.4.0" } }, 60_000);
    g.replies.set("r_a", async () => { g.push(sent); g.push({ ...sent, daemonVersion: "1.7.0", linkGeneration: 5 }); return null; });
    expect(await run(g)).toEqual([{ host: a, outcome: "ok", version: "1.5.0" }]);
  });

  it("a Host already on this Desktop's version that only reconnects has not been updated", async () => {
    // Sol: Host and Desktop both run 1.5.0, release 1.6.0 offered; the Host reconnects and
    // the backend refuses the obsolete target. No build was sent: not a success.
    const back = status({ host: "r_a", status: "upgrade-pending", updateAction: "update", updateSource: "release", updateVersion: "1.6.0", daemonVersion: "1.5.0", daemonBuild: build, desktopBuild: build, linkGeneration: 4 });
    const f = fixture({ r_a: { ...release("r_a"), daemonVersion: "1.5.0" } }, 50);
    f.replies.set("r_a", async () => { f.push(back); throw new Error("the Host's connection changed"); });
    expect((await run(f))[0]).toMatchObject({ host: a, outcome: "failed", error: "the Host's connection changed" });
    // Another build of this Desktop's version is not the build it sent either.
    const h = fixture({ r_a: { ...release("r_a"), daemonVersion: "1.4.0" } }, 50);
    h.replies.set("r_a", async () => { setTimeout(() => h.push({ ...back, daemonBuild: other }), 5); return null; });
    expect((await run(h))[0]).toMatchObject({ host: a, outcome: "failed" });
  });

  it("this Desktop's build still has to come back with the same build", async () => {
    const f = fixture({ r_a: status({ host: "r_a", desktopBuild: build }) }, 50);
    f.replies.set("r_a", async () => { setTimeout(() => f.push(healthy("r_a", 4, { daemonVersion: "1.5.0", daemonBuild: other })), 5); return null; });
    expect((await run(f))[0]).toMatchObject({ host: a, outcome: "failed" });
    const g = fixture({ r_a: status({ host: "r_a", desktopBuild: build }) }, 50);
    g.replies.set("r_a", async () => { setTimeout(() => g.push(healthy("r_a", 4, { daemonVersion: "1.5.0", daemonBuild: build })), 5); return null; });
    expect(await run(g)).toEqual([{ host: a, outcome: "ok", version: "1.5.0" }]);
  });

  it("settles as already current when the Host answers it is current, without waiting for the time limit", async () => {
    // What the backend publishes after the answer "already current": connected, nothing
    // offered, the same version and link generation as before.
    const f = fixture({ r_a: release("r_a") }, 60_000);
    f.replies.set("r_a", async () => {
      setTimeout(() => f.push(status({ host: "r_a", status: "connected", updateAction: null, updateSource: null, updateVersion: null, daemonVersion: "1.5.0", desktopBuild: build, linkGeneration: 3 })), 5);
      return null;
    });
    expect(await run(f)).toEqual([{ host: a, outcome: "skipped", reason: "current" }]);
  });

  it("an update that worked is a success even when a newer release is offered right after", async () => {
    // Brought to 1.6.0; meanwhile the Desktop learned 1.7.0, so the Host is back offering it.
    const further = (gen: number) => status({ host: "r_a", status: "upgrade-pending", updateAction: "update", updateSource: "release", updateVersion: "1.7.0", daemonVersion: "1.6.0", daemonBuild: other, desktopBuild: build, linkGeneration: gen });
    const f = fixture({ r_a: release("r_a") }, 60_000);
    f.replies.set("r_a", async () => { setTimeout(() => f.push(further(4)), 5); return null; });
    expect(await run(f)).toEqual([{ host: a, outcome: "ok", version: "1.6.0" }]);
    // A single Update keeps that evidence over a lost acknowledgement.
    let reject!: (e: Error) => void;
    const updates = new HostUpdates(vi.fn(() => new Promise<never>((_, fail) => { reject = fail; })) as never);
    updates.ask(host, release(host.id));
    const pending = updates.confirm();
    updates.reconcile({ ...further(4), host: host.id });
    reject(new Error("acknowledgement lost"));
    await pending;
    expect(updates.getSnapshot().errors[host.id]).toBe("");
  });

  describe("the time limit", () => {
    afterEach(() => { vi.useRealTimers(); });
    it("starts when the request answers: a long download is not a failure", async () => {
      vi.useFakeTimers();
      const f = fixture({ r_a: release("r_a") }, SETTLE_MS);
      f.replies.set("r_a", () => new Promise(done => setTimeout(() => done(null), 140_000)));
      const results = run(f);
      await vi.advanceTimersByTimeAsync(140_000);
      await vi.advanceTimersByTimeAsync(60_000);
      f.push(healthy("r_a", 4));
      expect(await results).toEqual([{ host: a, outcome: "ok", version: "1.6.0" }]);
    });
    it("fails with the approved sentence when nothing comes back within it after the answer", async () => {
      vi.useFakeTimers();
      const f = fixture({ r_a: release("r_a") }, SETTLE_MS);
      let settled = false;
      const results = run(f).then(r => { settled = true; return r; });
      await vi.advanceTimersByTimeAsync(SETTLE_MS - 1);
      expect(settled).toBe(false);
      await vi.advanceTimersByTimeAsync(1);
      expect(await results).toEqual([{ host: a, outcome: "failed", error: "box-a hasn't reconnected after the update. Check that machine, then try connecting again." }]);
    });
  });

  it("remembers the confirmed source while the update runs, and forgets it once the Host is healthy", async () => {
    let resolve!: () => void;
    const updates = new HostUpdates(vi.fn(() => new Promise<void>(done => { resolve = done; })) as never);
    updates.ask(host, release(host.id));
    const pending = updates.confirm();
    expect(updates.getSnapshot().sources[host.id]).toBe("release");
    // Running: the status offers nothing (and so names no source).
    updates.reconcile(healthy(host.id, 4));
    expect(updates.getSnapshot().sources[host.id]).toBe("release");
    resolve();
    await pending;
    updates.reconcile(healthy(host.id, 4));
    expect(updates.getSnapshot().sources[host.id]).toBeUndefined();
  });
});
