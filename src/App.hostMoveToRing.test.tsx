// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, expect, it, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";
import { load } from "@tauri-apps/plugin-store";
import { member, me } from "./components/ringTestKit";
import type { AvailableHost, AvailableSnapshot, HostConfig, HostStatus, TerminalInfo } from "./hosts/types";
import type { MemberView } from "./ring/types";

// Add to Ring on a saved SSH Host moves it to the Ring list (#119). Real App, Settings and
// TerminalTab; only xterm and the native boundary are mocked.
const native = vi.hoisted(() => ({ handlers: new Map<string, Set<(event: { payload: unknown }) => void>>() }));
const xterms = vi.hoisted(() => [] as { dispose: ReturnType<typeof vi.fn>; onInput?: (data: string) => void }[]);
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
vi.mock("@tauri-apps/api/app", () => ({ getVersion: async () => "1.6.0" }));
vi.mock("@tauri-apps/plugin-updater", () => ({ check: async () => null }));
vi.mock("@tauri-apps/plugin-store", () => ({ load: vi.fn() }));
vi.mock("./lib/fonts", () => ({ detectMonoFontFamily: () => "monospace", ensureMonoFontsLoaded: async () => {} }));
vi.mock("@xterm/xterm", () => ({ Terminal: class {
  options = {}; unicode = { activeVersion: "" }; parser = { registerOscHandler: vi.fn() };
  cols = 80; rows = 24; dispose = vi.fn(); onInput?: (data: string) => void;
  constructor() { xterms.push(this); }
  open() {} loadAddon() {} focus() {} write() {} attachCustomKeyEventHandler() {}
  onData(cb: (data: string) => void) { this.onInput = cb; return { dispose() {} }; }
  onResize() { return { dispose() {} }; }
} }));
vi.mock("@xterm/addon-fit", () => ({ FitAddon: class { fit() {} } }));
vi.mock("@xterm/addon-web-links", () => ({ WebLinksAddon: class {} }));
vi.mock("@xterm/addon-unicode11", () => ({ Unicode11Addon: class {} }));

const KEY = btoa(String.fromCharCode(...new Array(32).fill(0xb0))).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
const H = "h_boxboxbo", R = `r_${"b0".repeat(32)}`;
const CONFIG: HostConfig = { id: H, name: "box", sshTarget: "dev" };
const box: MemberView = { ...me, name: "box", signKey: KEY, daemonKey: null, kind: "computer", thisApp: false, thisComputer: false, enrollmentId: "u9", presence: { kind: "online" }, daemonPresence: null };
const info: TerminalInfo = { terminal: "box-uuid", spec: { cwd: "/p", shellMode: "raw", shellId: "bash" }, meta: { title: "Box shell", projectName: "p" }, createdAtMs: 1, pid: 7, exitCode: null };
const savedRow = (over: Partial<AvailableHost> = {}): AvailableHost => ({ id: H, aliases: [H], name: "box", color: null, daemonKey: null, activeRoute: "ssh", routes: ["ssh"], reachability: { kind: "unknown" }, ...over });
const memberRow = (kind: "online" | "unknown" = "online") => savedRow({ aliases: [H, R], daemonKey: KEY, routes: ["ssh", "relay"], reachability: { kind } });
const ringRow: AvailableHost = { id: R, aliases: [R], name: "box", color: null, daemonKey: KEY, activeRoute: "relay", routes: ["relay"], reachability: { kind: "online" } };
const status = (host: string, generation = 1): HostStatus => ({ host, status: "connected", phase: null, lastError: null, errorHint: null, daemonVersion: "1.6.0", desktopVersion: "1.6.0", protocol: 1, os: "linux", arch: "x86_64", incompatibleReason: null, nextRetryAt: null, sinceMs: 1, configGeneration: generation, linkGeneration: generation, daemonCapabilities: ["ring"], updateAction: null, updateUnavailable: null } as HostStatus);
const PI_ENTRY = (host: string) => JSON.stringify([host, "pi", "/p", "pi-1"]);

