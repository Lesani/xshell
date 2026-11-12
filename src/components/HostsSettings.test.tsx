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

it("renders a real hosts_available Ring row and updates it through hosts:available with no SSH editing authority", async () => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  const id = `r_${"a".repeat(64)}`;
  const snapshot: AvailableSnapshot = { sequence: 1, hosts: [{ id, aliases: [id], name: "Ring-only Daemon", color: null, daemonKey: "key", routes: ["relay"], activeRoute: "relay", reachability: { kind: "online" } }] };
  const status = { host: id, status: "build-differs", phase: null, updateAction: null, routeUpdateUnavailable: "relay-only", updateUnavailable: "desktop-owned", lastError: null } as HostStatus;
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
    const row = container.querySelector<HTMLElement>("[data-ring-host]")!;
    expect(row.textContent).toContain("Ring-only Daemon");
    expect(row.textContent).toContain("Ring · Relay");
    expect(row.textContent).toContain("Desktop there manages this Daemon");
    expect(Array.from(row.querySelectorAll("button")).map(b => b.textContent)).toEqual(["Reconnect"]);
    await act(async () => { row.querySelector("button")!.click(); });
    expect(native.invoke).toHaveBeenCalledWith("hosts_kick", { host: id });
    expect(save).not.toHaveBeenCalled(); expect(remove).not.toHaveBeenCalled();
    expect(registry.getSnapshot().configs).toEqual([]);
    expect(native.invoke.mock.calls.some(([command]) => ["host_test", "host_upgrade"].includes(command))).toBe(false);
    await act(async () => { native.handlers.get("hosts:available")!({ payload: { sequence: 2, hosts: [{ ...snapshot.hosts[0], name: "Renamed signed member", reachability: { kind: "closed", reason: "app-closed", at: 10 } }] } }); });
    expect(row.textContent).toContain("Renamed signed member");
    expect(row.textContent).toContain("Closed: app-closed");
    await act(async () => { native.handlers.get("hosts:available")!({ payload: { sequence: 3, hosts: [] } }); });
    expect(container.querySelector("[data-ring-host]")).toBeNull();
    expect(remove).not.toHaveBeenCalled();
  } finally {
    await act(async () => { root.unmount(); }); container.remove(); registry._dispose(); vi.unstubAllGlobals();
  }
});
