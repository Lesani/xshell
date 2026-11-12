// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { expect, it, vi } from "vitest";
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

it.each(["success", "admission", "abort", "partial-abort", "offline-abort"])("actual Settings live retarget preserves original identity without K2 escape (%s)", async mode => {
  vi.resetModules(); events.clear(); xterms.length = 0; vi.mocked(invoke).mockReset();
  const { default: App } = await import("./App");
  const { registry } = await import("./hosts/registry"), { cache } = await import("./hosts/cache");
  const { captureTerminalInput, writeTerminalInput, pendingOpens } = await import("./hosts/terminalTransport");
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  vi.stubGlobal("fetch", vi.fn(async () => ({ ok: true, json: async () => [] })));
  vi.stubGlobal("ResizeObserver", class { observe() {} disconnect() {} });
  vi.stubGlobal("IntersectionObserver", class { observe() {} disconnect() {} });
  Element.prototype.scrollIntoView = vi.fn();
  // Run one real TerminalTab startup frame; subsequent visual fit frames need no pixels.
  const frames: FrameRequestCallback[] = [];
  vi.stubGlobal("requestAnimationFrame", (cb: FrameRequestCallback) => { frames.push(cb); return frames.length; });
  const h = "h_aaaaaaaa", r = `r_${"a".repeat(64)}`;
  const oldConfig: HostConfig = { id: h, name: "Saved route", sshTarget: "K" };
  const info: TerminalInfo = { terminal: "original-uuid", spec: { cwd: "/p", shellMode: "raw", shellId: "bash" }, meta: { title: "Original Terminal", projectName: "Project" }, createdAtMs: 1, pid: 42, exitCode: null };
  const status = (host: string, configGeneration: number): HostStatus => ({ host, status: "connected", phase: null, lastError: null, errorHint: null, daemonVersion: "1.5.0", desktopVersion: "1.5.0", protocol: 1, os: "linux", arch: "x86_64", incompatibleReason: null, nextRetryAt: null, sinceMs: 1, configGeneration, linkGeneration: 2 });
  const descriptor: AvailableHost = { id: h, aliases: [h, r], name: "K", color: null, daemonKey: "K-key", activeRoute: "ssh", routes: ["ssh", "relay"], reachability: { kind: "online" } };
  const infos = mode === "partial-abort" ? [info, { ...info, terminal: "second-uuid" }] : [info];
  let snapshots: HostSnapshot[] = [{ status: status(h, 1), terminals: infos }];
  let releaseDetach!: () => void;
  const detaching = new Promise<void>(resolve => { releaseDetach = resolve; });
  const order: string[] = [];
  let oldData: { onmessage: (data: Uint8Array) => void } | undefined;
  let newData: { onmessage: (data: Uint8Array) => void } | undefined;
  const oldChannels: NonNullable<typeof oldData>[] = [], recoveredChannels: NonNullable<typeof newData>[] = [];
  const settings = new Map<string, unknown>([["hosts", [oldConfig]], ["project_paths", [`host:${h}:/p`]], ["file_explorer_on_start", false], ["webgl_rendering_enabled", false], ["terminal_header_stats", false], ["eager_init_tabs", true], ["rate_limit_in_sidebar", false], ["rate_limit_in_sidebar_codex", false], ["context_tree_enabled", false]]);
  const stores = new Map<string, Map<string, unknown>>([["settings.json", settings]]);
  vi.mocked(load).mockImplementation(async file => {
    const data = stores.get(file) ?? new Map<string, unknown>(); stores.set(file, data);
    return { get: async (key: string) => data.get(key) ?? null, set: async (key: string, value: unknown) => { data.set(key, value); }, save: async () => {} } as unknown as Awaited<ReturnType<typeof load>>;
  });
  const unexpected: string[] = [];
  vi.mocked(invoke).mockImplementation(async (command, args) => {
    const a = args as Record<string, unknown> | undefined;
    const method = command === "host_call" ? String(a?.method) : command;
    switch (method) {
      case "local_host_info": return { mode: "in-process", reason: null };
      case "hosts_available": return { sequence: 1, hosts: [descriptor] };
      case "hosts_status": return snapshots;
      case "hosts_configure": {
        const cfg = (a?.hosts as HostConfig[])[0];
        if (cfg.sshTarget !== "K2") return;
        order.push("configure-K2");
        snapshots = [{ status: status(h, 2), terminals: [] }];
        events.get("hosts:available")!({ payload: { sequence: 2, hosts: [{ ...descriptor, aliases: [h], daemonKey: null, routes: ["ssh"] }] } });
        events.get("hosts:status")!({ payload: status(h, 2) });
        events.get("hosts:terminals")!({ payload: { host: h, list: [] } });
        return;
      }
      case "host_term_attach":
        order.push(`attach-${String(a?.host)}`);
        if (a?.host === h) { oldData = a.onData as typeof oldData; oldChannels.push(oldData!); }
        else { newData = a?.onData as typeof newData; recoveredChannels.push(newData!); }
        return { exitCode: null };
      case "host_term_detach":
        order.push(`detach-${String(a?.host)}`);
        if (a?.host === h) {
          if (mode === "partial-abort" && a?.terminal === "second-uuid") throw new Error("controlled Detach failure");
          await detaching;
          if (mode === "offline-abort") {
            const offline = { ...status(h, 1), status: "offline" };
            snapshots = [{ status: offline as HostStatus, terminals: infos }];
            events.get("hosts:status")!({ payload: offline });
          }
          if (mode === "abort" || mode === "offline-abort") throw new Error("controlled Detach failure");
        }
        order.push("detached"); return;
      case "host_term_input": case "host_term_resize": return;
      case "list_ssh_hosts": case "list_claude_projects": case "list_pi_projects": case "get_all_recent_sessions": case "get_sessions": case "open_copies_pending": case "get_git_log": case "list_dir": case "list_project_session_ids": return [];
      case "detect_session_branch": return null;
      case "get_git_status": return { is_repo: false, files: [], branch: null };
      case "get_global_rate_limits": return {};
      case "get_codex_usage": return { present: false, primary: null, secondary: null, daily_sessions: [] };
      case "detect_agent_binary": return { installed: false };
      case "probe_statusline_setup": return { stats_dir_present: false, stats_session_count: 0 };
      case "app_build": return { version: "1.5.0", build: null };
      default: unexpected.push(method); throw new Error(`unexpected retarget native call ${method}`);
    }
  });
  const container = document.createElement("div"); document.body.append(container);
  const root = createRoot(container);
  const clickText = async (selector: string, text: string) => {
    const el = Array.from(container.querySelectorAll<HTMLElement>(selector)).find(e => e.textContent?.trim() === text);
    expect(el, `${selector}: ${text}`).toBeTruthy(); await act(async () => { el!.click(); });
  };
  try {
    await act(async () => { root.render(<App />); });
    const xterm = xterms[0]; expect(xterm).toBeTruthy();
    // Eager init is deliberately driven by the test's owned animation callback.
    await act(async () => { frames.splice(0).forEach(cb => cb(0)); });
    expect(oldData).toBeDefined();
    await act(async () => { oldData!.onmessage(new Uint8Array([75])); });
    const captured = captureTerminalInput({ id: "remote-original-uuid", type: "terminal", title: "Original", host: h, terminal: info.terminal });
    const settingsButton = container.querySelector<HTMLElement>('[aria-label="Settings"]')!;
    await act(async () => { settingsButton.click(); });
    await clickText(".settings-nav-item", "Hosts");
    await clickText(".host-row button", "Edit");
    const input = container.querySelector<HTMLInputElement>(".host-form-mono")!;
    await act(async () => {
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(input, "K2");
      input.dispatchEvent(new Event("input", { bubbles: true }));
    });
    await clickText(".host-form-panel button", "Save");
    expect(order).toContain(`detach-${h}`);
    expect(order).not.toContain("configure-K2");
    if (mode === "admission") {
      await act(async () => { container.querySelector<HTMLElement>(`[data-drop-path='host:${h}:/p']`)!.click(); });
      await clickText("button", "New Chat");
      const shell = Array.from(container.querySelectorAll<HTMLElement>(".agent-picker-option")).find(el => el.textContent?.trim() === "Shell");
      if (shell) await act(async () => shell.click());
      expect(pendingOpens.size).toBe(0);
      expect(xterms).toHaveLength(1);
    }
    const writes = xterm.write.mock.calls.length;
    oldData!.onmessage(new Uint8Array([75, 50]));
    expect(xterm.write.mock.calls).toHaveLength(writes);
    writeTerminalInput(captured, "late-before-detach");
    expect(vi.mocked(invoke).mock.calls.filter(([cmd]) => cmd === "host_term_input")).toEqual([]);
    if (mode === "partial-abort") expect(order.filter(v => v === `attach-${r}`)).toHaveLength(0);
    await act(async () => { releaseDetach(); });
    if (mode.includes("abort")) {
      expect(order).not.toContain("configure-K2");
      expect(settings.get("hosts")).toEqual([oldConfig]);
      if (mode === "offline-abort") {
        expect(order.filter(v => v === `attach-${r}`)).toHaveLength(0);
        await act(async () => { xterm.onInput!("must-stay-read-only"); });
        expect(vi.mocked(invoke).mock.calls.filter(([cmd]) => cmd === "host_term_input")).toEqual([]);
        snapshots = [{ status: status(h, 1), terminals: infos }];
        await act(async () => { events.get("hosts:status")!({ payload: status(h, 1) }); });
      }
      expect(order.filter(v => v === `attach-${r}`)).toHaveLength(infos.length);
      const before = xterms.map(term => term.write.mock.calls.length);
      oldChannels.forEach(channel => channel.onmessage(new Uint8Array([88])));
      expect(xterms.map(term => term.write.mock.calls.length)).toEqual(before);
      await act(async () => { recoveredChannels.forEach(channel => channel.onmessage(new Uint8Array([75]))); });
      expect(xterms.map(term => term.write.mock.calls.length)).toEqual(before.map(n => n + 1));
      xterms.forEach(term => term.onInput!("recovered-original"));
      const inputs = vi.mocked(invoke).mock.calls.filter(([cmd]) => cmd === "host_term_input");
      expect(inputs).toHaveLength(infos.length);
      expect(inputs.every(([, args]) => (args as { host: string }).host === r)).toBe(true);
      expect(xterms).toHaveLength(infos.length); expect(xterms.every(t => !t.dispose.mock.calls.length)).toBe(true);
      expect(vi.mocked(invoke).mock.calls.filter(([cmd]) => ["host_term_open", "host_term_close", "host_term_relaunch"].includes(cmd))).toEqual([]);
      return;
    }
    expect(order.indexOf("detached")).toBeLessThan(order.indexOf("configure-K2"));
    expect(order.filter(v => v.startsWith("attach-"))).toEqual([`attach-${h}`]);
    expect(container.querySelector('[data-drag-id="remote-original-uuid"]')).not.toBeNull();
    expect(xterms).toHaveLength(1); expect(xterm.dispose).not.toHaveBeenCalled();
    // K is unavailable: the original UUID remains stale, never attached to K2.
    expect(container.querySelector(".terminal-remote-banner")?.textContent).toContain("Waiting");
    writeTerminalInput(captured, "late-after-configure");
    snapshots = [{ status: status(h, 2), terminals: [] }, { status: status(r, 3), terminals: [info] }];
    await act(async () => {
      events.get("hosts:available")!({ payload: { sequence: 3, hosts: [{ ...descriptor, aliases: [h], daemonKey: null, routes: ["ssh"] }, { ...descriptor, id: r, aliases: [r], activeRoute: "relay", routes: ["relay"] }] } });
      events.get("hosts:status")!({ payload: status(r, 3) });
      events.get("hosts:terminals")!({ payload: { host: r, list: [info] } });
    });
    expect(order.filter(v => v.startsWith("attach-"))).toEqual([`attach-${h}`, `attach-${r}`]);
    expect(newData).toBeDefined();
    const beforeReplay = xterm.write.mock.calls.length;
    oldData!.onmessage(new Uint8Array([88]));
    expect(xterm.write.mock.calls).toHaveLength(beforeReplay);
    newData!.onmessage(new Uint8Array([75]));
    expect(xterm.write.mock.calls).toHaveLength(beforeReplay + 1);
    xterm.onInput!("original-K-only");
    const inputCalls = vi.mocked(invoke).mock.calls.filter(([cmd]) => cmd === "host_term_input");
    expect(inputCalls).toHaveLength(1); expect(inputCalls[0][1]).toMatchObject({ host: r, terminal: info.terminal, data: "original-K-only" });
    expect(vi.mocked(invoke).mock.calls.filter(([cmd]) => ["host_term_open", "host_term_close", "host_term_relaunch"].includes(cmd))).toEqual([]);
    expect(xterms).toHaveLength(1); expect(xterm.dispose).not.toHaveBeenCalled();
    expect(unexpected).toEqual([]);
  } finally {
    await act(async () => { root.unmount(); }); container.remove(); registry._dispose(); cache._reset(); vi.unstubAllGlobals();
  }
});
