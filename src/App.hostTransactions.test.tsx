// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, expect, it, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";
import { load } from "@tauri-apps/plugin-store";
import type { HostConfig, HostStatus, TerminalInfo } from "./hosts/types";

const xterms = vi.hoisted(() => [] as { dispose: ReturnType<typeof vi.fn>; write: ReturnType<typeof vi.fn>; onInput?: (data: string) => void; keyHandler?: (e: KeyboardEvent) => boolean; el?: HTMLElement }[]);
const native = vi.hoisted(() => ({ focused: false, minimized: false, changed: null as null | ((event: { payload: boolean }) => void) }));
vi.mock("@tauri-apps/api/core", async original => ({ ...await original<typeof import("@tauri-apps/api/core")>(), invoke: vi.fn(), Channel: class {} }));
const events = vi.hoisted(() => new Map<string, (e: { payload: unknown }) => void>());
vi.mock("@tauri-apps/api/event", () => ({ listen: async (name: string, cb: (e: { payload: unknown }) => void) => { events.set(name, cb); return () => events.delete(name); } }));
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
  open(el: HTMLElement) { this.el = el; } loadAddon() {} focus() {} write = vi.fn();
  attachCustomKeyEventHandler(cb: (e: KeyboardEvent) => boolean) { this.keyHandler = cb; }
  // Installed xterm synchronously emits its complete generated bracketed-paste frame.
  paste(text: string) { this.onInput?.(`\u001b[200~${text}\u001b[201~`); }
  onData(cb: (data: string) => void) { this.onInput = cb; return { dispose() {} }; }
  onResize() { return { dispose() {} }; }
} }));
vi.mock("@xterm/addon-fit", () => ({ FitAddon: class { fit() {} } }));
vi.mock("@xterm/addon-web-links", () => ({ WebLinksAddon: class {} }));
vi.mock("@xterm/addon-unicode11", () => ({ Unicode11Addon: class {} }));

import type { AvailableHost, HostSnapshot } from "./hosts/types";

const A = "h_aaaaaaaa", B = "h_bbbbbbbb", RA = `r_${"a".repeat(64)}`, RB = `r_${"b".repeat(64)}`;
let dispose: (() => Promise<void>) | undefined;
afterEach(async () => { await dispose?.(); dispose = undefined; vi.unstubAllGlobals(); });

