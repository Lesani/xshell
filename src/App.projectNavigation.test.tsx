// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";
import { load } from "@tauri-apps/plugin-store";
import type { HostConfig, HostStatus, TerminalInfo } from "./hosts/types";
import type { Group } from "./types";

const xterms = vi.hoisted(() => [] as { dispose: ReturnType<typeof vi.fn> }[]);
vi.mock("@tauri-apps/api/core", async original => ({ ...await original<typeof import("@tauri-apps/api/core")>(), invoke: vi.fn(), Channel: class {} }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
vi.mock("@tauri-apps/api/window", () => ({ getCurrentWindow: () => ({ setAlwaysOnTop: async () => {} }) }));
vi.mock("@tauri-apps/api/app", () => ({ getVersion: async () => "1.5.0" }));
vi.mock("@tauri-apps/plugin-updater", () => ({ check: async () => null }));
vi.mock("@tauri-apps/plugin-store", () => ({ load: vi.fn() }));
vi.mock("./lib/fonts", () => ({ detectMonoFontFamily: () => "monospace", ensureMonoFontsLoaded: async () => {} }));
vi.mock("@xterm/xterm", () => ({ Terminal: class {
  options = {}; unicode = { activeVersion: "" }; parser = { registerOscHandler: vi.fn() };
  cols = 80; rows = 24; dispose = vi.fn();
  constructor() { xterms.push(this); }
  open() {} loadAddon() {} focus() {} write() {} attachCustomKeyEventHandler() {}
} }));
vi.mock("@xterm/addon-fit", () => ({ FitAddon: class { fit() {} } }));
vi.mock("@xterm/addon-web-links", () => ({ WebLinksAddon: class {} }));
vi.mock("@xterm/addon-unicode11", () => ({ Unicode11Addon: class {} }));

const host: HostConfig = { id: "h_aaaaaaaa", name: "Fixture Host", sshTarget: "fixture" };
const otherHost: HostConfig = { id: "h_bbbbbbbb", name: "Other Host", sshTarget: "fixture-other" };
const terminal = (id: string): TerminalInfo => ({ terminal: id, spec: { agent: "claude", cwd: "/projects/same", sessionId: `${id}-session`, shellMode: "claude" }, meta: { title: id, projectName: "Same" }, createdAtMs: 1, pid: 100, exitCode: null });
const panelKey = (id: string, h = host.id) => JSON.stringify(["daemon", h, id]);
const status = (h: string, state: "connected" | "offline" = "connected"): HostStatus => ({ host: h, status: state, phase: null, lastError: null, errorHint: null, daemonVersion: "1.5.0", desktopVersion: "1.5.0", protocol: 1, os: "linux", arch: "x86_64", incompatibleReason: null, nextRetryAt: null, sinceMs: 1, configGeneration: 1 });
let root: Root | undefined, container: HTMLDivElement;
let registry: typeof import("./hosts/registry").registry;
let cache: typeof import("./hosts/cache").cache;
let settings: Map<string, unknown>;
let stores: Map<string, Map<string, unknown>>;
let unexpected: unknown[];
let panelWrites: number;
let fixtureTerminals: TerminalInfo[];
let savedGroups: Group[];
let fixtureHistory: import("./types").SessionInfo[];

beforeEach(() => {
  vi.resetModules();
  xterms.length = 0;
  unexpected = []; panelWrites = 0;
  fixtureTerminals = []; savedGroups = []; fixtureHistory = [];
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  vi.stubGlobal("fetch", vi.fn(async () => ({ ok: true, json: async () => [] })));
  vi.stubGlobal("ResizeObserver", class { observe() {} disconnect() {} });
  vi.stubGlobal("IntersectionObserver", class { observe() {} disconnect() {} });
  // Do not spawn processes in a DOM fixture. The real TerminalTab still constructs xterm,
  // mounts its real transport and runs all panel/explorer effects.
  vi.stubGlobal("requestAnimationFrame", vi.fn(() => 1));
  Element.prototype.scrollIntoView = vi.fn();
  document.elementFromPoint = vi.fn(); document.elementsFromPoint = vi.fn();
});
afterEach(async () => {
  await act(async () => { root?.unmount(); }); root = undefined;
  registry?._dispose(); cache?._reset(); container?.remove();
  vi.unstubAllGlobals(); vi.restoreAllMocks();
});

async function setup(panels: Record<string, unknown>, cachedIds = ["files", "git", "closed", "missing"], savedTabs: unknown[] = [], defaultOn = true) {
  ({ registry } = await import("./hosts/registry"));
  const cacheModule = await import("./hosts/cache"); cache = cacheModule.cache;
  let cached = cacheModule.putTerminals(cacheModule.emptyCache(), host.id, (fixtureTerminals.length ? fixtureTerminals : cachedIds.map(terminal)), 1);
  cached = cacheModule.putTerminals(cached, otherHost.id, [terminal("other")], 1);
  settings = new Map<string, unknown>([
    ["hosts", [host, otherHost]], ["open_tabs", savedTabs], ["open_groups", savedGroups],
    ["project_paths", [`host:${host.id}:/projects/same`, `host:${otherHost.id}:/projects/same`]], ["terminal_right_panels", { version: 1, entries: panels }],
    ["file_explorer_on_start", defaultOn], ["webgl_rendering_enabled", false], ["rate_limit_in_sidebar", false],
    ["rate_limit_in_sidebar_codex", false], ["context_tree_enabled", false], ["terminal_header_stats", false],
  ]);
  stores = new Map([["settings.json", settings], ["hosts-cache.json", new Map<string, unknown>([["cache", cached]])]]);
  vi.mocked(load).mockImplementation(async file => {
    const data = stores.get(file) ?? new Map<string, unknown>(); stores.set(file, data);
    return { get: async (key: string) => data.get(key) ?? null, set: async (key: string, value: unknown) => { if (key === "terminal_right_panels") panelWrites++; data.set(key, value); }, save: async () => {} } as unknown as Awaited<ReturnType<typeof load>>;
  });
  vi.mocked(invoke).mockImplementation(async (command, args) => {
    const a = args as Record<string, unknown> | undefined;
    const method = command === "host_call" ? a?.method : command;
    switch (method) {
      case "local_host_info": return { mode: "in-process", reason: null };
      case "hosts_available": return { sequence: 0, hosts: [] };
      case "hosts_configure": return;
      case "hosts_status": return [host, otherHost].map(h => ({ status: status(h.id, "offline"), terminals: null }));
      case "get_sessions": return fixtureHistory;
      case "list_claude_projects": case "list_pi_projects": case "get_all_recent_sessions": case "open_copies_pending": case "get_git_log": case "list_dir": case "list_project_session_ids": return [];
      case "get_global_rate_limits": return { five_hour_pct: null, seven_day_pct: null, five_hour_resets_at: null, seven_day_resets_at: null };
      case "get_codex_usage": return { present: false, primary: null, secondary: null, daily_sessions: [] };
      case "detect_agent_binary": return { installed: false };
      case "get_git_status": return { is_repo: true, branch: "main", files: [], ahead: 0, behind: 0 };
      case "close_terminal": case "host_term_detach": case "host_term_close": return;
      case "detect_session_branch": return null;
      case "probe_statusline_setup": return { stats_dir_present: false, stats_session_count: 0 };
      case "app_build": return { version: "1.5.0", build: { commit: null, dirty: null, time: null } };
      default: unexpected.push(method); throw new Error(`Unexpected panel fixture IPC: ${String(method)}`);
    }
  });
}
async function mount() {
  const { default: App } = await import("./App");
  container = document.createElement("div"); document.body.append(container); root = createRoot(container);
  await act(async () => { root!.render(<App />); });
}

async function click(el: HTMLElement | null) { expect(el).toBeTruthy(); await act(async () => { el!.click(); }); }
const order = () => [...container.querySelectorAll<HTMLElement>(".tab-item[data-drag-id]")].map(e => e.dataset.dragId!.replace("remote-", ""));
const dragEl = (id: string) => (container.querySelector<HTMLElement>(`[data-drag-id="remote-${id}"]`) ?? container.querySelector<HTMLElement>(`[data-drag-id="${id}"]`))!;
const rect = (left = 300) => ({ left, top: 0, width: 100, height: 30, right: left + 100, bottom: 30, x: left, y: 0, toJSON() {} });
async function pointerDrag(source: string, target: string, x = 390) {
  vi.spyOn(dragEl(target), "getBoundingClientRect").mockReturnValue(rect());
  vi.mocked(document.elementFromPoint).mockReturnValue(dragEl(target));
  await act(async () => { dragEl(source).dispatchEvent(new PointerEvent("pointerdown", { button: 0, bubbles: true, clientX: 10, clientY: 10 })); });
  await act(async () => { window.dispatchEvent(new PointerEvent("pointermove", { clientX: x, clientY: 10 })); });
  await act(async () => { window.dispatchEvent(new PointerEvent("pointerup", { clientX: x, clientY: 10 })); });
}
async function behaviorToggle() {
  await click(container.querySelector('[aria-label="Settings"]'));
  await click([...container.querySelectorAll<HTMLElement>(".settings-nav-item")].find(e => e.textContent?.trim() === "Behavior")!);
  const row = [...container.querySelectorAll<HTMLElement>(".settings-row")].find(e => e.textContent?.includes("Group Tabs by Project"));
  expect(row?.textContent).toContain("Keep each Project's Tabs together. Existing split-pane Groups stay intact.");
  return row!.querySelector<HTMLInputElement>('input[type="checkbox"]')!;
}
function terminalHost(id: string) { const el = container.querySelector<HTMLElement>(`[data-terminal-slot="remote-${id}"] .terminal-host`); expect(el).toBeTruthy(); return el!; }

it.each([undefined, false, "false"])("hydrates strict grouping (%s) before restoration; actual canonical drag/settings/reconcile retain xterms and panel hosts", async value => {
  fixtureTerminals = [terminal("a"), { ...terminal("b"), spec: { ...terminal("b").spec, cwd: "/projects/other" } }, terminal("c")];
  await setup({});
  if (value !== undefined) settings.set("group_tabs_by_project", value);
  // Hold hydration: no restoration or normalization may run before this setting arrives.
  const baseLoad = vi.mocked(load).getMockImplementation()!;
  let release!: () => void;
  const barrier = new Promise<void>(resolve => { release = resolve; });
  vi.mocked(load).mockImplementation(async (...args) => {
    const store = await baseLoad(...args);
    return args[0] !== "settings.json" ? store : { ...store, get: async (key: string) => { if (key === "group_tabs_by_project") await barrier; return store.get(key); } } as typeof store;
  });
  await mount(); expect(xterms).toHaveLength(0);
  await act(async () => { release(); });
  const on = value !== false;
  expect(order()).toEqual(on ? ["a", "c", "b", "other"] : ["a", "b", "c", "other"]);
  const hosts = ["a", "b", "c"].map(terminalHost); const terms = [...xterms];
  await click(dragEl("a"));
  const toggle = await behaviorToggle(); expect(toggle.checked).toBe(on);
  if (!on) await click(toggle);
  expect(settings.get("group_tabs_by_project")).toBe(value === false ? true : value);
  expect(order()).toEqual(["a", "c", "b", "other"]);
  await pointerDrag("a", "b"); expect(order()).toEqual(["b", "a", "c", "other"]);
  expect(container.querySelectorAll(".group-pane")).toHaveLength(0);
  await pointerDrag("a", "c"); expect(order()).toEqual(["b", "c", "a", "other"]);
  const off = await behaviorToggle(); await click(off);
  expect(settings.get("group_tabs_by_project")).toBe(false);
  await pointerDrag("b", "c"); expect(order()).toEqual(["c", "b", "a", "other"]);
  await click(await behaviorToggle()); expect(order()).toEqual(["c", "a", "b", "other"]);
  // New Terminal joins the existing Project block without focus loss or remount.
  await click(dragEl("a"));
  await act(async () => { registry.applyTerminals(host.id, [...fixtureTerminals, terminal("new")]); });
  expect(order()).toEqual(["c", "a", "new", "b", "other"]);
  expect(dragEl("a").classList.contains("active")).toBe(true);
  expect(["a", "b", "c"].map(terminalHost)).toEqual(hosts);
  expect(xterms.slice(0, terms.length)).toEqual(terms); expect(terms.every(t => t.dispose.mock.calls.length === 0)).toBe(true);
  expect(unexpected).toEqual([]);
});

it("actual HomeView live row focuses exact existing Group leaf, never opens/resumes, retains UUID/portal/xterm and offline state", async () => {
  const group: Group = { id: "g", name: "Saved split", layout: { kind: "split", direction: "col", ratio: .5, children: [{ kind: "leaf", tabId: "remote-a" }, { kind: "leaf", tabId: "remote-c" }] } };
  savedGroups = [group];
  fixtureTerminals = [terminal("a"), { ...terminal("c"), meta: { ...terminal("c").meta, customTitle: "Exact named leaf" } }, { ...terminal("unlinked"), spec: { ...terminal("unlinked").spec, sessionId: null } }, { ...terminal("shell"), spec: { cwd: "/projects/same", shellMode: "raw", shellId: "bash" } }];
  await setup({}); await mount();
  const hosts = ["a", "c", "unlinked", "shell"].map(terminalHost); const terms = [...xterms];
  const sidebar = container.querySelector<HTMLElement>('[data-drop-kind="project"]');
  await click(sidebar!);
  expect(container.querySelectorAll("[data-terminal-tab]")).toHaveLength(4);
  expect(container.querySelector('[data-terminal-tab="remote-c"]')?.textContent).toContain("Saved split");
  const before = vi.mocked(invoke).mock.calls.length;
  await click(container.querySelector('[data-terminal-tab="remote-c"]'));
  expect(container.querySelector('.tab-group-item.active')?.textContent).toContain("Saved split");
  expect(container.querySelector('.group-pane-active')?.getAttribute("data-group-leaf")).toBe("remote-c");
  expect(["a", "c", "unlinked", "shell"].map(terminalHost)).toEqual(hosts); expect(xterms).toEqual(terms);
  const operations = vi.mocked(invoke).mock.calls.slice(before).map(([cmd, args]) => cmd === "host_call" ? (args as Record<string, unknown>).method : cmd);
  expect(operations).not.toContain("host_term_open"); expect(operations).not.toContain("spawn_terminal");
  expect(operations).not.toContain("get_sessions");
  await click(sidebar!);
  expect(container.querySelector('[data-terminal-tab="remote-c"]')?.textContent).toContain("Last known");
  await act(async () => { registry.applyStatus(status(host.id)); registry.applyTerminals(host.id, fixtureTerminals); });
  expect(container.querySelector('[data-terminal-tab="remote-c"]')?.textContent).not.toContain("Last known");
  await click(container.querySelector('[data-terminal-tab="remote-unlinked"]'));
  expect(dragEl("unlinked").classList.contains("active")).toBe(true);
  expect(xterms).toEqual(terms); expect(unexpected).toEqual([]);
});

it("real App authoritative closure reveals history in the unchanged saved folder, while same-path other-Host live row stays out", async () => {
  fixtureTerminals = [terminal("a")];
  fixtureHistory = [{ id: "a-session", agent: "claude", title: "Archived conversation", project_name: "Same", project_path: "/projects/same", timestamp: "2026-11-01T00:00:00Z", git_branch: "", message_count: 1, tool_use_count: 0, duration_ms: 0, model: "", total_input_tokens: 0, total_cache_creation_tokens: 0, total_cache_read_tokens: 0, total_output_tokens: 0, daily_cost: {}, daily_tokens: {}, cost_usd: 0, context_tokens: 0, context_limit: 0, is_authoritative_stats: false, claude_version: "", rate_limit_5h_pct: null, rate_limit_7d_pct: null }];
  await setup({});
  const key = `host:${host.id}:/projects/same`;
  const folders = [{ id: "saved", name: "Retained history", collapsed: true, sessionIds: ["a-session"] }];
  settings.set("project_icons", { [key]: { folders } });
  await mount();
  await click(container.querySelector('[data-drop-kind="project"]'));
  expect(container.querySelectorAll("[data-terminal-tab]")).toHaveLength(1);
  expect(container.querySelector('[data-terminal-tab="remote-other"]')).toBeNull();
  expect(container.querySelectorAll(".session-item")).toHaveLength(0);
  expect(container.querySelector(".folder-count")?.textContent).toBe("0");
  await act(async () => { registry.applyTerminals(host.id, []); });
  expect(container.querySelectorAll("[data-terminal-tab]")).toHaveLength(0);
  expect(container.querySelector(".folder-count")?.textContent).toBe("1");
  await click(container.querySelector(".folder-header"));
  expect(container.querySelector(".folder-body")?.textContent).toContain("Archived conversation");
  const saved = settings.get("project_icons") as Record<string, { folders: { sessionIds: string[] }[] }>;
  expect(saved[key].folders[0].sessionIds).toEqual(["a-session"]);
  expect(order()).toEqual(["other"]); expect(unexpected).toEqual([]);
});

it("canonical App mixed/single-Project Groups remain atomic through pointer drops and changed-entry cancellation", async () => {
  const group = (id: string, a: string, b: string): Group => ({ id, name: id, layout: { kind: "split", direction: "col", ratio: .5, children: [{ kind: "leaf", tabId: `remote-${a}` }, { kind: "leaf", tabId: `remote-${b}` }] } });
  savedGroups = [group("mixed", "x", "y"), group("pure", "p1", "p2")];
  fixtureTerminals = [terminal("a"), terminal("x"), { ...terminal("y"), spec: { ...terminal("y").spec, cwd: "/projects/q" } }, terminal("p1"), terminal("p2"), { ...terminal("b"), spec: { ...terminal("b").spec, cwd: "/projects/q" } }];
  await setup({}); await mount();
  expect(order()).toEqual(["a", "pure", "mixed", "b", "other"]);
  const hosts = ["a", "x", "y", "p1", "p2", "b"].map(terminalHost); const terms = [...xterms];
  await pointerDrag("pure", "b"); expect(order()).toEqual(["mixed", "b", "a", "pure", "other"]);
  await pointerDrag("mixed", "other"); expect(order()).toEqual(["b", "a", "pure", "other", "mixed"]);
  expect(container.querySelectorAll(".group-pane")).toHaveLength(4);
  const saved = settings.get("open_groups") as Group[];
  expect(saved).toEqual(savedGroups);
  expect(["a", "x", "y", "p1", "p2", "b"].map(terminalHost)).toEqual(hosts);
  expect(xterms).toEqual(terms);
  vi.spyOn(dragEl("b"), "getBoundingClientRect").mockReturnValue(rect());
  vi.mocked(document.elementFromPoint).mockReturnValue(dragEl("b"));
  await act(async () => { dragEl("a").dispatchEvent(new PointerEvent("pointerdown", { button: 0, bubbles: true, clientX: 10, clientY: 10 })); window.dispatchEvent(new PointerEvent("pointermove", { clientX: 390, clientY: 10 })); });
  await act(async () => { registry.applyTerminals(host.id, [...fixtureTerminals, terminal("new")]); });
  const reconciled = order();
  await act(async () => { window.dispatchEvent(new PointerEvent("pointerup", { clientX: 390, clientY: 10 })); });
  expect(order()).toEqual(reconciled); expect(container.querySelector(".tab-dragging")).toBeNull();
  expect(unexpected).toEqual([]);
});
