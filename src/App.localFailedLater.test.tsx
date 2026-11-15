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

// The Daemon fails AFTER start-up's first readiness read (starting → failed → ready). Real App,
// real Local migration and real TerminalTabs (only xterm and the native boundary are mocked).
const native = vi.hoisted(() => ({ handlers: new Map<string, Set<(event: { payload: unknown }) => void>>(), xterms: 0 }));
vi.mock("@tauri-apps/api/core", async original => ({ ...await original<typeof import("@tauri-apps/api/core")>(), invoke: vi.fn(), Channel: class {} }));
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
vi.mock("./lib/fonts", () => ({ detectMonoFontFamily: () => "monospace", ensureMonoFontsLoaded: async () => {} }));
vi.mock("@xterm/xterm", () => ({ Terminal: class {
  options = {}; unicode = { activeVersion: "" }; parser = { registerOscHandler: vi.fn() };
  cols = 80; rows = 24; dispose = vi.fn();
  constructor() { native.xterms++; }
  open() {} loadAddon() {} focus() {} write() {} attachCustomKeyEventHandler() {}
} }));
vi.mock("@xterm/addon-fit", () => ({ FitAddon: class { fit() {} } }));
vi.mock("@xterm/addon-web-links", () => ({ WebLinksAddon: class {} }));
vi.mock("@xterm/addon-unicode11", () => ({ Unicode11Addon: class {} }));

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
const daemon = (state: "starting" | "failed" | "ready") => ({ mode: "daemon", reason: null, persistent: { supported: false, enabled: false, running: null, log: null }, daemon: { state, error: state === "failed" ? "exited with 7" : null, errorKind: null, log: "/h/.xshell/log/xshelld.log" }, service: { mechanism: "systemd", linger: true, error: null } });
const emit = async (name: string, payload: unknown) => { await act(async () => { native.handlers.get(name)?.forEach(h => h({ payload })); }); };

it("starting → failed → ready: no migration, no in-process restore, no layout write; ready reloads", async () => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  vi.stubGlobal("fetch", vi.fn(async () => ({ ok: true, json: async () => [] })));
  const settings = new Map<string, unknown>([["open_tabs", [savedTab]], ["open_groups", []], ["rate_limit_in_sidebar", false], ["rate_limit_in_sidebar_codex", false], ["context_tree_enabled", false]]);
  const writes: string[] = [];
  vi.mocked(load).mockImplementation(async () => ({
    get: async (key: string) => settings.get(key) ?? null,
    set: async (key: string, value: unknown) => { writes.push(key); settings.set(key, value); },
    save: async () => {},
  }) as unknown as Awaited<ReturnType<typeof load>>);
  let info: unknown = daemon("starting");
  vi.mocked(invoke).mockImplementation(async (command) => {
    switch (command) {
      case "local_host_info": case "local_daemon_retry": return info;
      case "ring_status": return ringStatus();
      case "ring_join_overview": return { owner: true, joins: [] };
      case "hosts_available": return { sequence: 0, hosts: [] };
      case "hosts_status": return [];
      case "local_migration_lock": return true;
      case "get_global_rate_limits": return { five_hour_pct: null, seven_day_pct: null, five_hour_resets_at: null, seven_day_resets_at: null };
      case "get_codex_usage": return { present: false, primary: null, secondary: null, plan_type: null, rate_limits_updated_iso: null, daily_sessions: [] };
      case "detect_agent_binary": return { installed: false };
      case "probe_statusline_setup": return { stats_dir_present: false, stats_session_count: 0 };
      default: return String(command).startsWith("local_") ? null : [];
    }
  });
  const reload = vi.fn();
  vi.spyOn(window, "location", "get").mockReturnValue({ ...window.location, reload } as Location);
  container = document.createElement("div");
  document.body.append(container);
  root = createRoot(container);
  await act(async () => { root!.render(<App />); });
  await act(async () => {});
  // Start-up waits for live readiness: nothing restored or migrated yet.
  expect(container.querySelector("[role=alertdialog]")).toBeNull();
  // The Daemon fails while it starts.
  info = daemon("failed");
  await emit("local:daemon", info);
  await act(async () => {});
  expect(container.querySelector("[role=alertdialog]")?.textContent).toContain(C["local.failed.title"]);
  const commands = vi.mocked(invoke).mock.calls.map(([c]) => c as string);
  expect(commands.filter(c => c.startsWith("local_migration_") || c === "spawn_terminal" || c === "host_term_open")).toEqual([]);
  expect(native.xterms).toBe(0);
  expect(container.textContent).not.toContain("Old shell");
  expect(writes.filter(k => k === "open_tabs" || k === "open_groups")).toEqual([]);
  expect(settings.get("open_tabs")).toEqual([savedTab]);

  // Retry → starting → ready: the window reloads so start-up runs in Daemon mode.
  info = daemon("starting");
  await act(async () => { (container.querySelector("[data-local-retry]") as HTMLElement).click(); });
  expect(reload).not.toHaveBeenCalled();
  info = daemon("ready");
  await emit("local:daemon", info);
  expect(reload).toHaveBeenCalledOnce();
  expect(writes.filter(k => k === "open_tabs" || k === "open_groups")).toEqual([]);
});