async function setup(alias = false) {
  vi.resetModules(); events.clear(); xterms.length = 0; vi.mocked(invoke).mockReset();
  const { default: App } = await import("./App");
  const { registry } = await import("./hosts/registry"), { cache } = await import("./hosts/cache");
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  vi.stubGlobal("fetch", vi.fn(async () => ({ ok: true, json: async () => [] })));
  vi.stubGlobal("ResizeObserver", class { observe() {} disconnect() {} });
  vi.stubGlobal("IntersectionObserver", class { observe() {} disconnect() {} });
  Element.prototype.scrollIntoView = vi.fn();
  const frames: FrameRequestCallback[] = [];
  vi.stubGlobal("requestAnimationFrame", (cb: FrameRequestCallback) => { frames.push(cb); return frames.length; });
  const info = (terminal: string): TerminalInfo => ({ terminal, spec: { cwd: "/p", shellMode: "raw", shellId: "bash" }, meta: { title: terminal, projectName: "Project" }, createdAtMs: 1, pid: 42, exitCode: null });
  const status = (host: string): HostStatus => ({ host, status: "connected", phase: null, lastError: null, errorHint: null, daemonVersion: "1.5.0", desktopVersion: "1.5.0", protocol: 1, os: "linux", arch: "x86_64", incompatibleReason: null, nextRetryAt: null, sinceMs: 1, configGeneration: 1, linkGeneration: 1 });
  let configs: HostConfig[] = [{ id: A, name: "Host A", sshTarget: "A1" }, { id: B, name: "Host B", sshTarget: "B1" }];
  const row = (id: string, identity: string, key: string): AvailableHost => ({ id, aliases: [id, identity], name: id === A ? "Host A" : "Host B", color: null, daemonKey: key, activeRoute: "ssh", routes: ["ssh", "relay"], reachability: { kind: "online" } });
  let rows = alias ? [{ ...row(A, RA, "A-key"), aliases: [A], daemonKey: null }, { ...row(RA, RA, "A-key"), aliases: [RA], activeRoute: "relay" as const, routes: ["relay" as const] }] : [row(A, RA, "A-key"), row(B, RB, "B-key")];
  let snapshots: HostSnapshot[] = alias ? [{ status: status(A), terminals: [info("live-a")] }, { status: status(RA), terminals: [info("deferred-alias")] }] : [{ status: status(A), terminals: [info("live-a")] }, { status: status(B), terminals: [info("live-b")] }];
  let sequence = 1;
  let release!: () => void;
  const detach = new Promise<void>(resolve => { release = resolve; });
  const commits: HostConfig[][] = [], channels = new Map<string, { onmessage: (data: Uint8Array) => void }>();
  const settings = new Map<string, unknown>([["hosts", configs], ["file_explorer_on_start", false], ["webgl_rendering_enabled", false], ["terminal_header_stats", false], ["eager_init_tabs", false], ["rate_limit_in_sidebar", false], ["rate_limit_in_sidebar_codex", false], ["context_tree_enabled", false]]);
  const stores = new Map<string, Map<string, unknown>>([["settings.json", settings]]);
  vi.mocked(load).mockImplementation(async file => {
    const data = stores.get(file) ?? new Map<string, unknown>(); stores.set(file, data);
    return { get: async (key: string) => data.get(key) ?? null, set: async (key: string, value: unknown) => { data.set(key, value); }, save: async () => {} } as unknown as Awaited<ReturnType<typeof load>>;
  });
  vi.mocked(invoke).mockImplementation(async (command, args) => {
    const a = args as Record<string, unknown> | undefined, method = command === "host_call" ? a?.method : command;
    switch (method) {
      case "local_host_info": return { mode: "in-process", reason: null };
      case "hosts_available": return { sequence, hosts: rows };
      case "hosts_status": return snapshots;
      case "hosts_configure": {
        const next = a?.hosts as HostConfig[];
        if (JSON.stringify(next) === JSON.stringify(configs)) return;
        configs = next; commits.push(next.map(c => ({ ...c })));
        return;
      }
      case "host_term_attach": channels.set(`${String(a?.host)}:${String(a?.terminal)}`, a?.onData as { onmessage: (data: Uint8Array) => void }); return { exitCode: null };
      case "host_term_detach": if (a?.host === A) { await detach; if (alias) throw new Error("A Detach rejected"); } return;
      case "host_term_input": case "host_term_resize": return;
      case "list_ssh_hosts": case "list_claude_projects": case "list_pi_projects": case "get_all_recent_sessions": case "get_sessions": case "open_copies_pending": case "get_git_log": case "list_dir": case "list_project_session_ids": return [];
      case "detect_session_branch": return null;
      case "get_git_status": return { is_repo: false, files: [], branch: null };
      case "get_global_rate_limits": return {};
      case "get_codex_usage": return { present: false, primary: null, secondary: null, daily_sessions: [] };
      case "detect_agent_binary": return { installed: false };
      case "probe_statusline_setup": return { stats_dir_present: false, stats_session_count: 0 };
      case "app_build": return { version: "1.5.0", build: null };
      default: throw new Error(`Unexpected transaction fixture command ${String(method)}`);
    }
  });
  const container = document.createElement("div"); document.body.append(container); const root = createRoot(container);
  dispose = async () => { await act(async () => { release(); }); await act(async () => root.unmount()); container.remove(); registry._dispose(); cache._reset(); };
  await act(async () => root.render(<App />));
  expect(xterms).toHaveLength(2);
  const visible = (index: number) => { Object.defineProperty(xterms[index].el!, "offsetWidth", { configurable: true, value: 800 }); Object.defineProperty(xterms[index].el!, "offsetHeight", { configurable: true, value: 600 }); };
  const frame = async () => { await act(async () => { frames.splice(0).forEach(cb => cb(0)); }); };
  visible(0); if (!alias) visible(1);
  await frame(); await frame();
  expect(channels.has(`${A}:live-a`)).toBe(true);
  if (alias) {
    rows = [row(A, RA, "A-key")]; snapshots = [{ status: status(A), terminals: [info("live-a"), info("deferred-alias")] }]; sequence++;
    await act(async () => { events.get("hosts:available")!({ payload: { sequence, hosts: rows } }); });
  }
  const click = async (selector: string, text?: string) => { const element = text ? Array.from(container.querySelectorAll<HTMLElement>(selector)).find(el => el.textContent?.trim() === text) : container.querySelector<HTMLElement>(selector); expect(element, `${selector} ${text ?? ""}`).toBeTruthy(); await act(async () => element!.click()); };
  await click('[aria-label="Settings"]'); await click(".settings-nav-item", "Hosts");
  const hostButton = async (name: string, text: string) => {
    const hostRow = Array.from(container.querySelectorAll<HTMLElement>(".host-row")).find(el => el.querySelector(".host-row-name")?.textContent === name);
    const button = Array.from(hostRow?.querySelectorAll("button") ?? []).find(el => el.textContent?.trim() === text); expect(button).toBeTruthy(); await act(async () => button!.click());
  };
  const edit = async (name: string, target: string) => {
    await hostButton(name, "Edit");
    const input = container.querySelector<HTMLInputElement>(".host-form-mono")!;
    await act(async () => { Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(input, target); input.dispatchEvent(new Event("input", { bubbles: true })); });
    await click(".host-form-panel button", "Save");
  };
  const remove = async (name: string) => { await hostButton(name, "Remove"); await click(".host-confirm-panel button", "Remove host"); };
  return { container, registry, settings, commits, channels, visible, frame, edit, remove, release: async () => { await act(async () => release()); } };
}