let registry: typeof import("./hosts/registry").registry;
let cache: typeof import("./hosts/cache").cache;
let root: Root | undefined;
let container: HTMLDivElement;
let seq = 1;
let settings: Map<string, unknown>;
let calls: string[];
// Per test: how the native side answers.
let world: {
  config: HostConfig;
  start: AvailableHost;
  members: MemberView[];
  add: () => Promise<unknown>;
  keep: () => Promise<unknown>;
  detach: () => Promise<unknown>;
  saveHosts: () => Promise<void>;
  configure: (hosts: HostConfig[]) => Promise<void>;
};

afterEach(async () => {
  await act(async () => { root?.unmount(); });
  root = undefined;
  container?.remove();
  registry._dispose();
  cache._reset();
  native.handlers.clear();
  xterms.length = 0;
  vi.unstubAllGlobals();
});
const emit = async (name: string, payload: unknown) => { await act(async () => { native.handlers.get(name)?.forEach(h => h({ payload })); }); };
const available = async (hosts: AvailableHost[]) => emit("hosts:available", { sequence: ++seq, hosts } satisfies AvailableSnapshot);
const settle = async (ms = 0) => { await act(async () => { await new Promise(r => setTimeout(r, ms)); }); };
const tabs = () => container.querySelectorAll('[data-drag-id="remote-box-uuid"]');
const q = (selector: string) => container.querySelector<HTMLElement>(selector);
const activeCategory = () => q(".settings-nav-item.active")?.textContent?.trim();
const clickText = async (selector: string, text: string) => {
  const el = Array.from(container.querySelectorAll<HTMLElement>(selector)).find(e => e.textContent?.trim() === text);
  expect(el, `${selector}: ${text}`).toBeTruthy();
  await act(async () => { el!.click(); });
};
const count = (prefix: string) => calls.filter(c => c.startsWith(prefix)).length;

