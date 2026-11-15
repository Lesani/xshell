import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, it, expect, vi } from "vitest";
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
import { invoke } from "@tauri-apps/api/core";
import { registry } from "../hosts/registry";
import { hostUpdates } from "../hosts/hostUpdate";
import { HostUpdateAction } from "./HostUpdateAction";
import type { HostStatus } from "../hosts/types";
const host = { id: "h_aaaaaaaa", name: "Windows fixture", sshTarget: "test" };
const status: HostStatus = { host: host.id, status: "build-differs", phase: null, lastError: null, errorHint: null, daemonVersion: "1.5.0", desktopVersion: "1.5.0", protocol: 1, os: "windows", arch: "x86_64", incompatibleReason: null, nextRetryAt: null, sinceMs: 1, updateAction: "replace" };
const render = (patch: Partial<HostStatus> = {}) => renderToStaticMarkup(createElement(HostUpdateAction, { host, status: { ...status, ...patch }, description: true }));
describe("shared Host update action", () => {
  it("describes a build difference without build hashes, not assumed age", () => {
    expect(render()).toContain("Windows fixture: Update available");
    expect(render({ daemonBuild: { commit: "42c9be9636a9", dirty: null, time: 1 }, desktopBuild: { commit: "81badb2ccf31", dirty: null, time: 2 } })).not.toMatch(/42c9be|81badb/);
    expect(render()).toContain("Use this Desktop’s build");
  });
  it("shows backend preparation, waiting and failure rather than treating an invocation as completion", () => {
    expect(render({ phase: "preparing" })).toContain("Sending update…");
    expect(render({ phase: "installing" })).toContain("Sending update…");
    expect(render({ phase: "upgrading" })).toContain("Restarting…");
    // A failure in approved words only, never the backend's text.
    expect(render({ lastError: "stage failed", errorHint: "update-failed" })).toContain("reconnected after the update.");
    expect(render({ lastError: "stage failed", errorHint: "update-failed" })).not.toContain("stage failed");
    expect(render({ status: "offline", phase: null, lastError: "offline: deadline reached", errorHint: "update-failed" })).toContain("Windows fixture is offline.");
    expect(render({ status: "offline", phase: null, lastError: "offline: deadline reached", errorHint: "update-failed" })).not.toContain("deadline reached");
  });
  it("has no operative update for an old Windows owner, even during reconnect", () => {
    for (const kind of ["build-differs", "reconnecting"] as const) {
      const html = render({ status: kind, updateUnavailable: "windows-owner" });
      expect(html).toContain("first update and reopen xshell");
      expect(html).not.toContain("<button");
    }
    expect(render({ status: "connected", updateAction: null })).toBe("");
  });
  it("lets exact backend success override a lost invocation acknowledgement", async () => {
    vi.mocked(invoke).mockRejectedValueOnce({ message: "acknowledgement lost" });
    hostUpdates.ask(host, status);
    await hostUpdates.confirm();
    expect(render()).toContain("reconnected after the update");
    expect(render()).not.toContain("acknowledgement lost");
    const build = { commit: "85ee920a1b2c", dirty: null, time: 7 };
    expect(render({ status: "connected", updateAction: null, daemonBuild: build, desktopBuild: build, lastError: null, errorHint: null, phase: null })).toBe("");
  });

  for (const initiallyMounted of [false, true]) {
    it(`remembers registry success across offline and late rejection (${initiallyMounted ? "action unmounts after success" : "no mounted action"})`, async () => {
      let reject!: (reason: Error) => void;
      vi.mocked(invoke).mockImplementation(command => command === "host_upgrade"
        ? new Promise<never>((_, fail) => { reject = fail; })
        : Promise.resolve(command === "hosts_available" ? { sequence: 0, hosts: [] } : []));
      await registry.init([host]);
      const build = { commit: "85ee920a1b2c", dirty: null, time: 7 };
      const initial = { ...status, desktopBuild: build, configGeneration: 7, linkGeneration: 12 };
      registry.applyStatus(initial);
      // useSyncExternalStore's UI subscription may be absent or removed after success.
      const unmount = initiallyMounted ? hostUpdates.subscribe(() => {}) : () => {};
      hostUpdates.ask(host, initial);
      const pending = hostUpdates.confirm();
      const healthy: HostStatus = { ...initial, status: "connected", phase: null, updateAction: null, daemonBuild: build, linkGeneration: 13 };
      registry.applyStatus(healthy);
      if (initiallyMounted) {
        expect(renderToStaticMarkup(createElement(HostUpdateAction, { host, status: healthy, description: true }))).toBe("");
      }
      unmount();
      const offline: HostStatus = { ...healthy, status: "offline" };
      registry.applyStatus(offline);
      reject(new Error("late acknowledgement lost"));
      await pending;
      expect(hostUpdates.getSnapshot().errors[host.id]).toBe("");
      expect(hostUpdates.getSnapshot().invoking[host.id]).toBe(false);
      // A newly rendered action sees only the current offline status and retained store.
      expect(renderToStaticMarkup(createElement(HostUpdateAction, { host, status: offline, description: true }))).toBe("");
    });
  }

});

it("keeps the Ring limitation and Desktop-owner diagnosis independently visible with no build action", () => {
  const html = render({ routeUpdateUnavailable: "ring-update-unsupported", updateUnavailable: "desktop-owned", updateAction: null });
  expect(html).toContain("Desktop there manages this Daemon");
  expect(html).not.toContain("<button");
});

it("shows how to update a Ring machine whose xshell cannot take an update over the Ring", async () => {
  vi.mocked(invoke).mockImplementation(async command => command === "host_install_command" ? "curl -fsSL https://github.com/Lesani/xshell/releases/download/v1.5.0/install.sh | sh" : null);
  const { fetchInstallCommand } = await import("./HostUpdateAction");
  expect(await fetchInstallCommand()).toContain("/install.sh | sh");
  const html = render({ os: null, routeUpdateUnavailable: "ring-update-unsupported", updateUnavailable: null, updateAction: null, status: "upgrade-pending" });
  expect(html).toContain("Windows fixture is too old to update through your Ring. Update xshell directly on that machine.");
  expect(html).not.toContain("Use this Desktop");
});

it("offers Retry after a failed update and shows the backend's rollback text", () => {
  const html = render({ status: "upgrade-pending", updateAction: "update", daemonVersion: "1.4.0", lastError: "The update failed. Windows fixture is back on version 1.4.0. Try again.", errorHint: "update-failed" });
  expect(html).toContain("Retry");
  expect(html).toContain("back on version 1.4.0");
});
