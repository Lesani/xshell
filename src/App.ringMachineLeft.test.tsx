// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, expect, it, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";
import { load } from "@tauri-apps/plugin-store";
import { member, me, ringStatus } from "./components/ringTestKit";
import type { AvailableHost, AvailableSnapshot, HostStatus, TerminalInfo } from "./hosts/types";
import type { MemberView } from "./ring/types";

// A machine leaves the Ring (removed, or removes itself) while this Desktop shows one of its
// Terminals; later it joins again under a new identity. Real App and real TerminalTabs; only
// xterm and the native boundary are mocked.
const native = vi.hoisted(() => ({ handlers: new Map<string, Set<(event: { payload: unknown }) => void>>(), written: [] as string[] }));
vi.mock("@tauri-apps/api/core", async original => ({ ...await original<typeof import("@tauri-apps/api/core")>(), invoke: vi.fn(), Channel: class { onmessage: unknown = null; } }));
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
  open() {} loadAddon() {} focus() {} write(d: unknown) { native.written.push(typeof d === "string" ? d : ""); } attachCustomKeyEventHandler() {} onData() { return { dispose() {} }; } onResize() { return { dispose() {} }; }
} }));
vi.mock("@xterm/addon-fit", () => ({ FitAddon: class { fit() {} } }));
vi.mock("@xterm/addon-web-links", () => ({ WebLinksAddon: class {} }));
vi.mock("@xterm/addon-unicode11", () => ({ Unicode11Addon: class {} }));

// 32-byte keys: base64url (what the Roster carries) and the Host id the backend derives.
const keyOf = (byte: number) => btoa(String.fromCharCode(...new Array(32).fill(byte))).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
const idOf = (byte: number) => `r_${byte.toString(16).padStart(2, "0").repeat(32)}`;
const hotel = (byte: number): MemberView => ({ ...me, name: "hotel", signKey: keyOf(byte + 1), daemonKey: keyOf(byte), thisApp: false, thisComputer: false, enrollmentId: `u${byte}` });
const info: TerminalInfo = { terminal: "hotel-uuid", spec: { agent: "claude", cwd: "/projects/h", sessionId: "s1" }, meta: { title: "Hotel work", projectName: "h" }, createdAtMs: 1, pid: 7, exitCode: null };
const row = (id: string, kind: "online" | "closed" = "online"): AvailableHost => ({ id, aliases: [id], name: "hotel", color: null, daemonKey: id, activeRoute: "relay", routes: ["relay"], reachability: kind === "online" ? { kind: "online" } : { kind: "closed", reason: "app-closed", at: 1 } } as AvailableHost);
const status = (id: string, s: HostStatus["status"] = "connected"): HostStatus => ({ host: id, status: s, phase: null, lastError: null, errorHint: null, daemonVersion: "1.5.0", desktopVersion: "1.5.0", protocol: 1, os: "linux", arch: "x86_64", incompatibleReason: null, nextRetryAt: null, sinceMs: 1, configGeneration: 1, linkGeneration: 1, updateAction: null, updateUnavailable: null } as HostStatus);

// Fresh App/registry/cache modules per test: App's start-up runs once per module instance.
let registry: typeof import("./hosts/registry").registry;
let cache: typeof import("./hosts/cache").cache;
let hostUpdates: typeof import("./hosts/hostUpdate").hostUpdates;
let root: Root | undefined;
let container: HTMLDivElement;
let seq = 1;
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
const emit = async (name: string, payload: unknown) => { await act(async () => { native.handlers.get(name)?.forEach(h => h({ payload })); }); };
const available = async (hosts: AvailableHost[]) => emit("hosts:available", { sequence: ++seq, hosts } satisfies AvailableSnapshot);
const tabs = () => container.querySelectorAll('[data-drag-id="remote-hotel-uuid"]');