async function mount(over: Partial<typeof world> = {}, waitMs = 5000) {
  calls = [];
  world = {
    config: CONFIG, start: savedRow(), members: [me],
    add: async () => ({ names: ["box"], version: 4 }),
    keep: async () => null, detach: async () => null, saveHosts: async () => {},
    // The backend's answer to the saved Host leaving: the machine as a Ring row.
    configure: async hosts => {
      if (hosts.some(h => h.id === H)) return;
      native.handlers.get("hosts:available")?.forEach(h => h({ payload: { sequence: ++seq, hosts: [ringRow] } }));
      native.handlers.get("hosts:status")?.forEach(h => h({ payload: status(R, 2) }));
      native.handlers.get("hosts:terminals")?.forEach(h => h({ payload: { host: R, list: [info] } }));
    },
    ...over,
  };
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  vi.stubGlobal("fetch", vi.fn(async () => ({ ok: true, json: async () => [] })));
  vi.stubGlobal("ResizeObserver", class { observe() {} disconnect() {} });
  vi.stubGlobal("IntersectionObserver", class { observe() {} disconnect() {} });
  vi.stubGlobal("requestAnimationFrame", (cb: FrameRequestCallback) => { queueMicrotask(() => cb(0)); return 1; });
  Element.prototype.scrollIntoView = vi.fn();
  settings = new Map<string, unknown>([
    ["hosts", [world.config]],
    ["sidebar_layout", [{ kind: "folder", id: "f1", name: "Work", collapsed: false, projectPaths: [`host:${H}:/p`, "/local"] }, { kind: "project", path: `host:${H}:/q` }]],
    ["project_paths", [`host:${H}:/p`, "/local", `host:${H}:/q`]],
    ["project_icons", { [`host:${H}:/p`]: { customName: "Proj", icon: "PX", folders: [{ id: "sf", name: "Kept", sessionIds: ["raw-1", PI_ENTRY(H)] }] }, "/local": { customName: "Mine" } }],
    ["terminal_right_panels", { version: 1, entries: { [JSON.stringify(["daemon", H, "box-uuid"])]: "git" } }],
    ["update_skipped_versions", { [H]: "release:9.9.9" }],
    ["file_explorer_on_start", false], ["webgl_rendering_enabled", false], ["terminal_header_stats", false], ["eager_init_tabs", true],
    ["rate_limit_in_sidebar", false], ["rate_limit_in_sidebar_codex", false], ["context_tree_enabled", false],
  ]);
  const stores = new Map<string, Map<string, unknown>>([["settings.json", settings]]);
  vi.mocked(load).mockImplementation(async file => {
    const data = stores.get(file) ?? new Map<string, unknown>(); stores.set(file, data);
    return {
      get: async (key: string) => data.get(key) ?? null,
      set: async (key: string, value: unknown) => {
        if (file === "settings.json" && key === "hosts") { calls.push(`save-hosts:${(value as HostConfig[]).length}`); await world.saveHosts(); }
        data.set(key, value);
      },
      save: async () => {},
    } as unknown as Awaited<ReturnType<typeof load>>;
  });
  vi.mocked(invoke).mockImplementation(async (command, args) => {
    const a = args as Record<string, unknown> | undefined;
    switch (command) {
      case "local_host_info": return { mode: "in-process", reason: null };
      case "ring_status": return member({ members: world.members });
      case "ring_join_overview": return { owner: true, joins: [] };
      case "hosts_available": return { sequence: 1, hosts: [world.start] };
      case "hosts_status": return registry?.isConfigured(H) ? [{ status: status(H), terminals: [info] }] : [{ status: status(R, 2), terminals: [info] }];
      case "hosts_configure": calls.push(`configure:${(a?.hosts as HostConfig[]).map(h => h.name).join(",")}`); return world.configure(a?.hosts as HostConfig[]);
      case "ring_add_ssh_host": calls.push("add"); return world.add();
      case "ring_ssh_host_keep_running": calls.push(`keep:${String(a?.host)}`); return world.keep();
      case "host_term_attach": calls.push(`attach:${String(a?.host)}`); return { exitCode: null };
      case "host_term_detach": calls.push(`detach:${String(a?.host)}`); return world.detach();
      case "host_term_input": calls.push(`input:${String(a?.host)}`); return null;
      case "get_global_rate_limits": return { five_hour_pct: null, seven_day_pct: null, five_hour_resets_at: null, seven_day_resets_at: null };
      case "get_codex_usage": return { present: false, primary: null, secondary: null, plan_type: null, rate_limits_updated_iso: null, daily_sessions: [] };
      case "detect_agent_binary": return { installed: false };
      case "probe_statusline_setup": return { stats_dir_present: false, stats_session_count: 0 };
      case "get_git_status": return { is_repo: false, files: [], branch: null };
      case "app_build": return { version: "1.6.0", build: null };
      case "app_updates_enabled": return true;
      case "detect_session_branch": case "host_call": return null;
      default: return String(command).startsWith("local_") || String(command).startsWith("host_term") ? null : [];
    }
  });
  vi.resetModules();
  const { default: App } = await import("./App");
  ({ registry } = await import("./hosts/registry"));
  ({ cache } = await import("./hosts/cache"));
  // The real gate; the tests in which it never opens wait only briefly.
  (await import("./hosts/sshRing")).ringMove.waitMs = waitMs;
  container = document.createElement("div");
  document.body.append(container);
  root = createRoot(container);
  await act(async () => { root!.render(<App />); });
  await settle(5);
  expect(tabs()).toHaveLength(1);
  expect(calls).toContain(`attach:${H}`);
  await act(async () => { q('[aria-label="Settings"]')!.click(); });
  await clickText(".settings-nav-item", "Hosts");
}

// The Roster and the Host's row as they are once `ring_add_ssh_host` has returned.
async function becameMember(kind: "online" | "unknown" = "online") {
  world.members = [me, box];
  await emit("ring:status", member({ members: world.members }));
  await available([memberRow(kind)]);
}
async function addToRing(kind: "online" | "unknown" = "online") {
  await act(async () => { q("[data-ring-add]")!.click(); });
  await becameMember(kind);
}
const layoutKeys = () => (settings.get("sidebar_layout") as ({ path?: string; projectPaths?: string[] })[]).flatMap(i => i.projectPaths ?? [i.path!]);

