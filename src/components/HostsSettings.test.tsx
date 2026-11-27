// @vitest-environment happy-dom
import { act } from "react";
import { createRoot } from "react-dom/client";
import { expect, it, vi } from "vitest";
import { HostsSettings } from "./HostsSettings";
import { registry } from "../hosts/registry";
import type { AvailableSnapshot, HostStatus } from "../hosts/types";

const native = vi.hoisted(() => ({ invoke: vi.fn(), handlers: new Map<string, (event: { payload: unknown }) => void>() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: native.invoke }));
vi.mock("@tauri-apps/api/event", () => ({ listen: async (name: string, handler: (event: { payload: unknown }) => void) => { native.handlers.set(name, handler); return () => native.handlers.delete(name); } }));
vi.mock("@tauri-apps/plugin-store", () => ({ load: async () => ({ get: async () => null, set: async () => {}, save: async () => {} }) }));

// The Ring's machines moved to Settings → Ring: Settings → Hosts shows this computer and SSH
// Hosts only, even while a Ring-only Host is available over the Relay.
it("shows no Ring rows or Ring section for a Ring-only Host, and asks nothing of the Ring", async () => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  const id = `r_${"a".repeat(64)}`;
  const snapshot: AvailableSnapshot = { sequence: 1, hosts: [{ id, aliases: [id], name: "Ring-only Daemon", color: null, daemonKey: "key", routes: ["relay"], activeRoute: "relay", reachability: { kind: "online" } }] };
  const status = { host: id, status: "build-differs", phase: null, updateAction: null, routeUpdateUnavailable: "ring-update-unsupported", updateUnavailable: "desktop-owned", lastError: null } as HostStatus;
  native.invoke.mockImplementation(async command => {
    if (command === "hosts_available") return snapshot;
    if (command === "hosts_status") return [{ status, terminals: null }];
    if (command === "local_host_info") return { mode: "in-process", reason: null };
    if (command === "host_call") return { installed: false };
  });
  await registry.init([]);
  const container = document.createElement("div"); document.body.append(container);
  const root = createRoot(container), save = vi.fn(), remove = vi.fn();
  try {
    await act(async () => { root.render(<HostsSettings onSave={save} onRemove={remove} />); });
    expect(registry.getSnapshot().available.map(h => h.id)).toEqual([id]);
    expect(container.querySelector(".host-empty")).not.toBeNull();
    expect(container.querySelector("[data-ring-host]")).toBeNull();
    expect(container.querySelector("[data-ring-section]")).toBeNull();
    expect(container.textContent).not.toContain("Ring-only Daemon");
    expect(native.invoke.mock.calls.some(([command]) => String(command).startsWith("ring_"))).toBe(false);
  } finally {
    await act(async () => { root.unmount(); }); container.remove(); registry._dispose(); vi.unstubAllGlobals();
  }
});
