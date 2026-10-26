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
  it("describes a build difference, not assumed age", () => {
    expect(render()).toContain("different build");
    expect(render()).toContain("Use this Desktop’s build");
  });
  it("shows backend preparation, waiting and failure rather than treating an invocation as completion", () => {
    expect(render({ phase: "preparing" })).toContain("Preparing this Desktop’s build");
    expect(render({ phase: "upgrading" })).toContain("Waiting for its Daemon to reconnect");
    expect(render({ lastError: "stage failed", errorHint: "update-failed" })).toContain("stage failed");
    expect(render({ status: "offline", phase: null, lastError: "deadline reached", errorHint: "update-failed" })).toContain("deadline reached");
  });
  it("has no operative update for an old Windows owner, even during reconnect", () => {
    for (const kind of ["build-differs", "reconnecting"] as const) {
      const html = render({ status: kind, updateUnavailable: "old owner" });
      expect(html).toContain("first update and reopen xshell");
      expect(html).not.toContain("<button");
    }
    expect(render({ status: "connected", updateAction: null })).toBe("");
  });
  it("lets exact backend success override a lost invocation acknowledgement", async () => {
    vi.mocked(invoke).mockRejectedValueOnce({ message: "acknowledgement lost" });
    hostUpdates.ask(host, status);
    await hostUpdates.confirm();
    expect(render()).toContain("acknowledgement lost");
    const build = { commit: "85ee920a1b2c", dirty: null, time: 7 };
    expect(render({ status: "connected", updateAction: null, daemonBuild: build, desktopBuild: build, lastError: null, errorHint: null, phase: null })).toBe("");
  });

  for (const initiallyMounted of [false, true]) {
    it(`remembers registry success across offline and late rejection (${initiallyMounted ? "action unmounts after success" : "no mounted action"})`, async () => {
      let reject!: (reason: Error) => void;
      vi.mocked(invoke).mockImplementation(command => command === "host_upgrade"
        ? new Promise<never>((_, fail) => { reject = fail; })
        : Promise.resolve([]));
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
