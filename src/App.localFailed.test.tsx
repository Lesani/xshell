// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, expect, it, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";
import { load } from "@tauri-apps/plugin-store";
import App from "./App";
import { registry } from "./hosts/registry";
import { cache } from "./hosts/cache";
import { hostUpdates } from "./hosts/hostUpdate";
import { C } from "./ring/copy";
import { ringStatus } from "./components/ringTestKit";

const native = vi.hoisted(() => ({ handlers: new Map<string, Set<(event: { payload: unknown }) => void>>() }));

// App, Settings and the Ring screens stay real; only the native boundary is replaced.
vi.mock("@tauri-apps/api/core", async importOriginal => ({
  ...await importOriginal<typeof import("@tauri-apps/api/core")>(),
  invoke: vi.fn(),
}));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async (name: string, handler: (event: { payload: unknown }) => void) => {
    const set = native.handlers.get(name) ?? new Set();
    set.add(handler);
    native.handlers.set(name, set);
    return () => { set.delete(handler); };
  }),
}));
vi.mock("@tauri-apps/api/window", () => ({ getCurrentWindow: () => ({ setAlwaysOnTop: async () => {} }) }));
vi.mock("@tauri-apps/api/app", () => ({ getVersion: async () => "1.5.0" }));
vi.mock("@tauri-apps/plugin-updater", () => ({ check: async () => null }));
vi.mock("@tauri-apps/plugin-store", () => ({ load: vi.fn() }));
vi.mock("./components/TerminalTab", () => ({ TerminalTab: () => null, DARK_TERM_BG: "#1c1c1b", LIGHT_TERM_BG: "#faf9f5" }));

let root: Root | undefined;
let container: HTMLDivElement;
afterEach(async () => {
  await act(async () => { root?.unmount(); });
  root = undefined;
  container?.remove();
  registry._dispose();
  cache._reset();
  hostUpdates.cancel();
  native.handlers.clear();
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

const savedTab = { id: "t1", title: "Old shell", projectPath: "/p", projectName: "p", sessionId: "s-1", agent: "claude" };

it("mode failed → blocking view; no spawn_terminal; open_tabs never written; recovery reloads the window", async () => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  vi.stubGlobal("fetch", vi.fn(async () => ({ ok: true, json: async () => [] })));
  const settings = new Map<string, unknown>([["open_tabs", [savedTab]], ["rate_limit_in_sidebar", false], ["rate_limit_in_sidebar_codex", false], ["context_tree_enabled", false]]);
  const writes: string[] = [];
  vi.mocked(load).mockImplementation(async () => ({
    get: async (key: string) => settings.get(key) ?? null,
    set: async (key: string, value: unknown) => { writes.push(key); settings.set(key, value); },
    save: async () => {},
  }) as unknown as Awaited<ReturnType<typeof load>>);
  let info: unknown = { mode: "failed", reason: "home directory is unknown", persistent: { supported: false, enabled: false, running: null, log: null }, daemon: null, service: null };
  vi.mocked(invoke).mockImplementation(async (command) => {
    switch (command) {
      case "local_host_info": return info;
      case "local_daemon_retry": return info;
      case "ring_status": return ringStatus();
      case "ring_join_overview": return { owner: true, joins: [] };
      case "hosts_available": return { sequence: 0, hosts: [] };
      case "hosts_status": return [];
      default: return [];
    }
  });
  const reload = vi.fn();
  vi.spyOn(window, "location", "get").mockReturnValue({ ...window.location, reload } as Location);
  container = document.createElement("div");
  document.body.append(container);
  root = createRoot(container);
  await act(async () => { root!.render(<App />); });
  await act(async () => {});
  const view = container.querySelector("[role=alertdialog]");
  expect(view?.textContent).toContain(C["local.failed.title"]);
  expect(view?.textContent).toContain("~/.xshell/log/xshelld.log");
  // Nothing else opens over it: no setup screen while local Terminals cannot start.
  expect(container.querySelector("[data-ring-setup]")).toBeNull();
  expect(vi.mocked(invoke).mock.calls.map(([c]) => c)).not.toContain("spawn_terminal");
  expect(writes).not.toContain("open_tabs");
  expect(writes).not.toContain("open_groups");

  // Retry → the Daemon starts → ready: the startup runs again (reload).
  info = { mode: "daemon", reason: null, daemon: { state: "starting", error: null, errorKind: null, log: "/h/.xshell/log/xshelld.log" }, service: null };
  await act(async () => { (container.querySelector("[data-local-retry]") as HTMLElement).click(); });
  expect(container.querySelector("[role=alertdialog]")).toBeNull();
  expect(reload).not.toHaveBeenCalled();
  await act(async () => { native.handlers.get("local:daemon")?.forEach(h => h({ payload: { ...(info as object), daemon: { state: "ready", error: null, errorKind: null, log: null } } })); });
  expect(reload).toHaveBeenCalledOnce();
  expect(writes).not.toContain("open_tabs");
});
