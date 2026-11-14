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
import { S } from "./ring/strings";
import type { RingStatus } from "./ring/types";

const native = vi.hoisted(() => ({ handlers: new Map<string, Set<(event: { payload: unknown }) => void>>() }));

// App, Settings and the Ring section stay real; only the native boundary is replaced.
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

const ring: RingStatus = {
  enabled: false, ringId: null, version: null, relayUrl: null, hostedRelayUrl: "wss://r", connection: "off",
  retryIn: null, connectionError: null, limited: false, farewells: [], problem: null, problemDetail: null,
  members: [], quotaResetAt: null, relayHosted: true, local: "daemon", hosts: [], approval: "none", format: null,
};

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
  vi.unstubAllGlobals();
});

it("ring:focus opens Settings on Hosts with the Ring section", async () => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  vi.stubGlobal("fetch", vi.fn(async () => ({ ok: true, json: async () => [] })));
  const settings = new Map<string, unknown>([["rate_limit_in_sidebar", false], ["rate_limit_in_sidebar_codex", false], ["context_tree_enabled", false]]);
  vi.mocked(load).mockImplementation(async () => ({
    get: async (key: string) => settings.get(key) ?? null,
    set: async (key: string, value: unknown) => { settings.set(key, value); },
    save: async () => {},
  }) as unknown as Awaited<ReturnType<typeof load>>);
  vi.mocked(invoke).mockImplementation(async (command, args) => {
    const a = args as Record<string, unknown> | undefined;
    const method = command === "host_call" ? a?.method : command;
    switch (method) {
      case "local_host_info": return { mode: "in-process", reason: null };
      case "hosts_available": return { sequence: 0, hosts: [] };
      case "hosts_configure": return;
      case "hosts_status": return [];
      case "app_build": return { version: "1.5.0", build: { commit: "7ab4537a9baf", dirty: null, time: 2 } };
      case "ring_status": return ring;
      case "ring_join_overview": return { owner: true, joins: [] };
      case "get_global_rate_limits": return { five_hour_pct: null, seven_day_pct: null, five_hour_resets_at: null, seven_day_resets_at: null };
      case "get_codex_usage": return { present: false, primary: null, secondary: null, plan_type: null, rate_limits_updated_iso: null, daily_sessions: [] };
      case "detect_agent_binary": return { installed: false };
      case "probe_statusline_setup": return { stats_dir_present: false, stats_session_count: 0 };
      default: return [];
    }
  });
  container = document.createElement("div");
  document.body.append(container);
  root = createRoot(container);
  await act(async () => { root!.render(<App />); });
  const settingsShown = () => (container.querySelector(".settings-view")?.parentElement as HTMLElement | null)?.style.display === "flex";
  expect(settingsShown()).toBe(false);
  // There is no separate Mobile category any more.
  expect(Array.from(container.querySelectorAll(".settings-nav-item")).map(n => n.textContent)).not.toContain("Mobile");

  await act(async () => { native.handlers.get("ring:focus")!.forEach(h => h({ payload: null })); });
  await act(async () => {});
  expect(settingsShown()).toBe(true);
  expect(container.querySelector(".settings-nav-item.active")?.textContent).toBe("Hosts");
  expect(container.querySelector("[data-ring-section]")?.textContent).toContain(S["ring.section.title"]);
  expect(container.querySelector("[data-ring-create]")).not.toBeNull();
});
