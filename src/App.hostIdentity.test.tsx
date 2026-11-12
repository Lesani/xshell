// @vitest-environment happy-dom
import { act } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, expect, it, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";
import { load } from "@tauri-apps/plugin-store";
import type { AvailableSnapshot, HostSnapshot, HostStatus, TerminalInfo } from "./hosts/types";
import type { SessionInfo } from "./types";

const events = vi.hoisted(() => new Map<string, (event: { payload: unknown }) => void>());
vi.mock("@tauri-apps/api/core", async original => ({ ...await original<typeof import("@tauri-apps/api/core")>(), invoke: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({ listen: async (name: string, handler: (event: { payload: unknown }) => void) => { events.set(name, handler); return () => events.delete(name); } }));
vi.mock("@tauri-apps/api/window", () => ({ getCurrentWindow: () => ({ setAlwaysOnTop: async () => {} }) }));
vi.mock("@tauri-apps/api/app", () => ({ getVersion: async () => "1.5.0" }));
vi.mock("@tauri-apps/plugin-updater", () => ({ check: async () => null }));
vi.mock("@tauri-apps/plugin-store", () => ({ load: vi.fn() }));
// Real App/Settings/Picker/navigation/history and registry; PTY/WebKit painting is not native E2E.
vi.mock("./components/TerminalTab", () => ({ TerminalTab: () => null, DARK_TERM_BG: "#1c1c1b", LIGHT_TERM_BG: "#faf9f5" }));

let cleanup: (() => Promise<void>) | undefined;
afterEach(async () => { await cleanup?.(); cleanup = undefined; vi.unstubAllGlobals(); });
const H = "h_aaaaaaaa", R = `r_${"a".repeat(64)}`;
async function setup(startRingOnly = false, localAlias = false, pinSaved = false) {
  vi.resetModules(); events.clear();
  const { registry } = await import("./hosts/registry"), { cache } = await import("./hosts/cache");
  const { pendingOpens } = await import("./hosts/terminalTransport");
  const { default: App } = await import("./App");
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  vi.stubGlobal("fetch", vi.fn(async () => ({ ok: true, json: async () => [] })));
  const project = (name: string) => ({ name: `Stored-${name}`, path: `/projects/${name}`, encoded_name: `-${name}`, session_count: 1, last_active: "2026-11-12" });
  const session = (title: string) => ({ id: title, title, project_name: "K1-Claude", project_path: "/projects/K1-Claude", timestamp: "2026-11-12", agent: "claude", message_count: 1, model: "fixture", daily_tokens: {}, daily_cost: {} } as SessionInfo);
  let generation = 1, identity = "K1", failure = "none", representative = startRingOnly ? R : localAlias ? "local" : H;
  const descriptor = () => ({ id: representative, aliases: representative === R ? [R] : [representative, identity === "K1" ? R : `r_${"b".repeat(64)}`], name: identity, color: null, daemonKey: identity, activeRoute: representative === R ? "relay" : localAlias ? "local" : "ssh", routes: representative === R ? ["relay"] : localAlias ? ["local", "relay"] : ["ssh", "relay"], reachability: { kind: "online" } });
  const status = (): HostStatus => ({ host: representative, status: "build-differs", phase: null, lastError: null, errorHint: null, daemonVersion: "1.5.0", desktopVersion: "1.5.0", protocol: 1, os: "linux", arch: "x86_64", incompatibleReason: null, nextRetryAt: null, sinceMs: 1, configGeneration: generation, linkGeneration: generation, updateAction: "replace" });
  const settings = new Map<string, unknown>([["hosts", startRingOnly || localAlias ? [] : [{ id: H, name: "Saved", sshTarget: "saved" }]], ["project_paths", pinSaved ? [`host:${H}:/projects/K1-Claude`, `host:${H}:/projects/K1-Pi`] : [`host:${R}:/projects/K1-Claude`]], ["rate_limit_in_sidebar", false], ["rate_limit_in_sidebar_codex", false], ["context_tree_enabled", false], ["default_agent", "ask"]]);
  const stores = new Map<string, Map<string, unknown>>([["settings.json", settings]]);
  vi.mocked(load).mockImplementation(async file => { const data = stores.get(file) ?? new Map(); stores.set(file, data); return { get: async (key: string) => data.get(key) ?? null, set: async (key: string, value: unknown) => { data.set(key, value); }, save: async () => {} } as unknown as Awaited<ReturnType<typeof load>>; });
  vi.mocked(invoke).mockImplementation(async (command, args) => {
    const a = args as Record<string, unknown> | undefined, remote = command === "host_call", method = remote ? String(a?.method) : command;
    if (remote && identity === "K2" && failure !== "none") {
      const succeeds = failure === "projects" ? method === "list_claude_projects" : failure === "history" ? method === "get_all_recent_sessions" : failure === "pi" ? method === "list_pi_projects" : false;
      if (!succeeds) throw { code: "remote", message: "controlled K2 read failure" };
    }
    switch (method) {
      case "local_host_info": return { mode: "in-process", reason: null };
      case "hosts_available": return { sequence: generation, hosts: [descriptor()] };
      case "hosts_status": return [{ status: status(), terminals: null }];
      case "hosts_configure": return;
      case "list_claude_projects": return remote ? [project(`${identity}-Claude`)] : localAlias ? [project("K1-Claude")] : [];
      case "list_pi_projects": return remote ? [project(`${identity}-Pi`)] : [];
      case "get_all_recent_sessions": case "get_sessions": return remote ? [session(`${identity}-History`)] : [];
      case "list_codex_projects": case "list_cursor_projects": case "list_opencode_projects": case "list_antigravity_projects": case "open_copies_pending": return [];
      case "detect_agent_binary": return { installed: (remote ? (a?.params as { binary?: string })?.binary : a?.binary) === "claude" };
      case "app_build": return { version: "1.5.0", build: null };
      case "probe_statusline_setup": return { stats_dir_present: false, stats_session_count: 0 };
      case "get_global_rate_limits": return {};
      case "get_codex_usage": return { present: false, primary: null, secondary: null, daily_sessions: [] };
      default: throw new Error(`Unexpected identity fixture command ${method}`);
    }
  });
  const container = document.createElement("div"); document.body.append(container); const root = createRoot(container);
  cleanup = async () => { await act(async () => root.unmount()); container.remove(); registry._dispose(); cache._reset(); };
  await act(async () => root.render(<App />));
  const publish = async (newIdentity: string, mode = "none", rep = representative) => {
    identity = newIdentity; failure = mode; representative = rep; generation++;
    if (rep === H && !registry.isConfigured(H)) {
      const saved = [{ id: H, name: "Saved", sshTarget: "saved" }];
      settings.set("hosts", saved); await act(async () => { await registry.configure(saved); });
    }
    await act(async () => { events.get("hosts:available")!({ payload: { sequence: generation, hosts: [descriptor()] } }); events.get("hosts:status")!({ payload: status() }); });
  };
  const click = async (selector: string, text?: string) => { const el = text ? Array.from(container.querySelectorAll<HTMLElement>(selector)).find(el => el.textContent?.trim() === text) : container.querySelector<HTMLElement>(selector); expect(el).toBeTruthy(); await act(async () => el!.click()); };
  return { container, registry, pendingOpens, publish, click };
}

it("mixed r/h history Open retains original operational Ring identity and its banner cannot borrow SSH update authority", async () => {
  const f = await setup(true);
  await f.publish("K1", "none", H);
  await f.click(".session-item");
  expect([...f.pendingOpens.values()].map(p => p.host)).toEqual([R]);
  await f.click(`[data-drop-path='host:${R}:/projects/K1-Claude']`);
  expect(f.container.querySelector(".main-content > .host-update-banner button")).toBeNull();
  await f.click('[aria-label="Settings"]'); await f.click(".settings-nav-item", "Hosts");
  expect(Array.from(f.container.querySelectorAll(".host-row button")).some(b => b.textContent?.includes("Use this Desktop"))).toBe(true);
});

it.each([false, true])("alias agent observation offers Claude without replacing operational r host (Local=%s)", async local => {
  const f = await setup(false, local);
  await f.click(`[data-drop-path='host:${R}:/projects/K1-Claude']`);
  await f.click("button", "New Chat");
  expect(f.container.querySelector(".agent-picker")?.textContent).toContain("Claude Code");
  await f.click(".agent-picker-option", "Claude Code");
  expect([...f.pendingOpens.values()].map(p => p.host)).toEqual([R]);
});

it.each(["all", "projects", "history", "pi"])("App and mounted Picker never relabel K1 fields/agents as K2 after %s success/failure", async mode => {
  const f = await setup(false, false, true);
  expect(f.container.textContent).toContain("K1-History");
  expect(f.container.querySelector(".project-pills")?.textContent).toContain("Stored-K1-Claude");
  await f.click(".ds-add");
  expect(f.container.querySelector(".picker-group")?.textContent).toContain("K1-Claude");
  await f.publish("K2", mode);
  const picker = f.container.querySelector(".picker-group")!;
  expect.soft(picker.textContent).not.toContain("K1-Claude"); expect.soft(picker.textContent).not.toContain("K1-Pi");
  expect.soft(f.container.querySelector(".home-view")?.textContent ?? f.container.textContent).not.toContain("K1-History");
  expect.soft(f.container.querySelector(".project-pills")?.textContent).not.toContain("Stored-K1-Claude");
  expect.soft(f.container.querySelector(".project-pills")?.textContent).not.toContain("Stored-K1-Pi");
  if (mode === "projects") expect(picker.textContent).toContain("K2-Claude");
  if (mode === "pi") expect(picker.textContent).toContain("K2-Pi");
});

it("a requested Ring banner cannot borrow canonical SSH lifecycle authority", async () => {
  const f = await setup();
  await f.click(`[data-drop-path='host:${R}:/projects/K1-Claude']`);
  expect(f.container.querySelector(".main-content > .host-update-banner button")).toBeNull();
  await f.click('[aria-label="Settings"]'); await f.click(".settings-nav-item", "Hosts");
  expect(Array.from(f.container.querySelectorAll(".host-row button")).some(b => b.textContent?.includes("Use this Desktop"))).toBe(true);
});