async function mountWithHotel(byte: number) {
  const OLD = idOf(byte);
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  vi.stubGlobal("fetch", vi.fn(async () => ({ ok: true, json: async () => [] })));
  const settings = new Map<string, unknown>([["rate_limit_in_sidebar", false], ["rate_limit_in_sidebar_codex", false], ["context_tree_enabled", false]]);
  vi.mocked(load).mockImplementation(async () => ({
    get: async (key: string) => settings.get(key) ?? null,
    set: async (key: string, value: unknown) => { settings.set(key, value); },
    save: async () => {},
  }) as unknown as Awaited<ReturnType<typeof load>>);
  vi.mocked(invoke).mockImplementation(async (command) => {
    switch (command) {
      case "local_host_info": return { mode: "in-process", reason: null };
      case "ring_status": return member({ members: [me, hotel(byte)] });
      case "ring_join_overview": return { owner: true, joins: [] };
      case "hosts_available": return { sequence: 0, hosts: [] };
      case "hosts_status": return [];
      case "get_global_rate_limits": return { five_hour_pct: null, seven_day_pct: null, five_hour_resets_at: null, seven_day_resets_at: null };
      case "get_codex_usage": return { present: false, primary: null, secondary: null, plan_type: null, rate_limits_updated_iso: null, daily_sessions: [] };
      case "detect_agent_binary": return { installed: false };
      case "probe_statusline_setup": return { stats_dir_present: false, stats_session_count: 0 };
      case "host_term_attach": return new Promise(() => {});
      case "host_call": return null;
      default: return String(command).startsWith("local_") || String(command).startsWith("host_term") ? null : [];
    }
  });
  vi.resetModules();
  const { default: App } = await import("./App");
  ({ registry } = await import("./hosts/registry"));
  ({ cache } = await import("./hosts/cache"));
  ({ hostUpdates } = await import("./hosts/hostUpdate"));
  container = document.createElement("div");
  document.body.append(container);
  root = createRoot(container);
  await act(async () => { root!.render(<App />); });
  await act(async () => {});
  await available([row(OLD)]);
  await emit("hosts:status", status(OLD));
  await emit("hosts:terminals", { host: OLD, list: [info] });
  await act(async () => {});
  expect(tabs()).toHaveLength(1);
}

it("a machine that left: the tab stays, says so by name, never shows its Host id, and Close tab closes only it", async () => {
  const OLD = idOf(0xd1);
  await mountWithHotel(0xd1);
  // In the Ring but unreachable: offline, reconnecting.
  await available([row(OLD, "closed")]);
  await emit("hosts:status", status(OLD, "offline"));
  expect(container.textContent).toContain("hotel is offline. Reconnecting…");
  expect(container.querySelector("[data-tab-removed]")).toBeNull();
  // Offline only when the Relay says the machine is away and the Relay is the route in use;
  // otherwise the Host is being connected.
  await emit("hosts:status", status(OLD, "reconnecting"));
  expect(container.textContent).toContain("hotel is offline. Reconnecting…");
  await available([{ ...row(OLD), reachability: { kind: "unknown" } }]);
  expect(container.textContent).not.toContain("hotel is offline");
  expect(container.textContent).toContain("Waiting for hotel…");
  await available([{ ...row(OLD), activeRoute: "ssh", routes: ["ssh", "relay"], reachability: { kind: "unreachable", at: 1 } }]);
  expect(container.textContent).not.toContain("hotel is offline");
  expect(container.textContent).toContain("Waiting for hotel…");
  await available([row(OLD, "closed")]);
  await emit("hosts:status", status(OLD, "offline"));

  // hotel leaves the Ring (the Roster drops it; it is no longer available).
  await emit("ring:status", member({ members: [me] }));
  await available([]);
  const banner = container.querySelector("[data-tab-removed]")!;
  expect(banner.textContent).toContain("hotel is no longer in your Ring.");
  expect(banner.textContent).toContain("This tab stays open so you can read it.");
  expect(tabs()).toHaveLength(1);
  expect(container.textContent).not.toContain("Waiting for");
  expect(container.innerHTML).not.toContain("r_d1");
  expect(registry.hostName(OLD)).toBe("hotel (not in Ring)");

  vi.mocked(invoke).mockClear();
  await act(async () => { (container.querySelector("[data-tab-removed-close]") as HTMLElement).click(); });
  await act(async () => {});
  expect(tabs()).toHaveLength(0);
  expect(vi.mocked(invoke).mock.calls.filter(([c]) => c === "host_term_close")).toEqual([]);
  expect(cache.terminals(OLD)?.some(t => t.terminal === "hotel-uuid") ?? false).toBe(false);
});

it("the same machine joins again under a new identity: its Terminal keeps one tab, moved to the new Host", async () => {
  const NEW = idOf(0xe2);
  await mountWithHotel(0xd2);
  await emit("ring:status", member({ members: [me] }));
  await available([]);
  expect(container.querySelector("[data-tab-removed]")).not.toBeNull();

  await emit("ring:status", member({ members: [me, hotel(0xe2)] }));
  await available([row(NEW)]);
  await emit("hosts:status", status(NEW));
  await emit("hosts:terminals", { host: NEW, list: [info] });
  await act(async () => {});
  expect(tabs()).toHaveLength(1);
  expect(container.querySelector("[data-tab-removed]")).toBeNull();
  expect(container.innerHTML).not.toContain("r_e2");
  expect(container.innerHTML).not.toContain("r_d2");
});

