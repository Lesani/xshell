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
import { keyOf } from "./hosts/projectKey";
import type { ProjectInfo, Tab } from "./types";
import type { HostConfig, HostSnapshot, HostStatus, TerminalInfo } from "./hosts/types";

// Keep App, navigation components, Host registry, selector, Banner and Action real.
// Only the native boundary and xterm/PTY rendering are replaced.
vi.mock("@tauri-apps/api/core", async importOriginal => ({
  ...await importOriginal<typeof import("@tauri-apps/api/core")>(),
  invoke: vi.fn(),
}));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({ setAlwaysOnTop: async () => {} }),
}));
vi.mock("@tauri-apps/api/app", () => ({ getVersion: async () => "1.5.0" }));
vi.mock("@tauri-apps/plugin-updater", () => ({ check: async () => null }));
vi.mock("@tauri-apps/plugin-store", () => ({ load: vi.fn() }));
vi.mock("./components/TerminalTab", () => ({
  TerminalTab: () => null,
  DARK_TERM_BG: "#1c1c1b",
  LIGHT_TERM_BG: "#faf9f5",
}));

const hostA: HostConfig = { id: "h_aaaaaaaa", name: "Project A Host", sshTarget: "fixture-a" };
const hostB: HostConfig = { id: "h_bbbbbbbb", name: "Project B Host", sshTarget: "fixture-b" };
const projectA: ProjectInfo = { name: "ProjectA", path: "/projects/a", encoded_name: "-projects-a", session_count: 0, last_active: "", host: hostA.id };
const projectB: ProjectInfo = { name: "ProjectB", path: "/projects/b", encoded_name: "-projects-b", session_count: 0, last_active: "", host: hostB.id };
const localProject: ProjectInfo = { name: "LocalProject", path: "/projects/local", encoded_name: "-projects-local", session_count: 0, last_active: "" };
const localTab: Tab = { id: "local-tab", type: "terminal", title: "Local Terminal", sessionId: "local-session", projectPath: localProject.path, projectName: localProject.name };
const eligibleStatus = (host: string): HostStatus => ({
  host, status: "build-differs", phase: null, lastError: null, errorHint: null,
  daemonVersion: "1.5.0", desktopVersion: "1.5.0", protocol: 1,
  os: "linux", arch: "x86_64", incompatibleReason: null, nextRetryAt: null, sinceMs: 1,
  configGeneration: 2, linkGeneration: 3, updateAction: "replace", updateUnavailable: null,
  daemonBuild: { commit: "3b9c9a89c10e", dirty: null, time: 1 },
  desktopBuild: { commit: "7ab4537a9baf", dirty: null, time: 2 },
});
const terminal = (project: ProjectInfo, id: string): TerminalInfo => ({
  terminal: id, spec: { agent: "claude", cwd: project.path, sessionId: `${id}-session` },
  meta: { title: `${project.name} Terminal`, projectName: project.name },
  createdAtMs: 1, pid: 100, exitCode: null,
});

let root: Root | undefined;
let container: HTMLDivElement;
afterEach(async () => {
  await act(async () => { root?.unmount(); });
  root = undefined;
  container?.remove();
  registry._dispose();
  cache._reset();
  hostUpdates.cancel();
  vi.unstubAllGlobals();
});

async function click(selector: string) {
  const element = container.querySelector<HTMLElement>(selector);
  expect(element, `navigation target ${selector}`).not.toBeNull();
  await act(async () => { element!.click(); });
}
const banner = () => container.querySelector(".main-content > .host-update-banner");
function expectNotice(host: HostConfig, owned = false) {
  // Soft assertions let the rest of the navigation controls execute on the RED tree.
  expect.soft(banner()?.textContent ?? "", `${host.name} notice in production App banner`).toContain(host.name);
  if (owned) {
    expect.soft(banner()?.textContent).toContain("different build of xshell");
    expect.soft(banner()?.textContent).toContain(`Update xshell on ${host.name}. The Desktop there manages this Daemon, so its build cannot be replaced from here.`);
    expect.soft(banner()?.querySelector("button")).toBeNull();
    expect.soft(container.querySelector(".host-confirm-panel")).toBeNull();
  } else expect.soft(banner()?.querySelector("button")?.textContent ?? "", `${host.name} replacement action`).toBe("Use this Desktop’s build");
}

