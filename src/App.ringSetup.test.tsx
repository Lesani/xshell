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
import { member, ringStatus } from "./components/ringTestKit";
import type { RingStatus } from "./ring/types";

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
let settings: Map<string, unknown>;
let ring: RingStatus;
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

async function mountApp() {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  vi.stubGlobal("fetch", vi.fn(async () => ({ ok: true, json: async () => [] })));
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
      case "ring_join_cancel": return null;
      case "local_quit_prompt_ready": return null;
      case "get_global_rate_limits": return { five_hour_pct: null, seven_day_pct: null, five_hour_resets_at: null, seven_day_resets_at: null };
      case "get_codex_usage": return { present: false, primary: null, secondary: null, plan_type: null, rate_limits_updated_iso: null, daily_sessions: [] };
      case "detect_agent_binary": return { installed: false };
      case "probe_statusline_setup": return { stats_dir_present: false, stats_session_count: 0 };
      default: return [];
    }
  });
  container?.remove();
  container = document.createElement("div");
  document.body.append(container);
  root = createRoot(container);
  await act(async () => { root!.render(<App />); });
  await act(async () => {});
}
async function remount() {
  await act(async () => { root?.unmount(); });
  root = createRoot(container);
  await act(async () => { root!.render(<App />); });
  await act(async () => {});
}
const q = (sel: string) => container.querySelector<HTMLElement>(sel);
const click = async (el: HTMLElement | null | undefined) => { expect(el).toBeTruthy(); await act(async () => { el!.click(); }); };
const setupShown = () => q("[data-ring-setup]");
const navItem = (label: string) => Array.from(container.querySelectorAll<HTMLElement>(".settings-nav-item")).find(e => e.textContent === label);
const emit = async (name: string, payload: unknown) => { await act(async () => { native.handlers.get(name)?.forEach(h => h({ payload })); }); };

function freshSettings() {
  settings = new Map<string, unknown>([["rate_limit_in_sidebar", false], ["rate_limit_in_sidebar_codex", false], ["context_tree_enabled", false]]);
}

it("membership none and no skip → Connect your machines shown; after Skip and remount → Home; Settings → Ring → Set up your Ring reopens it", async () => {
  freshSettings();
  ring = ringStatus();
  await mountApp();
  expect(setupShown()?.getAttribute("data-ring-setup")).toBe("first");
  expect(q(".ring-setup-title")?.textContent).toBe(C["first.title"]);

  await click(q("[data-ring-skip]"));
  await act(async () => {});
  expect(settings.get("ring_setup_skipped")).toBe(true);
  expect(setupShown()).toBeNull();

  await remount();
  expect(setupShown()).toBeNull();

  await click(q('[aria-label="Settings"]'));
  await click(navItem(C["ring.title"]));
  expect(q("[data-ring-section]")?.textContent).toContain(C["ring.body"]);
  await click(q("[data-ring-setup-open]"));
  expect(setupShown()?.getAttribute("data-ring-setup")).toBe("first");
});

it("a member opens on Home; a removal while Home is open raises a notice whose Join again opens the words (U3)", async () => {
  freshSettings();
  ring = member();
  await mountApp();
  expect(setupShown()).toBeNull();
  expect(q("[data-ring-removed-notice]")).toBeNull();

  ring = ringStatus({ membership: "removed", removed: { ringId: "r", at: 42 } });
  await emit("ring:status", ring);
  expect(q("[data-ring-removed-notice]")?.textContent).toContain(C["removed.title"]);
  // Not now dismisses this removal's notice.
  await click(q("[data-ring-removed-dismiss]"));
  expect(q("[data-ring-removed-notice]")).toBeNull();
  // A later removal raises it again; Join again opens the setup screen.
  ring = ringStatus({ membership: "removed", removed: { ringId: "r", at: 43 } });
  await emit("ring:status", ring);
  await click(q("[data-ring-removed-join]"));
  expect(setupShown()?.getAttribute("data-ring-setup")).toBe("removed");
  expect(q("[data-ring-removed-notice]")).toBeNull();
});

it("a removed machine opens on the removed screen even after Skip", async () => {
  freshSettings();
  settings.set("ring_setup_skipped", true);
  ring = ringStatus({ membership: "removed", removed: { ringId: "r", at: 5 } });
  await mountApp();
  expect(setupShown()?.getAttribute("data-ring-setup")).toBe("removed");
  expect(q(".ring-setup-title")?.textContent).toBe(C["removed.title"]);
});

it("leaving the setup screen for Settings cancels the shown words", async () => {
  freshSettings();
  ring = ringStatus();
  await mountApp();
  vi.mocked(invoke).mockClear();
  vi.mocked(invoke).mockImplementation(async (command, args) => {
    const a = args as Record<string, unknown> | undefined;
    switch (command) {
      case "ring_status": return ring;
      case "ring_join_overview": return { owner: true, joins: [] };
      case "ring_join_start": return { operation: a?.operation, words: "acid-baker-cable-dance-eagle-fable-gamma-habit", expiresAt: Math.floor(Date.now() / 1000) + 600 };
      case "ring_join_cancel": return null;
      default: return [];
    }
  });
  await click(q("[data-ring-join-action]"));
  expect(q("[data-ring-words]")).not.toBeNull();
  const op = vi.mocked(invoke).mock.calls.find(([c]) => c === "ring_join_start")![1] as { operation: string };
  await click(q('[aria-label="Settings"]'));
  expect(q("[data-ring-words]")).toBeNull();
  expect(vi.mocked(invoke).mock.calls.filter(([c]) => c === "ring_join_cancel").map(([, a]) => a)).toEqual([{ operation: op.operation }]);
});