it("while this machine is not a member, no tab is marked as left (nothing is known)", async () => {
  await mountWithHotel(0xd3);
  await emit("ring:status", ringStatus({ membership: "none" }));
  await available([]);
  expect(container.querySelector("[data-tab-removed]")).toBeNull();
  expect(container.innerHTML).not.toContain("r_d3");
});

it("removed → restart (the tab comes back from disk, removed) → the machine joins again: the tab moves to the new Host and attaches", async () => {
  const OLD = idOf(0xd4), NEW = idOf(0xe4);
  native.written = [];
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  vi.stubGlobal("fetch", vi.fn(async () => ({ ok: true, json: async () => [] })));
  // What B kept on disk from before the restart: hotel's last list and its name.
  const stores = new Map<string, Map<string, unknown>>([
    ["settings.json", new Map<string, unknown>([["rate_limit_in_sidebar", false], ["rate_limit_in_sidebar_codex", false], ["context_tree_enabled", false]])],
    ["hosts-cache.json", new Map<string, unknown>([["cache", { version: 1, hosts: { [OLD]: { provenance: { host: OLD, daemonKey: OLD }, terminals: [info], terminalsAt: 1, calls: {} } }, names: { [OLD]: "hotel" } }]])],
  ]);
  vi.mocked(load).mockImplementation(async (file: string) => {
    const data = stores.get(file) ?? new Map<string, unknown>(); stores.set(file, data);
    return { get: async (k: string) => data.get(k) ?? null, set: async (k: string, v: unknown) => { data.set(k, v); }, save: async () => {} } as unknown as Awaited<ReturnType<typeof load>>;
  });
  let roster = member({ members: [me] });
  vi.mocked(invoke).mockImplementation(async (command, args) => {
    const a = args as Record<string, unknown> | undefined;
    switch (command) {
      case "local_host_info": return { mode: "in-process", reason: null };
      case "ring_status": return roster;
      case "ring_join_overview": return { owner: true, joins: [] };
      case "hosts_available": return { sequence: 0, hosts: [] };
      case "hosts_status": return [];
      case "get_global_rate_limits": return { five_hour_pct: null, seven_day_pct: null, five_hour_resets_at: null, seven_day_resets_at: null };
      case "get_codex_usage": return { present: false, primary: null, secondary: null, plan_type: null, rate_limits_updated_iso: null, daily_sessions: [] };
      case "detect_agent_binary": return { installed: false };
      case "probe_statusline_setup": return { stats_dir_present: false, stats_session_count: 0 };
      case "host_call": return null;
      case "host_term_attach": case "host_term_open":
        if (a?.host !== NEW) { throw { code: "unknown-host", message: `unknown host ${String(a?.host)}` }; }
        return new Promise(() => {});
      default: return String(command).startsWith("local_") || String(command).startsWith("host_term") ? null : [];
    }
  });
  vi.resetModules();
  const { default: App } = await import("./App");
  ({ registry } = await import("./hosts/registry"));
  ({ cache } = await import("./hosts/cache"));
  ({ hostUpdates } = await import("./hosts/hostUpdate"));
  container = document.createElement("div");
  document.body.append(container);
  root = createRoot(container);
  await act(async () => { root!.render(<App />); });
  await act(async () => {});
  await act(async () => {});
  // Restored from disk, in the removed state.
  expect(tabs()).toHaveLength(1);
  expect(container.querySelector("[data-tab-removed]")?.textContent).toContain("hotel is no longer in your Ring.");
  (container.querySelector('[data-drag-id="remote-hotel-uuid"]') as HTMLElement).click();
  await act(async () => {});

  // hotel joins again: a new identity, its Daemon and the Terminal kept running.
  roster = member({ members: [me, hotel(0xe4)] });
  await emit("ring:status", roster);
  await available([row(NEW)]);
  await emit("hosts:status", status(NEW));
  await emit("hosts:terminals", { host: NEW, list: [info] });
  for (let i = 0; i < 5; i++) await act(async () => { await new Promise(r => setTimeout(r, 0)); });
  expect(tabs()).toHaveLength(1);
  expect(container.querySelector("[data-tab-removed]")).toBeNull();
  const attaches = vi.mocked(invoke).mock.calls.filter(([c]) => c === "host_term_attach" || c === "host_term_open").map(([c, a]) => [c, (a as { host: string }).host]);
  expect(attaches).toContainEqual(["host_term_attach", NEW]);
  expect(attaches.filter(([, h]) => h === OLD)).toEqual([]);
  expect(native.written.join("")).not.toContain("Couldn't start");
  expect(container.innerHTML).not.toContain("r_d4");
});