function expectStillSaved() {
  expect(settings.get("hosts")).toEqual([world.config]);
  expect(registry.isConfigured(H)).toBe(true);
  expect(count("configure:")).toBe(1); // the start-up one only
  expect(layoutKeys()).toEqual([`host:${H}:/p`, "/local", `host:${H}:/q`]);
  expect(Object.keys(settings.get("project_icons") as object)).toEqual([`host:${H}:/p`, "/local"]);
  expect(settings.get("update_skipped_versions")).toEqual({ [H]: "release:9.9.9" });
  expect(activeCategory()).toBe("Hosts");
  expect(q("[data-ring-arrived]")).toBeNull();
  expect(tabs()).toHaveLength(1);
  expect(xterms[0].dispose).not.toHaveBeenCalled();
}

it("a successful Add to Ring moves the Host: SSH entry gone, everything under the Ring id, Settings on Ring", async () => {
  await mount();
  await addToRing();
  await settle(20);

  // The service first, then the Terminal is detached (never closed), saved, reconfigured.
  const order = ["add", `keep:${H}`, `detach:${H}`, "save-hosts:0", "configure:"];
  expect(order.map(step => calls.indexOf(step))).toEqual([...order.map(step => calls.indexOf(step))].sort((x, y) => x - y));
  expect(order.every(step => calls.includes(step))).toBe(true);
  expect(settings.get("hosts")).toEqual([]);
  expect(registry.getSnapshot().configs).toEqual([]);
  expect(registry.availableHost(R)?.activeRoute).toBe("relay");

  // The Tab and its xterm are the same; it reattached under the Ring id and types there.
  expect(tabs()).toHaveLength(1);
  expect(xterms).toHaveLength(1);
  expect(xterms[0].dispose).not.toHaveBeenCalled();
  expect(calls.filter(c => c.startsWith("attach:"))).toEqual([`attach:${H}`, `attach:${R}`]);
  await act(async () => { xterms[0].onInput!("x"); });
  expect(calls.filter(c => c.startsWith("input:"))).toEqual([`input:${R}`]);
  expect(calls.some(c => c.startsWith("host_term_close"))).toBe(false);

  // Pinned Projects (folder and order kept), their settings, panel choice, skipped version.
  expect(settings.get("sidebar_layout")).toEqual([{ kind: "folder", id: "f1", name: "Work", collapsed: false, projectPaths: [`host:${R}:/p`, "/local"] }, { kind: "project", path: `host:${R}:/q` }]);
  expect(settings.get("project_paths")).toEqual([`host:${R}:/p`, "/local", `host:${R}:/q`]);
  expect(settings.get("project_icons")).toEqual({ [`host:${R}:/p`]: { customName: "Proj", icon: "PX", folders: [{ id: "sf", name: "Kept", sessionIds: ["raw-1", PI_ENTRY(R)] }] }, "/local": { customName: "Mine" } });
  expect(container.querySelector(`[data-drop-path='host:${R}:/p']`)).not.toBeNull();
  expect(container.querySelector(`[data-drop-path='host:${H}:/p']`)).toBeNull();
  await settle(20);
  expect((settings.get("terminal_right_panels") as { entries: Record<string, string> }).entries).toEqual({ [JSON.stringify(["daemon", R, "box-uuid"])]: "git" });
  expect(settings.get("update_skipped_versions")).toEqual({ [R]: "release:9.9.9" });

  // Settings jumped from Hosts to Ring; the machine's row is there, marked, Online.
  expect(activeCategory()).toBe("Ring");
  const row = q(`[data-ring-member="${KEY}"]`)!;
  expect(row.hasAttribute("data-ring-arrived")).toBe(true);
  expect(row.classList.contains("host-row-arrived")).toBe(true);
  expect(row.textContent).toContain("box");
  expect(row.querySelector(".host-chip")?.textContent).toBe("Online");
  expect(Element.prototype.scrollIntoView).toHaveBeenCalled();
  expect(container.querySelector("[data-ring-host-member]")).toBeNull();
  // The mark goes away by itself.
  await settle(2100);
  expect(q(`[data-ring-member="${KEY}"]`)!.hasAttribute("data-ring-arrived")).toBe(false);
  expect(activeCategory()).toBe("Ring");
}, 15000);

