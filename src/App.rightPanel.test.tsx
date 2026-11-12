// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";
import { load } from "@tauri-apps/plugin-store";
import type { HostConfig, HostStatus, TerminalInfo } from "./hosts/types";
import type { RightPanel, RightPanels } from "./tabs/rightPanel";

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

beforeEach(() => {
  vi.resetModules();
  xterms.length = 0;
  unexpected = []; panelWrites = 0;
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
  let cached = cacheModule.putTerminals(cacheModule.emptyCache(), host.id, cachedIds.map(terminal), 1);
  cached = cacheModule.putTerminals(cached, otherHost.id, [terminal("other")], 1);
  settings = new Map<string, unknown>([
    ["hosts", [host, otherHost]], ["open_tabs", savedTabs], ["terminal_right_panels", { version: 1, entries: panels }],
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
      case "list_claude_projects": case "list_pi_projects": case "get_all_recent_sessions": case "get_sessions": case "open_copies_pending": case "get_git_log": case "list_dir": case "list_project_session_ids": return [];
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
const panels = () => (settings.get("terminal_right_panels") as { entries: RightPanels }).entries;
const terminalHost = (id: string) => container.querySelector<HTMLElement>(`[data-terminal-slot="remote-${id}"] .terminal-host`) ?? [...container.querySelectorAll<HTMLElement>(".terminal-host")].find(el => el.querySelector(".terminal-header-label")?.textContent === `Same·${id}`)!;
const panelButton = (id: string, panel: "files" | "git") => terminalHost(id).querySelector<HTMLButtonElement>(`[aria-label="Toggle ${panel === "files" ? "file explorer" : "git panel"}"]`)!;
function expectPanel(id: string, panel: RightPanel) {
  expect(panelButton(id, "files").classList.contains("active")).toBe(panel === "files");
  expect(panelButton(id, "git").classList.contains("active")).toBe(panel === "git");
}
async function click(el: HTMLElement | null) { expect(el).toBeTruthy(); await act(async () => { el!.click(); }); }
async function select(id: string) { await click(container.querySelector(`[data-drag-id="remote-${id}"]`)); }

it("hydrates before real TerminalTabs mount; keeps independent choices, lazy explorer state, portal/xterm identity through switches, split moves, reorder and restart", async () => {
  await setup({ [panelKey("files")]: "files", [panelKey("git")]: "git", [panelKey("closed")]: "closed", [panelKey("missing")]: "invalid" });
  const originalLoad = vi.mocked(load).getMockImplementation()!;
  let release!: () => void;
  const hydration = new Promise<void>(resolve => { release = resolve; });
  vi.mocked(load).mockImplementation(async (...args) => {
    const store = await originalLoad(...args);
    if (args[0] !== "settings.json") return store;
    return { ...store, get: async (key: string) => { if (key === "terminal_right_panels") await hydration; return store.get(key); } } as typeof store;
  });
  await mount();
  expect(xterms).toHaveLength(0);
  expect(container.querySelector(".terminal-host")).toBeNull();
  await act(async () => { release(); });
  expect(xterms).toHaveLength(5);
  for (const [id, panel] of [["files", "files"], ["git", "git"], ["closed", "closed"], ["missing", "files"], ["other", "files"]] as const) expectPanel(id, panel);
  expect(panels()[panelKey("missing")]).toBe("files");
  expect(terminalHost("git").querySelector(".file-panel-header")).toBeNull();
  expect(terminalHost("closed").querySelector(".file-panel-header")).toBeNull();
  const hosts = ["files", "git", "closed"].map(terminalHost);
  const explorer = hosts[0].querySelector<HTMLElement>(".file-panel-header")!;
  // Browse to parent: actual explorer substate must survive hiding and portal reparenting.
  await click(explorer.querySelector("button"));
  expect(explorer.querySelector(".file-panel-title")?.textContent).toBe("projects");
  await click(panelButton("files", "git")); expectPanel("files", "git");
  expect(explorer.isConnected).toBe(true);
  expect(explorer.parentElement!.style.display).toBe("none");
  await click(panelButton("files", "files")); expectPanel("files", "files");
  expect(terminalHost("files").querySelector(".file-panel-header")).toBe(explorer);
  expect(explorer.querySelector(".file-panel-title")?.textContent).toBe("projects");
  await click([...terminalHost("git").querySelectorAll<HTMLButtonElement>(".git-tab")].find(b => b.textContent === "History")!);
  await click(panelButton("git", "git")); expectPanel("git", "closed");
  await click(panelButton("git", "git")); expectPanel("git", "git");
  expect(terminalHost("git").querySelector(".git-tab.active")?.textContent).toBe("History");
  const writesBeforeNavigation = panelWrites;
  await select("files"); await select("git"); await select("closed"); await select("files");
  expect(panelWrites).toBe(writesBeforeNavigation);
  expect(["files", "git", "closed"].map(terminalHost)).toEqual(hosts);

  // Real pointer handlers, with only layout/hit testing supplied by this DOM fixture.
  const area = container.querySelector<HTMLElement>(".work-area")!;
  const rect = (x: number, y: number, width: number, height: number) => ({ x, y, left: x, top: y, right: x + width, bottom: y + height, width, height, toJSON() {} });
  vi.spyOn(area, "getBoundingClientRect").mockReturnValue(rect(0, 100, 1000, 800));
  const pane = container.querySelector<HTMLElement>('[data-group-leaf="remote-files"]')!;
  vi.spyOn(pane, "getBoundingClientRect").mockReturnValue(rect(0, 100, 1000, 800));
  vi.spyOn(document, "elementsFromPoint").mockReturnValue([pane]);
  vi.spyOn(document, "elementFromPoint").mockReturnValue(pane);
  await act(async () => { container.querySelector('[data-drag-id="remote-missing"]')!.dispatchEvent(new PointerEvent("pointerdown", { button: 0, bubbles: true, clientX: 20, clientY: 20 })); });
  await act(async () => { window.dispatchEvent(new PointerEvent("pointermove", { clientX: 950, clientY: 400 })); });
  await act(async () => { window.dispatchEvent(new PointerEvent("pointerup", { clientX: 950, clientY: 400 })); });
  expect(container.querySelectorAll(".group-pane[data-group-leaf]")).toHaveLength(2);
  for (const [id, panel] of [["files", "files"], ["git", "git"], ["closed", "closed"]] as const) expectPanel(id, panel);
  expect(["files", "git", "closed"].map(terminalHost)).toEqual(hosts);
  // Closing the other leaf dissolves the group and reparents the surviving Files
  // Terminal back to a standalone slot; no new ungroup operation is introduced here.
  await click(container.querySelector<HTMLButtonElement>('[data-group-leaf="remote-missing"] .group-pane-close'));
  expect(container.querySelectorAll(".group-pane[data-group-leaf]")).toHaveLength(0);
  const target = container.querySelector<HTMLElement>('[data-drag-id="remote-closed"]')!;
  vi.spyOn(target, "getBoundingClientRect").mockReturnValue(rect(300, 0, 100, 30));
  vi.mocked(document.elementFromPoint).mockReturnValue(target);
  vi.mocked(document.elementsFromPoint).mockReturnValue([]);
  await act(async () => { container.querySelector('[data-drag-id="remote-files"]')!.dispatchEvent(new PointerEvent("pointerdown", { button: 0, bubbles: true, clientX: 10, clientY: 10 })); });
  await act(async () => { window.dispatchEvent(new PointerEvent("pointermove", { clientX: 390, clientY: 10 })); });
  await act(async () => { window.dispatchEvent(new PointerEvent("pointerup", { clientX: 390, clientY: 10 })); });
  expect([...container.querySelectorAll<HTMLElement>("[data-drag-id]")].map(e => e.dataset.dragId).indexOf("remote-files")).toBeGreaterThan([...container.querySelectorAll<HTMLElement>("[data-drag-id]")].map(e => e.dataset.dragId).indexOf("remote-closed"));
  expect(["files", "git", "closed"].map(terminalHost)).toEqual(hosts);
  expect(xterms).toHaveLength(5); expect(xterms.filter(t => t.dispose.mock.calls.length > 0)).toHaveLength(1);
  expect(xterms[0].dispose).not.toHaveBeenCalled(); expect(xterms[1].dispose).not.toHaveBeenCalled(); expect(xterms[2].dispose).not.toHaveBeenCalled();
  expect(explorer.querySelector(".file-panel-title")?.textContent).toBe("projects");
  // Read-only live-list metadata updates do not reset presentation or remount.
  await act(async () => { registry.applyTerminals(host.id, ["files", "git", "closed"].map(id => ({ ...terminal(id), meta: { title: id, projectName: "Same", customTitle: "Renamed" } }))); });
  expect(xterms).toHaveLength(5);
  expect(panels()[panelKey("closed")]).toBe("closed");
  expect(unexpected).toEqual([]);

  // New window/module lifetime, same serialized settings/cache. Global default now off.
  await act(async () => { root!.unmount(); }); root = undefined; container.remove();
  registry._dispose(); cache._reset();
  settings.set("file_explorer_on_start", false);
  settings.set("terminal_right_panels", JSON.parse(JSON.stringify(settings.get("terminal_right_panels"))));
  const cacheModule = await import("./hosts/cache");
  stores.get("hosts-cache.json")!.set("cache", cacheModule.putTerminals(stores.get("hosts-cache.json")!.get("cache") as import("./hosts/cache").CacheState, host.id, ["files", "git", "closed"].map(terminal), 2));
  vi.resetModules(); ({ registry } = await import("./hosts/registry")); ({ cache } = await import("./hosts/cache"));
  vi.mocked(load).mockImplementation(originalLoad);
  await mount();
  for (const [id, panel] of [["files", "files"], ["git", "git"], ["closed", "closed"]] as const) expectPanel(id, panel);
  expect(unexpected).toEqual([]);
});

it("retains offline/cache-absent preferences, then prunes only after hydrated authoritative lists and explicit Host removal", async () => {
  await setup({ [panelKey("files")]: "closed", [panelKey("absent")]: "git", [panelKey("other", otherHost.id)]: "git" }, ["files"]);
  await mount();
  expect(panels()[panelKey("absent")]).toBe("git");
  expectPanel("files", "closed");
  await act(async () => { registry.applyStatus(status(host.id, "offline")); });
  expect(panels()[panelKey("absent")]).toBe("git");
  await act(async () => { registry.applyTerminals(host.id, [terminal("files")]); });
  expect(panels()[panelKey("absent")]).toBeUndefined();
  expect(panels()[panelKey("files")]).toBe("closed");
  await act(async () => { registry.applyTerminals(host.id, []); });
  expect(panels()[panelKey("files")]).toBeUndefined();
  expect(panels()[panelKey("other", otherHost.id)]).toBe("git");
  await click(container.querySelector('[aria-label="Settings"]'));
  await click([...container.querySelectorAll<HTMLElement>(".settings-nav-item")].find(b => b.textContent?.trim() === "Hosts")!);
  await click(container.querySelectorAll<HTMLButtonElement>(".host-row-remove")[1]);
  await click(container.querySelector<HTMLButtonElement>(".host-confirm-panel .btn-primary"));
  expect(panels()[panelKey("other", otherHost.id)]).toBeUndefined();
  expect(unexpected).toEqual([]);
});

it.each([false, true])("transfers an adopted UUID before pruning; target existing preference wins (%s)", async targetExists => {
  await setup({ [panelKey("source")]: "git", ...(targetExists ? { [panelKey("owner")]: "closed" } : {}) }, ["source"]);
  await mount();
  const { remoteTerminals, pendingOpens } = await import("./hosts/terminalTransport");
  pendingOpens.set("source", { host: host.id, state: "opening", spec: terminal("source").spec, meta: {} });
  const baseInvoke = vi.mocked(invoke).getMockImplementation()!;
  vi.mocked(invoke).mockImplementation(async (...args) => args[0] === "host_term_open" ? { terminal: "owner", existed: true, pid: 100 } : baseInvoke(...args));
  const gen = remoteTerminals.mount(host.id, "source", { data() {}, exit() {} });
  await act(async () => { await remoteTerminals.start(host.id, "source", gen, { cols: 80, rows: 24, shellMode: "claude", shellCommand: null, shellId: null, fullscreenRendering: true, forceSyncOutput: true }); });
  expect(panels()[panelKey("source")]).toBeUndefined();
  expect(panels()[panelKey("owner")]).toBe(targetExists ? "closed" : "git");
  // A pre-owner list must not erase the transfer, even when no source Tab remains.
  await act(async () => { registry.applyTerminals(host.id, []); });
  expect(panels()[panelKey("owner")]).toBe(targetExists ? "closed" : "git");
  await act(async () => { registry.applyTerminals(host.id, [terminal("owner")]); });
  expectPanel("owner", targetExists ? "closed" : "git");
  await act(async () => { registry.applyTerminals(host.id, []); });
  expect(panels()[panelKey("owner")]).toBeUndefined();
  expect(unexpected).toEqual([]);
});

it("uses the in-process Tab-id fallback and establishes missing defaults once when the global setting changes", async () => {
  const saved = ["saved", "legacy"].map(id => ({ id, type: "terminal", title: id, sessionId: `${id}-session`, projectName: "Same", projectPath: "/projects/same", agent: "codex", shellMode: "claude" }));
  const fallbackKey = JSON.stringify(["in-process", "saved"]);
  const legacyKey = JSON.stringify(["in-process", "legacy"]);
  await setup({ [fallbackKey]: "git" }, [], saved, false);
  await mount();
  const findLocal = (id: string) => container.querySelector<HTMLElement>(`[data-terminal-slot="${id}"] .terminal-host`)!;
  const local = findLocal("saved");
  expect(local.querySelector('[aria-label="Toggle git panel"]')?.classList.contains("active")).toBe(true);
  expect(panels()[fallbackKey]).toBe("git"); expect(panels()[legacyKey]).toBe("closed");
  await click(container.querySelector('[aria-label="Settings"]'));
  await click([...container.querySelectorAll<HTMLElement>(".settings-nav-item")].find(b => b.textContent?.trim() === "Terminal")!);
  const row = [...container.querySelectorAll<HTMLElement>(".settings-row")].find(r => r.querySelector(".settings-row-title")?.textContent === "Open file explorer on start")!;
  const writesBeforeDefaultChange = panelWrites;
  await click(row.querySelector("input"));
  expect(panelWrites).toBe(writesBeforeDefaultChange);
  expect(settings.get("file_explorer_on_start")).toBe(true);
  expect(panels()[legacyKey]).toBe("closed"); expect(panels()[fallbackKey]).toBe("git");
  await act(async () => { registry.applyTerminals(host.id, [terminal("new-agent")]); });
  expectPanel("new-agent", "files");
  expect(panels()[panelKey("new-agent")]).toBe("files");
  expect(findLocal("saved")).toBe(local);
  expect(unexpected).toEqual([]);
});

it("does not shield an already listed adoption target from a later authoritative removal", async () => {
  await setup({ [panelKey("source")]: "git", [panelKey("owner")]: "closed" }, ["source", "owner"]);
  await mount();
  await act(async () => { registry.applyTerminals(host.id, [terminal("source"), terminal("owner")]); });
  const { remoteTerminals, pendingOpens } = await import("./hosts/terminalTransport");
  pendingOpens.set("source", { host: host.id, state: "opening", spec: terminal("source").spec, meta: {} });
  const baseInvoke = vi.mocked(invoke).getMockImplementation()!;
  vi.mocked(invoke).mockImplementation(async (...args) => args[0] === "host_term_open" ? { terminal: "owner", existed: true, pid: 100 } : baseInvoke(...args));
  const gen = remoteTerminals.mount(host.id, "source", { data() {}, exit() {} });
  await act(async () => { await remoteTerminals.start(host.id, "source", gen, { cols: 80, rows: 24, shellMode: "claude", shellCommand: null, shellId: null, fullscreenRendering: true, forceSyncOutput: true }); });
  expect(panels()[panelKey("owner")]).toBe("closed");
  await act(async () => { registry.applyTerminals(host.id, []); });
  expect(panels()[panelKey("owner")]).toBeUndefined();
  expect(unexpected).toEqual([]);
});