it("removed → restart → rejoin without that Terminal: the tab keeps the removed state, no error text", async () => {
  const OLD = idOf(0xd5), NEW = idOf(0xe5);
  native.written = [];
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  vi.stubGlobal("fetch", vi.fn(async () => ({ ok: true, json: async () => [] })));
  // What B kept on disk from before the restart: hotel's last list and its name.
  const stores = new Map<string, Map<string, unknown>>([
    ["settings.json", new Map<string, unknown>([["rate_limit_in_sidebar", false], ["rate_limit_in_sidebar_codex", false], ["context_tree_enabled", false]])],
    ["hosts-cache.json", new Map<string, unknown>([["cache", { version: 1, hosts: { [OLD]: { provenance: { host: OLD, daemonKey: OLD }, terminals: [info], terminalsAt: 1, calls: {} } }, names: { [OLD]: "hotel" } }]])],
  ]);
  vi.mocked(load).mockImplementation(async (file: string) => {
    const data = stores.get(file) ?? new Map<string, unknown>(); stores.set(file, data);
    return { get: async (k: string) => data.get(k) ?? null, set: async (k: string, v: unknown) => { data.set(k, v); }, save: async () => {} } as unknown as Awaited<ReturnType<typeof load>>;
  });
  let roster = member({ members: [me] });
  vi.mocked(invoke).mockImplementation(async (command, args) => {
    const a = args as Record<string, unknown> | undefined;
    switch (command) {
      case "local_host_info": return { mode: "in-process", reason: null };
      case "ring_status": return roster;
      case "ring_join_overview": return { owner: true, joins: [] };
      case "hosts_available": return { sequence: 0, hosts: [] };
      case "hosts_status": return [];
      case "get_global_rate_limits": return { five_hour_pct: null, seven_day_pct: null, five_hour_resets_at: null, seven_day_resets_at: null };
      case "get_codex_usage": return { present: false, primary: null, secondary: null, plan_type: null, rate_limits_updated_iso: null, daily_sessions: [] };
      case "detect_agent_binary": return { installed: false };
      case "probe_statusline_setup": return { stats_dir_present: false, stats_session_count: 0 };
      case "host_call": return null;
      case "host_term_attach": case "host_term_open":
        if (a?.host !== NEW) { throw { code: "unknown-host", message: `unknown host ${String(a?.host)}` }; }
        return new Promise(() => {});
      default: return String(command).startsWith("local_") || String(command).startsWith("host_term") ? null : [];
    }
  });
  vi.resetModules();
  const { default: App } = await import("./App");
  ({ registry } = await import("./hosts/registry"));
  ({ cache } = await import("./hosts/cache"));
  ({ hostUpdates } = await import("./hosts/hostUpdate"));
  container = document.createElement("div");
  document.body.append(container);
  root = createRoot(container);
  await act(async () => { root!.render(<App />); });
  await act(async () => {});
  await act(async () => {});
  // Restored from disk, in the removed state.
  expect(tabs()).toHaveLength(1);
  expect(container.querySelector("[data-tab-removed]")?.textContent).toContain("hotel is no longer in your Ring.");
  (container.querySelector('[data-drag-id="remote-hotel-uuid"]') as HTMLElement).click();
  await act(async () => {});

  // hotel joins again: a new identity, its Daemon and the Terminal kept running.
  roster = member({ members: [me, hotel(0xe5)] });
  await emit("ring:status", roster);
  await available([row(NEW)]);
  await emit("hosts:status", status(NEW));
  await emit("hosts:terminals", { host: NEW, list: [] });
  for (let i = 0; i < 5; i++) await act(async () => { await new Promise(r => setTimeout(r, 0)); });
  expect(tabs()).toHaveLength(1);
  expect(container.querySelector("[data-tab-removed]")?.textContent).toContain("hotel is no longer in your Ring.");
  expect(native.written.join("")).not.toContain("Couldn't");
  expect(native.written.join("")).not.toContain("unknown host");
  expect(container.innerHTML).not.toContain("r_d5");
  expect(container.innerHTML).not.toContain("r_e5");
});
