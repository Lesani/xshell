// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";
import { load } from "@tauri-apps/plugin-store";
import type { HostConfig, HostStatus, TerminalInfo } from "./hosts/types";
import type { RightPanel, RightPanels } from "./tabs/rightPanel";

const xterms = vi.hoisted(() => [] as { dispose: ReturnType<typeof vi.fn>; onInput?: (data: string) => void; keyHandler?: (e: KeyboardEvent) => boolean; el?: HTMLElement }[]);
const native = vi.hoisted(() => ({ focused: false, minimized: false, changed: null as null | ((event: { payload: boolean }) => void) }));
vi.mock("@tauri-apps/api/core", async original => ({ ...await original<typeof import("@tauri-apps/api/core")>(), invoke: vi.fn(), Channel: class {} }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
vi.mock("@tauri-apps/api/window", () => ({ getCurrentWindow: () => ({
  setAlwaysOnTop: async () => {}, isFocused: async () => native.focused, isMinimized: async () => native.minimized,
  onFocusChanged: async (cb: typeof native.changed) => { native.changed = cb; return () => { native.changed = null; }; },
}) }));
vi.mock("@tauri-apps/api/app", () => ({ getVersion: async () => "1.5.0" }));
vi.mock("@tauri-apps/plugin-updater", () => ({ check: async () => null }));
vi.mock("@tauri-apps/plugin-store", () => ({ load: vi.fn() }));
vi.mock("./lib/fonts", () => ({ detectMonoFontFamily: () => "monospace", ensureMonoFontsLoaded: async () => {} }));
vi.mock("@xterm/xterm", () => ({ Terminal: class {
  options = {}; unicode = { activeVersion: "" }; parser = { registerOscHandler: vi.fn() };
  cols = 80; rows = 24; dispose = vi.fn();
  constructor() { xterms.push(this); }
  el?: HTMLElement; onInput?: (data: string) => void;
  keyHandler?: (e: KeyboardEvent) => boolean;
  open(el: HTMLElement) { this.el = el; } loadAddon() {} focus() {} write() {}
  attachCustomKeyEventHandler(cb: (e: KeyboardEvent) => boolean) { this.keyHandler = cb; }
  // Installed xterm synchronously emits its complete generated bracketed-paste frame.
  paste(text: string) { this.onInput?.(`\u001b[200~${text}\u001b[201~`); }
  onData(cb: (data: string) => void) { this.onInput = cb; return { dispose() {} }; }
  onResize() { return { dispose() {} }; }
} }));
vi.mock("@xterm/addon-fit", () => ({ FitAddon: class { fit() {} } }));
vi.mock("@xterm/addon-web-links", () => ({ WebLinksAddon: class {} }));
vi.mock("@xterm/addon-unicode11", () => ({ Unicode11Addon: class {} }));

const host: HostConfig = { id: "h_aaaaaaaa", name: "Fixture Host", sshTarget: "fixture" };
const otherHost: HostConfig = { id: "h_bbbbbbbb", name: "Other Host", sshTarget: "fixture-other" };
const terminal = (id: string): TerminalInfo => ({ terminal: id, spec: { agent: "claude", cwd: "/projects/same", sessionId: `${id}-session`, shellMode: "claude" }, meta: { title: id, projectName: "Same" }, createdAtMs: 1, pid: 100, exitCode: null });
const panelKey = (id: string, h = host.id) => JSON.stringify(["daemon", h, id]);
const status = (h: string, state: "connected" | "offline" = "connected"): HostStatus => ({ host: h, status: state, phase: null, lastError: null, errorHint: null, daemonVersion: "1.5.0", desktopVersion: "1.5.0", protocol: 1, os: "linux", arch: "x86_64", incompatibleReason: null, nextRetryAt: null, sinceMs: 1, configGeneration: 1, linkGeneration: 2, daemonCapabilities: ["agent.status", "term.completion-attention"] });
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
  unexpected = []; panelWrites = 0; native.focused = false; native.minimized = false;
  vi.spyOn(document, "visibilityState", "get").mockReturnValue("visible");
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
      case "hosts_configure": return;
      case "hosts_status": return [host, otherHost].map(h => ({ status: status(h.id, "offline"), terminals: null }));
      case "list_claude_projects": case "list_pi_projects": case "get_all_recent_sessions": case "get_sessions": case "open_copies_pending": case "get_git_log": case "list_dir": case "list_project_session_ids": return [];
      case "get_global_rate_limits": return { five_hour_pct: null, seven_day_pct: null, five_hour_resets_at: null, seven_day_resets_at: null };
      case "get_codex_usage": return { present: false, primary: null, secondary: null, daily_sessions: [] };
      case "detect_agent_binary": return { installed: false };
      case "get_git_status": return { is_repo: true, branch: "main", files: [], ahead: 0, behind: 0 };
      case "close_terminal": case "host_term_detach": case "host_term_close": case "host_term_resize": case "host_term_input": return;
      case "host_term_attach": return { exitCode: null };
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
const f = "00000000-0000-4000-8000-000000000001", g = "00000000-0000-4000-8000-000000000002";
const seen = () => (settings.get("terminal_completion_seen") as { entries: Record<string, string> } | undefined)?.entries ?? {};
const key = (id: string) => JSON.stringify(["daemon", host.id, id]);
const finished = (id: string, completionId = f): TerminalInfo => ({ ...terminal(id), agentStatus: "finished", statusAtMs: 100, completionAttention: { completionId, inputHandled: false } });
async function publish(list: TerminalInfo[]) { await act(async () => { registry.applyStatus(status(host.id)); registry.applyTerminals(host.id, list); }); }
async function focus(value: boolean, minimized = false) { await act(async () => { native.focused = value; native.minimized = minimized; native.changed?.({ payload: value }); }); }
async function click(el: HTMLElement | null) { expect(el).toBeTruthy(); await act(async () => { el!.click(); }); }
const dragEl = (id: string) => container.querySelector<HTMLElement>(`[data-drag-id="remote-${id}"]`)!;
const unread = (id: string) => !!dragEl(id)?.querySelector(".tab-attention-unread");
async function select(id: string) { await click(dragEl(id)); }
const inputCommands = () => vi.mocked(invoke).mock.calls.filter(([c]) => ["host_term_input", "host_term_update", "host_term_input_handled", "host_term_resize", "spawn_terminal", "host_term_open"].includes(c));

it("real App views only foreground visible leaves, consumes new visible finishes locally and never sends read messages", async () => {
  await setup({}, ["a", "b", "c"]); await mount();
  await publish([finished("a"), finished("b"), finished("c")]);
  await select("a");
  expect(unread("a")).toBe(true); expect(seen()).toEqual({});
  await focus(true);
  expect(unread("a")).toBe(false); expect(seen()[key("a")]).toBe(f);
  expect(unread("b")).toBe(true); expect(seen()[key("b")]).toBeUndefined();
  await publish([finished("a", g), finished("b"), finished("c")]);
  expect(unread("a")).toBe(false); expect(seen()[key("a")]).toBe(g);
  await click(container.querySelector('[aria-label="Settings"]'));
  await publish([finished("a", f), finished("b"), finished("c")]);
  expect(unread("a")).toBe(true); expect(seen()[key("a")]).toBe(g);
  await select("a");
  // Real split pointer seam: B joins A, both are actually rendered, only one focused.
  const rect = (x: number, y: number, width: number, height: number) => ({ x, y, left: x, top: y, right: x + width, bottom: y + height, width, height, toJSON() {} });
  const area = container.querySelector<HTMLElement>(".work-area")!;
  const pane = container.querySelector<HTMLElement>('[data-group-leaf="remote-a"]')!;
  vi.spyOn(area, "getBoundingClientRect").mockReturnValue(rect(0, 100, 1000, 800));
  vi.spyOn(pane, "getBoundingClientRect").mockReturnValue(rect(0, 100, 1000, 800));
  vi.mocked(document.elementsFromPoint).mockReturnValue([pane]); vi.mocked(document.elementFromPoint).mockReturnValue(pane);
  await act(async () => { dragEl("b").dispatchEvent(new PointerEvent("pointerdown", { button: 0, bubbles: true, clientX: 20, clientY: 20 })); });
  await act(async () => { window.dispatchEvent(new PointerEvent("pointermove", { clientX: 950, clientY: 400 })); });
  await act(async () => { window.dispatchEvent(new PointerEvent("pointerup", { clientX: 950, clientY: 400 })); });
  expect(container.querySelectorAll(".group-pane[data-group-leaf]")).toHaveLength(2);
  expect(seen()[key("a")]).toBe(f); expect(seen()[key("b")]).toBe(f); expect(seen()[key("c")]).toBeUndefined();
  await focus(false);
  await publish([finished("a", g), finished("b", g), finished("c")]);
  expect(seen()[key("a")]).toBe(f); expect(seen()[key("b")]).toBe(f);
  await focus(true, true); expect(seen()[key("b")]).toBe(f);
  await focus(true, false); expect(seen()[key("a")]).toBe(g); expect(seen()[key("b")]).toBe(g);
  expect(inputCommands()).toEqual([]); expect(unexpected).toEqual([]);
});

it("loads personal tokens before deriving green, retains offline marks, and prunes authoritative removal only", async () => {
  await setup({}, ["a"]);
  settings.set("terminal_completion_seen", { version: 1, entries: { [key("a")]: f, [key("absent")]: g } });
  const cacheModule = await import("./hosts/cache");
  stores.get("hosts-cache.json")!.set("cache", cacheModule.putTerminals(cacheModule.emptyCache(), host.id, [finished("a")], 1));
  const originalLoad = vi.mocked(load).getMockImplementation()!;
  let release!: () => void; const waiting = new Promise<void>(r => { release = r; });
  vi.mocked(load).mockImplementation(async (...args) => {
    const store = await originalLoad(...args);
    return args[0] === "settings.json" ? { ...store, get: async (name: string) => { if (name === "terminal_completion_seen") await waiting; return store.get(name); } } as typeof store : store;
  });
  await mount(); expect(container.querySelector(".tab-attention-unread")).toBeNull(); expect(xterms).toHaveLength(0);
  await act(async () => { release(); });
  expect(seen()[key("absent")]).toBe(g);
  await publish([finished("a")]); expect(unread("a")).toBe(false); expect(seen()[key("absent")]).toBeUndefined();
  await act(async () => { registry.applyStatus(status(host.id, "offline")); });
  expect(seen()[key("a")]).toBe(f);
  await publish([]); expect(seen()[key("a")]).toBeUndefined();
  expect(inputCommands()).toEqual([]); expect(unexpected).toEqual([]);
});

it("actual mounted xterm callback carries its rendered token unchanged through Tauri and does not recapture at dispatch", async () => {
  await setup({}, ["a"]); await mount(); await publish([finished("a")]);
  vi.spyOn(HTMLElement.prototype, "offsetWidth", "get").mockReturnValue(800);
  vi.spyOn(HTMLElement.prototype, "offsetHeight", "get").mockReturnValue(600);
  // Flush the real TerminalTab's layout wait, not a synthetic input transport wrapper.
  for (let round = 0; round < 4; round++) {
    const callbacks = vi.mocked(requestAnimationFrame).mock.calls.splice(0);
    await act(async () => { callbacks.forEach(([cb]) => cb(0)); });
  }
  const term = xterms.find(t => t.onInput)!; expect(term).toBeTruthy();
  let dispatch!: () => void;
  const baseInvoke = vi.mocked(invoke).getMockImplementation()!;
  const queued: Record<string, unknown>[] = [];
  vi.mocked(invoke).mockImplementation(async (command, args) => {
    if (command === "host_term_input") { await new Promise<void>(r => { dispatch = r; }); queued.push(args as Record<string, unknown>); return; }
    return baseInvoke(command, args);
  });
  await act(async () => { term.onInput!("ö"); });
  await publish([finished("a", g)]);
  await act(async () => { dispatch(); });
  expect(queued).toEqual([{ host: host.id, terminal: "a", data: "ö", handledCompletionId: f, inputTarget: { configGeneration: 1, linkGeneration: 2 } }]);
  await act(async () => { term.onInput!("\u001b[A"); });
  await act(async () => { dispatch(); });
  expect(queued[1]).toMatchObject({ data: "\u001b[A", handledCompletionId: g });
  expect(unexpected).toEqual([]);
});

it("two independent Desktop settings see f independently; shared handling clears B, then g is unread", async () => {
  await setup({}, ["a"]); await mount(); await publish([finished("a")]);
  await select("a"); await focus(true);
  expect(seen()[key("a")]).toBe(f); expect(unread("a")).toBe(false);
  const seenA = JSON.parse(JSON.stringify(settings.get("terminal_completion_seen")));
  expect(inputCommands()).toEqual([]);
  // A separate module/window and personal settings map, observing the same Host record.
  await act(async () => { root!.unmount(); }); root = undefined; container.remove(); registry._dispose(); cache._reset();
  vi.resetModules();
  await setup({}, ["a"]); await mount(); await publish([finished("a")]);
  expect(seen()).toEqual({}); expect(unread("a")).toBe(true);
  expect(seenA.entries[key("a")]).toBe(f);
  await publish([{ ...finished("a"), completionAttention: { completionId: f, inputHandled: true } }]);
  expect(unread("a")).toBe(false); expect(seen()).toEqual({});
  await publish([finished("a", g)]);
  expect(unread("a")).toBe(true); expect(seen()).toEqual({});
  expect(inputCommands()).toEqual([]); expect(unexpected).toEqual([]);
});

it("document-hidden and the actual Quick Actions overlay do not consume a visible completion", async () => {
  await setup({}, ["a"]); await mount(); await publish([finished("a")]); await select("a"); await focus(true);
  expect(seen()[key("a")]).toBe(f);
  vi.spyOn(document, "visibilityState", "get").mockReturnValue("hidden");
  await act(async () => { document.dispatchEvent(new Event("visibilitychange")); });
  await publish([finished("a", g)]); expect(seen()[key("a")]).toBe(f); expect(unread("a")).toBe(true);
  vi.spyOn(document, "visibilityState", "get").mockReturnValue("visible");
  await act(async () => { document.dispatchEvent(new Event("visibilitychange")); });
  expect(seen()[key("a")]).toBe(g);
  await act(async () => { window.dispatchEvent(new KeyboardEvent("keydown", { key: "k", ctrlKey: true, bubbles: true })); });
  expect(container.querySelector(".ts-overlay")).not.toBeNull();
  await publish([finished("a", f)]); expect(seen()[key("a")]).toBe(g); expect(unread("a")).toBe(true);
  await act(async () => { container.querySelector<HTMLInputElement>(".ts-search-input")!.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true })); });
  expect(seen()[key("a")]).toBe(f); expect(unread("a")).toBe(false);
  expect(inputCommands()).toEqual([]); expect(unexpected).toEqual([]);
});