it("scopes production App update actions and Desktop-owned guidance to the selected Remote Project", async () => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  // The updater's optional changelog fetch must not use the network.
  vi.stubGlobal("fetch", vi.fn(async () => ({ ok: true, json: async () => [] })));
  const settings = new Map<string, unknown>([
    ["hosts", [hostA, hostB]],
    ["project_paths", [keyOf(projectA), keyOf(projectB), keyOf(localProject)]],
    ["open_tabs", [localTab]],
    ["rate_limit_in_sidebar", false],
    ["rate_limit_in_sidebar_codex", false],
    ["context_tree_enabled", false],
  ]);
  const stores = new Map<string, Map<string, unknown>>([["settings.json", settings]]);
  vi.mocked(load).mockImplementation(async file => {
    const data = stores.get(file) ?? new Map<string, unknown>();
    stores.set(file, data);
    return {
      get: async (key: string) => data.get(key) ?? null,
      set: async (key: string, value: unknown) => { data.set(key, value); },
      save: async () => {},
    } as unknown as Awaited<ReturnType<typeof load>>;
  });
  const snapshots: HostSnapshot[] = [
    { status: eligibleStatus(hostA.id), terminals: [terminal(projectA, "terminal-a")] },
    { status: eligibleStatus(hostB.id), terminals: [terminal(projectB, "terminal-b")] },
  ];
  const unexpectedNativeCalls: unknown[] = [];
  vi.mocked(invoke).mockImplementation(async (command, args) => {
    const a = args as Record<string, unknown> | undefined;
    const host = command === "host_call" ? a?.host : undefined;
    const method = command === "host_call" ? a?.method : command;
    switch (method) {
      case "local_host_info": return { mode: "in-process", reason: null };
      case "hosts_configure": return;
      case "hosts_status": return snapshots;
      case "app_build": return { version: "1.5.0", build: eligibleStatus(hostA.id).desktopBuild };
      case "list_claude_projects": return [host === hostA.id ? projectA : host === hostB.id ? projectB : localProject];
      case "list_pi_projects":
      case "get_all_recent_sessions":
      case "get_sessions":
      case "open_copies_pending": return [];
      case "get_global_rate_limits": return { five_hour_pct: null, seven_day_pct: null, five_hour_resets_at: null, seven_day_resets_at: null };
      case "get_codex_usage": return { present: false, primary: null, secondary: null, plan_type: null, rate_limits_updated_iso: null, daily_sessions: [] };
      case "detect_agent_binary": return { installed: false };
      case "probe_statusline_setup": return { stats_dir_present: false, stats_session_count: 0 };
      default:
        unexpectedNativeCalls.push(method);
        throw new Error(`Unexpected native call in App notice fixture: ${String(method)}`);
    }
  });
  container = document.createElement("div");
  document.body.append(container);
  root = createRoot(container);
  await act(async () => { root!.render(<App />); });
  expect(container.querySelector(".app-loading-overlay")).toBeNull();
  expect(registry.getStatus(hostA.id)?.updateAction).toBe("replace");
  expect(registry.getStatus(hostB.id)?.updateAction).toBe("replace");

  // Global Home has no Host context even with eligible Remote Hosts connected.
  expect(banner()).toBeNull();
  await click(`[data-drop-path='${keyOf(projectA)}']`);
  expect(container.querySelector(".project-detail-main .sessions-view-title")?.textContent).toContain(projectA.name);
  expectNotice(hostA);

  // Selecting a real restored Local Tab leaves Project A selected in App state.
  await click('[data-drag-id="local-tab"]');
  expect(container.querySelector('[data-drag-id="local-tab"].active')).not.toBeNull();
  expect(container.querySelector(".project-detail-main .sessions-view-title")?.textContent).toBe(projectA.name);
  expect(banner()).toBeNull();

  // The same Host's Terminal already renders the real notice/action on the RED
  // tree, isolating the omission to the Project navigation/wiring boundary.
  await click('[data-drag-id="remote-terminal-a"]');
  expect(banner()?.textContent).toContain(hostA.name);
  expectNotice(hostA);

  // A different Terminal overrides the retained Project A selection.
  await click('[data-drag-id="remote-terminal-b"]');
  expect(banner()?.textContent).toContain(hostB.name);
  expect(banner()?.querySelector("button")?.textContent).toBe("Use this Desktop’s build");
  expect(banner()?.textContent).not.toContain(hostA.name);

  await click(`[data-drop-path='${keyOf(projectB)}']`);
  expect(container.querySelector(".project-detail-main .sessions-view-title")?.textContent).toBe(projectB.name);
  expectNotice(hostB);
  expect(banner()?.textContent ?? "").not.toContain(hostA.name);

  await click(`[data-drop-path='${keyOf(projectA)}']`);
  await click('[aria-label="Settings"]');
  expect(container.querySelector(".tb-settings-btn.active")).not.toBeNull();
  expect(banner()).toBeNull();

  // Explicit Go Home must clear, rather than inherit, the selected Remote Project.
  await click(`[data-drop-path='${keyOf(projectA)}']`);
  await click(".ds-home");
  expect(container.querySelector("[data-drop-path].active")).toBeNull();
  expect(container.querySelector(".project-detail-split")).toBeNull();
  expect(banner()).toBeNull();

  await click(`[data-drop-path='${keyOf(localProject)}']`);
  expect(banner()).toBeNull();

  // Eligibility remains authoritative in the existing Host status / Action seam.
  await click(`[data-drop-path='${keyOf(projectA)}']`);
  // A positively identified Desktop-owned target keeps build eligibility, but neither
  // production surface may open a confirmation or invoke a native update.
  for (const status of ["build-differs", "incompatible", "reconnecting"] as const) {
    await act(async () => { registry.applyStatus({ ...eligibleStatus(hostA.id), status, updateUnavailable: "desktop-owned" }); });
    expectNotice(hostA, true);
    expect(banner()?.textContent).not.toContain("Windows");
    hostUpdates.ask(hostA, registry.getStatus(hostA.id)!);
    expect(hostUpdates.getSnapshot().confirmation).toBeNull();
  }
  // An old target's exact refusal stays visible, alongside ownership-specific guidance.
  const legacyRefusal = "xshell on this machine runs these terminals; update xshell there instead";
  await act(async () => { registry.applyStatus({ ...eligibleStatus(hostA.id), updateUnavailable: "desktop-owned", errorHint: "update-failed", lastError: legacyRefusal }); });
  expect(banner()?.textContent).toContain(legacyRefusal);
  await click('[aria-label="Settings"]');
  const hostsTab = [...container.querySelectorAll<HTMLElement>(".settings-nav-item")].find(b => b.textContent === "Hosts");
  expect(hostsTab).toBeDefined();
  await act(async () => { hostsTab!.click(); });
  const row = [...container.querySelectorAll(".host-row")].find(r => r.textContent?.includes(hostA.name));
  expect.soft(row?.textContent).toContain(`Update xshell on ${hostA.name}. The Desktop there manages this Daemon, so its build cannot be replaced from here.`);
  expect.soft(row?.querySelector(".host-update-action button")).toBeNull();
  await click(`[data-drop-path='${keyOf(projectB)}']`);
  expectNotice(hostB);
  await click(`[data-drop-path='${keyOf(projectA)}']`);
  const matching = eligibleStatus(hostA.id);
  await act(async () => {
    registry.applyStatus({ ...matching, status: "connected", updateAction: null, daemonBuild: matching.desktopBuild });
  });
  expect(container.querySelector(".main-content > .host-update-banner .host-update-action")).toBeNull();
  expect(banner()?.textContent ?? "").toBe("");

  // A stale selected Project cannot manufacture an action after its config is removed.
  await act(async () => {
    registry.applyStatus(eligibleStatus(hostA.id));
    await registry.configure([hostB]);
  });
  expect(registry.config(hostA.id)).toBeUndefined();
  expect(banner()).toBeNull();
  // No replacement is invoked by navigation or rendering an eligible action.
  expect(vi.mocked(invoke).mock.calls.some(([command]) => command === "host_upgrade")).toBe(false);
  expect(vi.mocked(invoke).mock.calls.some(([command, args]) => command === "host_call" && /prepare|upgrade|update/.test(String((args as { method?: string })?.method)))).toBe(false);
  expect(unexpectedNativeCalls).toEqual([]);
});
