import { describe, it, expect, vi } from "vitest";
import { activeUpdateHost, HostUpdates, updateAction } from "./hostUpdate";
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
  for (const routeUpdateUnavailable of ["relay-only", "identity-bound-alias"] as const) {
    expect(updateAction({ ...eligible, routeUpdateUnavailable })).toBeNull();
    for (const updateUnavailable of ["desktop-owned", "windows-owner"] as const) {
      expect(updateAction({ ...eligible, routeUpdateUnavailable, updateUnavailable })).toBeNull();
    }
  }
  expect(updateAction(eligible)).toBe("replace");
});