async function startMountedXterm() {
  vi.spyOn(HTMLElement.prototype, "offsetWidth", "get").mockReturnValue(800);
  vi.spyOn(HTMLElement.prototype, "offsetHeight", "get").mockReturnValue(600);
  for (let round = 0; round < 4; round++) {
    const callbacks = vi.mocked(requestAnimationFrame).mock.calls.splice(0);
    await act(async () => { callbacks.forEach(([cb]) => cb(0)); });
  }
  const term = xterms.find(t => t.onInput)!; expect(term).toBeTruthy(); return term;
}

it.each([
  { configGeneration: 1, linkGeneration: 3 },
  { configGeneration: 2, linkGeneration: 4 },
])("delayed clipboard paste keeps its complete input action across $configGeneration/$linkGeneration", async successor => {
  await setup({}, ["a"]); await mount(); await publish([finished("a")]);
  const term = await startMountedXterm();
  let release!: (text: string) => void;
  vi.spyOn(navigator.clipboard, "read").mockResolvedValue([]);
  vi.spyOn(navigator.clipboard, "readText").mockReturnValue(new Promise(r => { release = r; }));
  await act(async () => { term.keyHandler!(new KeyboardEvent("keydown", { key: "v", ctrlKey: true })); });
  expect(invoke).not.toHaveBeenCalledWith("host_term_input", expect.anything());
  await act(async () => {
    registry.applyStatus({ ...status(host.id), ...successor });
    registry.applyTerminals(host.id, [finished("a", g)]);
  });
  await act(async () => { release("ö\nlate clipboard"); });
  expect(vi.mocked(invoke).mock.calls.filter(([c]) => c === "host_term_input")).toEqual([["host_term_input", {
    host: host.id, terminal: "a", data: "\u001b[200~ö\nlate clipboard\u001b[201~",
    handledCompletionId: f, inputTarget: { configGeneration: 1, linkGeneration: 2 },
  }]]);
  // Ordinary input after the asynchronous action still captures its own current target.
  await act(async () => { term.onInput!("next"); });
  expect(vi.mocked(invoke).mock.calls.filter(([c]) => c === "host_term_input")[1][1]).toMatchObject({ handledCompletionId: g, inputTarget: successor });
  expect(unexpected).toEqual([]);
});