it("the Relay does not report the new member online: not moved, the row says In your Ring", async () => {
  await mount({}, 60);
  await addToRing("unknown");
  await settle(120);
  expect(count("keep:")).toBe(0);
  expect(count("detach:")).toBe(0);
  expectStillSaved();
  expect(q("[data-ring-host-member]")?.textContent).toContain("In your Ring");
  expect(q("[data-ring-add]")).toBeNull();
  // Coming online later moves nothing by itself.
  await available([memberRow("online")]);
  await settle(120);
  expectStillSaved();
});

it("the Host's Daemon cannot be made to start by itself: not moved", async () => {
  await mount({ keep: async () => { throw "failed: the Host has no way to start its Daemon by itself"; } });
  await addToRing();
  await settle(20);
  expect(count("keep:")).toBe(1);
  expect(count("detach:")).toBe(0);
  expectStillSaved();
  expect(q("[data-ring-host-member]")).not.toBeNull();
});

it("a Host with a launch prefix stays an SSH Host", async () => {
  const config = { ...CONFIG, launchPrefixes: { claude: "nix develop -c" } };
  await mount({ config }, 60);
  await addToRing();
  await settle(120);
  expect(count("keep:")).toBe(0);
  expectStillSaved();
  expect(q("[data-ring-host-member]")).not.toBeNull();
});

it.each(["detach", "save"])("the move fails midway (%s): the Host and its Tab stay on SSH", async step => {
  await mount(step === "detach" ? { detach: async () => { throw new Error("controlled Detach failure"); } } : { saveHosts: async () => { throw new Error("controlled save failure"); } });
  await addToRing();
  await settle(20);
  expect(count("detach:")).toBe(1);
  expectStillSaved();
  expect(q("[data-ring-host-member]")).not.toBeNull();
  // The Tab recovered through the unchanged saved Host and takes input.
  expect(calls.filter(c => c.startsWith("attach:"))).toEqual([`attach:${H}`, `attach:${R}`]);
  expect(registry.resolveAlias(R)).toBe(H);
  await act(async () => { xterms[0].onInput!("x"); });
  expect(count("input:")).toBe(1);
  expect(container.textContent).not.toContain("Cannot detach");
});

it("a Hosts change that is still being saved refuses the move", async () => {
  let release!: () => void;
  const held = new Promise<void>(resolve => { release = resolve; });
  let finishAdd!: (value: unknown) => void;
  const renamed = { ...CONFIG, name: "renamed" };
  await mount({
    add: () => new Promise(resolve => { finishAdd = resolve; }),
    configure: async hosts => { if (hosts[0]?.name === "renamed") await held; },
  });
  await act(async () => { q("[data-ring-add]")!.click(); });
  await clickText(".host-row button", "Edit");
  const input = q(".host-form-panel .edit-input") as HTMLInputElement;
  await act(async () => {
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(input, "renamed");
    input.dispatchEvent(new Event("input", { bubbles: true }));
  });
  await clickText(".host-form-panel button", "Save");
  await settle(5);
  expect(calls).toContain("configure:renamed");
  await act(async () => { finishAdd({ names: ["box"], version: 4 }); });
  await becameMember();
  await settle(20);
  expect(count("keep:")).toBe(1);
  expect(container.textContent).toContain("Another Host configuration change is still being saved.");
  expect(count("detach:")).toBe(0);
  await act(async () => { release(); });
  await settle(5);
  expect(settings.get("hosts")).toEqual([renamed]);
  expect(registry.isConfigured(H)).toBe(true);
  expect(layoutKeys()).toContain(`host:${H}:/p`);
  expect(tabs()).toHaveLength(1);
});

it("a Host added to the Ring before this version stays in the SSH list", async () => {
  await mount({ start: memberRow("online"), members: [me, box] });
  await settle(120);
  expect(q("[data-ring-host-member]")?.textContent).toContain("In your Ring");
  expect(q("[data-ring-add]")).toBeNull();
  expect(count("add")).toBe(0);
  expect(count("keep:")).toBe(0);
  expect(count("detach:")).toBe(0);
  expectStillSaved();
});