it("deferred original-r startup resumes after live-h Detach abort with unchanged status and the same UUID/xterm", async () => {
  const f = await setup(true), aliasTerm = xterms[1];
  expect(f.channels.has(`${RA}:deferred-alias`)).toBe(false);
  await f.edit("Host A", "A2");
  expect(f.registry.isRetargeting(RA)).toBe(true);
  f.visible(1); await f.frame(); await f.frame();
  expect(f.channels.has(`${RA}:deferred-alias`)).toBe(false);
  await f.release();
  expect(f.commits).toEqual([]); expect(f.registry.getStatus(RA)?.configGeneration).toBe(1);
  expect(f.channels.has(`${RA}:deferred-alias`)).toBe(true);
  await act(async () => { f.channels.get(`${RA}:deferred-alias`)!.onmessage(new Uint8Array([75])); });
  expect(aliasTerm.write).toHaveBeenCalled();
  aliasTerm.onInput!("original-identity-only");
  expect(vi.mocked(invoke).mock.calls.filter(([cmd]) => cmd === "host_term_input").slice(-1)[0]?.[1]).toMatchObject({ host: RA, terminal: "deferred-alias" });
  expect(xterms).toHaveLength(2); expect(aliasTerm.dispose).not.toHaveBeenCalled();
  expect(vi.mocked(invoke).mock.calls.filter(([cmd]) => ["host_term_open", "host_term_close", "host_term_relaunch"].includes(cmd))).toEqual([]);
});

it.each(["edit-B", "remove-B", "remove-A"])("overlapping full-list Settings mutation is rejected safely, unrelated Terminal use continues, and retry survives (%s)", async action => {
  const f = await setup();
  await f.edit("Host A", "A2"); expect(f.registry.isRetargeting(A)).toBe(true);
  if (action === "edit-B") await f.edit("Host B", "B2"); else await f.remove(action === "remove-A" ? "Host A" : "Host B");
  expect.soft(f.commits).toEqual([]);
  expect.soft(f.registry.config(B)?.sshTarget).toBe("B1");
  expect.soft(f.registry.isConfigured(A)).toBe(true);
  expect.soft(f.registry.isConfigured(B)).toBe(true);
  expect(f.container.textContent).toContain("configuration change");
  xterms[1].onInput!("unrelated-B-still-usable");
  expect(vi.mocked(invoke).mock.calls.filter(([cmd]) => cmd === "host_term_input").slice(-1)[0]?.[1]).toMatchObject({ host: RB, terminal: "live-b" });
  await f.release();
  expect(f.registry.config(A)?.sshTarget).toBe("A2");
  if (action === "edit-B") {
    await f.edit("Host B", "B2"); expect(f.registry.config(B)?.sshTarget).toBe("B2");
  } else {
    await f.remove(action === "remove-A" ? "Host A" : "Host B"); expect(f.registry.isConfigured(action === "remove-A" ? A : B)).toBe(false);
  }
  expect(f.settings.get("hosts")).toEqual(f.registry.getSnapshot().configs);
  expect(vi.mocked(invoke).mock.calls.filter(([cmd]) => ["host_term_open", "host_term_close", "host_term_relaunch"].includes(cmd))).toEqual([]);
});