it.each(["clipboard image", "image drop"])("delayed %s upload keeps the action from before the upload", async source => {
  await setup({}, ["a"]); await mount(); await publish([finished("a")]);
  const term = await startMountedXterm();
  let release!: (path: string) => void;
  const upload = new Promise<string>(resolve => { release = resolve; });
  let uploadStarted!: () => void;
  const started = new Promise<void>(resolve => { uploadStarted = resolve; });
  const baseInvoke = vi.mocked(invoke).getMockImplementation()!;
  vi.mocked(invoke).mockImplementation(async (command, args) => {
    if (command === "host_call" && (args as { method?: string }).method === "save_dropped_file") { uploadStarted(); return upload; }
    return baseInvoke(command, args);
  });
  if (source === "clipboard image") {
    vi.spyOn(navigator.clipboard, "read").mockResolvedValue([{ types: ["image/png"], getType: async () => new Blob(["image"], { type: "image/png" }) }] as unknown as ClipboardItem[]);
    await act(async () => { term.keyHandler!(new KeyboardEvent("keydown", { key: "v", ctrlKey: true })); await started; });
  } else {
    const event = new Event("drop", { bubbles: true, cancelable: true });
    Object.defineProperty(event, "dataTransfer", { value: { files: [new File(["image"], "image.png", { type: "image/png" })] } });
    await act(async () => { term.el!.dispatchEvent(event); await started; });
  }
  await act(async () => {
    registry.applyStatus({ ...status(host.id), configGeneration: 2, linkGeneration: 4 });
    registry.applyTerminals(host.id, [finished("a", g)]);
  });
  await act(async () => { release("/tmp/late image.png"); });
  expect(vi.mocked(invoke).mock.calls.filter(([c]) => c === "host_term_input")).toEqual([["host_term_input", {
    host: host.id, terminal: "a", data: '\"/tmp/late image.png\" ',
    handledCompletionId: f, inputTarget: { configGeneration: 1, linkGeneration: 2 },
  }]]);
  expect(unexpected).toEqual([]);
});

it("actual Host update confirmation keeps a covered completion unread and unsaved until close", async () => {
  await setup({}, ["a"]); await mount(); await publish([finished("a")]); await select("a"); await focus(true);
  expect(seen()[key("a")]).toBe(f);
  await act(async () => { registry.applyStatus({ ...status(host.id), status: "build-differs", updateAction: "replace" }); });
  await click(container.querySelector<HTMLButtonElement>(".host-update-banner .host-update-action button"));
  expect(container.querySelector(".settings-overlay .host-confirm-panel")).not.toBeNull();
  await act(async () => { registry.applyTerminals(host.id, [finished("a", g)]); });
  expect(unread("a")).toBe(true);
  expect(seen()[key("a")]).toBe(f);
  await click(container.querySelector<HTMLButtonElement>(".host-confirm-panel .btn-ghost"));
  expect(container.querySelector(".host-confirm-panel")).toBeNull();
  expect(unread("a")).toBe(false);
  expect(seen()[key("a")]).toBe(g);
  expect(inputCommands()).toEqual([]); expect(unexpected).toEqual([]);
});
