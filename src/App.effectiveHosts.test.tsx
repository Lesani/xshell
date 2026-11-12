// @vitest-environment happy-dom
import { act } from "react";
import { createRoot } from "react-dom/client";
import { expect, it, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";
import { load } from "@tauri-apps/plugin-store";
import App from "./App";
import { registry } from "./hosts/registry";
import { cache } from "./hosts/cache";
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

it("actual App discovers/selects Ring history without saved SSH or Local Daemon; revocation preserves personal tabs and paths", async () => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  vi.stubGlobal("fetch", vi.fn(async () => ({ ok: true, json: async () => [] })));
  const id = `r_${"d".repeat(64)}`, key = `host:${id}:/projects/ring`;
  const project = { name: "RingProject", path: "/projects/ring", encoded_name: "-projects-ring", session_count: 1, last_active: "2026-11-12" };
  const session = { id: "historical", title: "Retained Ring history", project_name: project.name, project_path: project.path, timestamp: "2026-11-12", agent: "claude", message_count: 1, model: "fixture", daily_cost: {}, daily_tokens: {} } as SessionInfo;
  const terminal: TerminalInfo = { terminal: "retained-uuid", spec: { agent: "claude", cwd: project.path, sessionId: "active-session" }, meta: { title: "Retained Ring Terminal", projectName: project.name }, createdAtMs: 1, pid: 42, exitCode: null };
  const status: HostStatus = { host: id, status: "build-differs", phase: null, lastError: null, errorHint: null, daemonVersion: "1.5.0", desktopVersion: "1.5.0", protocol: 1, os: null, arch: null, incompatibleReason: null, nextRetryAt: null, sinceMs: 1, configGeneration: 1, linkGeneration: 2, updateAction: null, routeUpdateUnavailable: "relay-only", updateUnavailable: "desktop-owned" };
  const available: AvailableSnapshot = { sequence: 1, hosts: [{ id, aliases: [id], name: "Actual Ring Host", color: null, daemonKey: "key", activeRoute: "relay", routes: ["relay"], reachability: { kind: "online" } }] };
  const settings = new Map<string, unknown>([["hosts", []], ["project_paths", [key]], ["rate_limit_in_sidebar", false], ["rate_limit_in_sidebar_codex", false], ["context_tree_enabled", false]]);
  const stores = new Map<string, Map<string, unknown>>([["settings.json", settings]]);
  const writes: string[] = [];
  vi.mocked(load).mockImplementation(async file => {
    const data = stores.get(file) ?? new Map<string, unknown>(); stores.set(file, data);
    return { get: async (key: string) => data.get(key) ?? null, set: async (key: string, value: unknown) => { writes.push(`${file}:${key}`); data.set(key, value); }, save: async () => {} } as unknown as Awaited<ReturnType<typeof load>>;
  });
  let snapshots: HostSnapshot[] = [], offline = false;
  const unexpected: string[] = [];
  vi.mocked(invoke).mockImplementation(async (command, args) => {
    const a = args as Record<string, unknown> | undefined;
    const host = command === "host_call" ? a?.host : undefined;
    const method = command === "host_call" ? String(a?.method) : command;
    if (host && offline) throw { code: "unknown-host", message: "signed membership withdrawn" };
    switch (method) {
      case "local_host_info": return { mode: "in-process", reason: null };
      case "hosts_configure": expect(a?.hosts).toEqual([]); return;
      case "hosts_available": return { sequence: 0, hosts: [] };
      case "hosts_status": return snapshots;
      case "app_build": return { version: "1.5.0", build: null };
      case "list_claude_projects": return host ? [project] : [];
      case "list_pi_projects": case "list_codex_projects": case "list_cursor_projects": case "list_opencode_projects": case "list_antigravity_projects": return [];
      case "get_all_recent_sessions": case "get_sessions": return host ? [session] : [];
      case "open_copies_pending": return [];
      case "get_global_rate_limits": return {};
      case "get_codex_usage": return { present: false, primary: null, secondary: null, daily_sessions: [] };
      case "detect_agent_binary": return { installed: false };
      case "probe_statusline_setup": return { stats_dir_present: false, stats_session_count: 0 };
      default: unexpected.push(method); throw new Error(`unexpected native call ${method}`);
    }
  });
  const container = document.createElement("div"); document.body.append(container);
  const root = createRoot(container);
  async function click(selector: string) {
    const element = container.querySelector<HTMLElement>(selector); expect(element, selector).not.toBeNull();
    await act(async () => { element!.click(); });
  }
  try {
    await act(async () => { root.render(<App />); });
    expect(registry.initialized).toBe(true);
    expect(registry.getSnapshot().configs).toEqual([]);
    expect(registry.localEnabled).toBe(false);
    snapshots = [{ status, terminals: [terminal] }];
    // Production status can precede the discovery projection. The registry must not lose it.
    await act(async () => {
      events.get("hosts:status")!({ payload: status });
      events.get("hosts:terminals")!({ payload: { host: id, list: [terminal] } });
      events.get("hosts:available")!({ payload: available });
    });
    expect(container.querySelector('[data-drag-id="remote-retained-uuid"]')).not.toBeNull();
    await click(`[data-drop-path='${key}']`);
    expect(container.querySelector(".project-detail-main .sessions-view-title")?.textContent).toBe("RingProject");
    expect(container.textContent).toContain("Retained Ring history");
    expect(container.querySelector(".main-content > .host-update-banner")?.textContent).toContain("Relay access");
    expect(container.querySelector(".main-content > .host-update-banner button")).toBeNull();
    await click('[aria-label="Settings"]');
    const hostsNav = Array.from(container.querySelectorAll<HTMLElement>(".settings-nav-item")).find(e => e.textContent === "Hosts")!;
    await act(async () => { hostsNav.click(); });
    expect(container.querySelector("[data-ring-host]")?.textContent).toContain("Actual Ring Host");
    expect(Array.from(container.querySelectorAll("[data-ring-host] button")).map(b => b.textContent)).toEqual(["Reconnect"]);
    await click(".ds-add");
    expect(container.querySelector(".picker-group-name")?.textContent).toBe("This computer");
    expect(container.querySelector(".picker-group .picker-item")?.textContent).toContain("ring");
    const done = Array.from(container.querySelectorAll<HTMLButtonElement>(".picker button")).find(b => b.textContent === "Done")!;
    await act(async () => { done.click(); });
    offline = true; snapshots = [];
    await act(async () => { events.get("hosts:available")!({ payload: { sequence: 2, hosts: [] } }); });
    expect(registry.isUsable(id)).toBe(false);
    expect(container.querySelector("[data-ring-host]")).toBeNull();
    expect(container.querySelector('[data-drag-id="remote-retained-uuid"]')).not.toBeNull();
    await click(`[data-drop-path='${key}']`);
    expect(container.textContent).toContain("Retained Ring history");
    expect(container.querySelector(".home-stale-banner")?.textContent).toContain("cached project data");
    expect(settings.get("project_paths")).toEqual([key]);
    expect(settings.get("hosts")).toEqual([]);
    expect(writes).not.toContain("settings.json:hosts");
    expect(vi.mocked(invoke).mock.calls.filter(([command]) => ["host_test", "host_upgrade", "host_term_close", "host_term_open"].includes(command))).toEqual([]);
    expect(unexpected).toEqual([]);
  } finally {
    await act(async () => { root.unmount(); }); container.remove(); registry._dispose(); cache._reset(); vi.unstubAllGlobals();
  }
});
