// @vitest-environment happy-dom
import { act } from "react";
import { createRoot } from "react-dom/client";
import { expect, it, vi } from "vitest";
import { ProjectPicker } from "./ProjectPicker";
import { registry } from "../hosts/registry";
import { cache } from "../hosts/cache";
import type { AvailableSnapshot, HostStatus } from "../hosts/types";

const native = vi.hoisted(() => ({ invoke: vi.fn(), handlers: new Map<string, (event: { payload: unknown }) => void>() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: native.invoke }));
vi.mock("@tauri-apps/api/event", () => ({ listen: async (name: string, handler: (event: { payload: unknown }) => void) => { native.handlers.set(name, handler); return () => native.handlers.delete(name); } }));
vi.mock("@tauri-apps/plugin-store", () => ({ load: async () => ({ get: async () => null, set: async () => {}, save: async () => {} }) }));

it("selects Ring history at the real command/event boundary with zero saved SSH, stale provenance and Local dedup", async () => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  const id = `r_${"b".repeat(64)}`, local = `r_${"c".repeat(64)}`;
  const snapshot: AvailableSnapshot = { sequence: 1, hosts: [
    { id, aliases: [id], name: "Ring-only Daemon", color: null, daemonKey: "key", routes: ["relay"], activeRoute: "relay", reachability: { kind: "online" } },
    { id: "local", aliases: ["local", local], name: "Local equivalent", color: null, daemonKey: "local-key", routes: ["local", "relay"], activeRoute: "local", reachability: { kind: "unknown" } },
  ] };
  let offline = false;
  native.invoke.mockImplementation(async (command, args) => {
    if (command === "hosts_available") return snapshot;
    if (command === "hosts_status") return [];
    const method = command === "host_call" ? args.method : command;
    if (command === "host_call" && offline) throw { code: "offline", message: "Relay lost" };
    if (method === "list_claude_projects") return command === "host_call" ? [{ path: "/history/ring-project", session_count: 2, last_active: "2026-11-12" }] : [];
    if (method.startsWith("list_")) return [];
    if (method === "detect_agent_binary") return { installed: false };
  });
  await cache.load(); await registry.init([]);
  registry.applyStatus({ host: id, status: "connected" } as HostStatus);
  const container = document.createElement("div"); document.body.append(container);
  const root = createRoot(container), toggle = vi.fn(), add = vi.fn(async () => "ok" as const);
  const render = (key: string) => root.render(<ProjectPicker key={key} allProjects={[]} savedPaths={[]} onToggle={toggle} onBrowse={() => {}} onClose={() => {}} onAddRemotePath={add} />);
  try {
    await act(async () => { render("live"); });
    expect(container.querySelectorAll(".picker-group")).toHaveLength(1);
    expect(container.textContent).toContain("Ring-only Daemon");
    expect(container.textContent).not.toContain("Local equivalent");
    const row = container.querySelector<HTMLElement>(".picker-group .picker-item")!;
    expect(row.textContent).toContain("ring-project");
    await act(async () => { row.click(); });
    expect(toggle).toHaveBeenCalledWith(`host:${id}:/history/ring-project`);
    expect(native.invoke.mock.calls.some(([command, args]) => command === "host_call" && args.host === id && args.method === "list_claude_projects")).toBe(true);
    offline = true;
    await act(async () => {
      native.handlers.get("hosts:status")!({ payload: { host: id, status: "offline" } });
      render("offline");
    });
    expect(container.querySelector(".picker-group-stale")?.textContent).toContain("Cached");
    expect(container.querySelector(".picker-group .picker-item")?.textContent).toContain("ring-project");
    expect(registry.getSnapshot().configs).toEqual([]);
    expect(native.invoke.mock.calls.some(([command]) => ["host_test", "host_upgrade"].includes(command))).toBe(false);
  } finally {
    await act(async () => { root.unmount(); }); container.remove(); registry._dispose(); cache._reset(); vi.unstubAllGlobals();
  }
});
