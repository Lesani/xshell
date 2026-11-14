// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, expect, it, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";
import { load } from "@tauri-apps/plugin-store";
import App from "./App";
import { cache, emptyCache, putTerminals } from "./hosts/cache";
import { hostUpdates } from "./hosts/hostUpdate";
import { LOCAL_HOST, type LocalHostInfo } from "./hosts/localHost";
import { keyOf } from "./hosts/projectKey";
import { registry } from "./hosts/registry";
import type { Group, ProjectInfo } from "./types";
import type { HostConfig, HostSnapshot, HostStatus, TerminalInfo } from "./hosts/types";

const nativeListeners = vi.hoisted(() => new Map<string, Set<(event: { payload: unknown }) => void>>());

// Only native IPC/store/updater and the xterm/PTY renderer are mocked. In particular,
// App startup, Local migration, registry, navigation, selector and Banner stay real.
vi.mock("@tauri-apps/api/core", async importOriginal => ({
  ...await importOriginal<typeof import("@tauri-apps/api/core")>(),
  invoke: vi.fn(),
}));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async (event: string, callback: (event: { payload: unknown }) => void) => {
    const callbacks = nativeListeners.get(event) ?? new Set();
    callbacks.add(callback);
    nativeListeners.set(event, callbacks);
    return () => { callbacks.delete(callback); };
  }),
}));
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

const hostA: HostConfig = { id: "h_aaaaaaaa", name: "Remote A", sshTarget: "fixture-a" };
const hostB: HostConfig = { id: "h_bbbbbbbb", name: "Remote B", sshTarget: "fixture-b" };
const localProject: ProjectInfo = { name: "LocalProject", path: "/projects/local", encoded_name: "-projects-local", session_count: 0, last_active: "" };
const projectA: ProjectInfo = { name: "ProjectA", path: "/projects/a", encoded_name: "-projects-a", session_count: 0, last_active: "", host: hostA.id };
const projectB: ProjectInfo = { name: "ProjectB", path: "/projects/b", encoded_name: "-projects-b", session_count: 0, last_active: "", host: hostB.id };
const localInfo: LocalHostInfo = {
  mode: "daemon", reason: null,
  persistent: { supported: true, enabled: true, running: "persistent", log: "/fixture/xshelld.log" },
};
const eligibleStatus = (host: string): HostStatus => ({
  host, status: "build-differs", phase: null, lastError: null, errorHint: null,
  daemonVersion: "1.5.0", desktopVersion: "1.5.0", protocol: 1,
  os: host === LOCAL_HOST ? null : "linux", arch: host === LOCAL_HOST ? null : "x86_64",
  incompatibleReason: null, nextRetryAt: null, sinceMs: 1,
  configGeneration: 1, linkGeneration: 1, updateAction: "replace", updateUnavailable: null,
  daemonCapabilities: ["call", "term", "daemon.upgrade"],
  daemonBuild: { commit: "3b9c9a89c10e", dirty: null, time: 1793039087 },
  desktopBuild: { commit: "7ab4537a9baf", dirty: null, time: 1793311604 },
});
const shell = (terminal: string, project: ProjectInfo): TerminalInfo => ({
  terminal,
  spec: { agent: null, sessionId: null, cwd: project.path, shellMode: "raw", shellId: "bash", shellCommand: "bash", fullscreenRendering: true, forceSyncOutput: true },
  meta: { title: "Bash", projectName: project.name, createdAt: 1 },
  createdAtMs: 1, pid: 100, exitCode: null,
});
const localTerminal = shell("11111111-1111-4111-8111-111111111111", localProject);
const localSplitTerminal = shell("22222222-2222-4222-8222-222222222222", localProject);
const terminalA = shell("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", projectA);
const terminalB = shell("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb", projectB);
const remoteSplitTerminal = shell("cccccccc-cccc-4ccc-8ccc-cccccccccccc", projectB);
const tabId = (terminal: TerminalInfo) => `remote-${terminal.terminal}`;
const mixedGroup: Group = {
  id: "mixed-local-remote", name: "Local and Remote split",
  layout: { kind: "split", direction: "col", ratio: 0.5, children: [
    { kind: "leaf", tabId: tabId(localSplitTerminal) },
    { kind: "leaf", tabId: tabId(remoteSplitTerminal) },
  ] },
};
const settingsHint = "Check Settings → Hosts → This computer for available actions.";
const differentBuildText = "This computer is running a different build of xshell 1.5.0 (3b9c9a89c10e). This Desktop uses 7ab4537a9baf.";

let root: Root | undefined;
let container: HTMLDivElement;
afterEach(async () => {
  await act(async () => { root?.unmount(); });
  root = undefined;
  container?.remove();
  registry._dispose();
  cache._reset();
  hostUpdates.cancel();
  nativeListeners.clear();
  vi.unstubAllGlobals();
});

async function click(selector: string) {
  const element = container.querySelector<HTMLElement>(selector);
  expect(element, `navigation target ${selector}`).not.toBeNull();
  await act(async () => { element!.click(); });
}
async function clickText(selector: string, text: string) {
  const elements = [...container.querySelectorAll<HTMLElement>(selector)].filter(e => e.textContent?.trim() === text);
  expect(elements, `navigation target ${selector}: ${text}`).toHaveLength(1);
  await act(async () => { elements[0].click(); });
}
async function focusPane(terminal: TerminalInfo) {
  const pane = container.querySelector<HTMLElement>(`[data-group-leaf="${tabId(terminal)}"]`);
  expect(pane, `restored group leaf ${terminal.terminal}`).not.toBeNull();
  await act(async () => { pane!.dispatchEvent(new PointerEvent("pointerdown", { bubbles: true, button: 0, pointerId: 1 })); });
  expect(pane!.classList.contains("group-pane-active")).toBe(true);
}
const banner = () => container.querySelector(".main-content > .host-update-banner");
function expectNoControls() {
  expect(banner()?.querySelectorAll("button, a, input, [role='button']").length ?? 0).toBe(0);
}
function expectLocalNotice(context: string, description = differentBuildText) {
  const text = banner()?.textContent ?? "";
  // Only required Local presence is soft so all native/navigation controls run on RED.
  expect.soft(text, `${context}: native Local description`).toContain(description);
  expect.soft(text, `${context}: textual Settings guidance`).toContain(settingsHint);
  expect(text).not.toContain(hostA.name);
  expect(text).not.toContain(hostB.name);
  expectNoControls();
}
function expectRemoteNotice(host: HostConfig) {
  expect(banner()?.textContent).toContain(`${host.name} is running a different build of xshell`);
  expect(banner()?.querySelector("button")?.textContent).toBe("Use this Desktop’s build");
  expect(banner()?.textContent).not.toContain("This computer");
  expect(banner()?.textContent).not.toContain(settingsHint);
}
async function localStatus(patch: Partial<HostStatus>) {
  const status = { ...eligibleStatus(LOCAL_HOST), ...patch };
  const callbacks = nativeListeners.get("hosts:status");
  expect(callbacks?.size, "real registry subscribed to native Host status").toBe(1);
  await act(async () => { for (const callback of callbacks!) callback({ payload: status }); });
  expect(registry.getStatus(LOCAL_HOST)).toEqual(status);
}

it("shows only text guidance for eligible native Local Daemon contexts, preserving Remote actions and status/navigation guards", async () => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  vi.stubGlobal("fetch", vi.fn(async () => ({ ok: true, json: async () => [] })));
  const snapshots: HostSnapshot[] = [
    { status: eligibleStatus(LOCAL_HOST), terminals: [localTerminal, localSplitTerminal] },
    { status: eligibleStatus(hostA.id), terminals: [terminalA] },
    { status: eligibleStatus(hostB.id), terminals: [terminalB, remoteSplitTerminal] },
  ];
  // Normal public cache + saved groups restore Daemon-backed leaves before live reconcile.
  let cached = emptyCache();
  for (const snapshot of snapshots) cached = putTerminals(cached, snapshot.status.host, snapshot.terminals!, 1);
  const settings = new Map<string, unknown>([
    ["hosts", [hostA, hostB]],
    ["project_paths", [keyOf(localProject), keyOf(projectA), keyOf(projectB)]],
    ["open_tabs", []],
    ["open_groups", [mixedGroup]],
    ["rate_limit_in_sidebar", false],
    ["rate_limit_in_sidebar_codex", false],
    ["context_tree_enabled", false],
  ]);
  const stores = new Map<string, Map<string, unknown>>([
    ["settings.json", settings],
    ["hosts-cache.json", new Map([["cache", cached]])],
  ]);
  vi.mocked(load).mockImplementation(async file => {
    const data = stores.get(file) ?? new Map<string, unknown>();
    stores.set(file, data);
    return {
      get: async (key: string) => data.get(key) ?? null,
      set: async (key: string, value: unknown) => { data.set(key, value); },
      save: async () => {},
    } as unknown as Awaited<ReturnType<typeof load>>;
  });
  const unexpectedNativeCalls: unknown[] = [];
  vi.mocked(invoke).mockImplementation(async (command, args) => {
    const a = args as Record<string, unknown> | undefined;
    const host = command === "host_call" ? a?.host : undefined;
    const method = command === "host_call" ? a?.method : command;
    switch (method) {
      case "local_host_info": return localInfo;
      // Daemon startup reads this even with no legacy Tabs. Keep migration real.
      case "local_migration_journal_read": return null;
      // Settings → Hosts mounts the Ring section (#87).
      case "ring_status": case "ring_join_overview": return null;
      case "hosts_available": return { sequence: 0, hosts: [] };
      case "hosts_configure": return;
      case "hosts_status": return snapshots;
      case "app_build": return { version: "1.5.0", build: eligibleStatus(LOCAL_HOST).desktopBuild };
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
        throw new Error(`Unexpected native call in Local notice fixture: ${String(method)}`);
    }
  });
  container = document.createElement("div");
  document.body.append(container);
  root = createRoot(container);
  await act(async () => { root!.render(<App />); });
  expect(container.querySelector(".app-loading-overlay")).toBeNull();
  expect(registry.localEnabled).toBe(true);
  expect(registry.getSnapshot().configs.map(c => c.id)).toEqual([hostA.id, hostB.id]);
  expect(registry.getStatus(LOCAL_HOST)).toEqual(eligibleStatus(LOCAL_HOST));
  expect(registry.getSnapshot().live[LOCAL_HOST]?.map(t => t.terminal)).toEqual([localTerminal.terminal, localSplitTerminal.terminal]);
  expect(vi.mocked(invoke).mock.calls.filter(([command]) => command === "local_migration_journal_read")).toHaveLength(1);
  expect(unexpectedNativeCalls).toEqual([]);

  // Eligible Local metadata alone does not make global Home a Host context.
  expect(banner()).toBeNull();
  await click(`[data-drop-path='${keyOf(localProject)}']`);
  expect(container.querySelector(".project-detail-main .sessions-view-title")?.textContent).toBe(localProject.name);
  expectLocalNotice("selected Local Project");

  await click(`[data-drag-id="${tabId(terminalA)}"]`);
  expectRemoteNotice(hostA);
  expect(container.querySelector(".project-detail-main .sessions-view-title")?.textContent).toBe(localProject.name);
  await click(`[data-drop-path='${keyOf(projectA)}']`);
  expectRemoteNotice(hostA);

  await click(`[data-drag-id="${tabId(localTerminal)}"]`);
  expect(container.querySelector(`[data-drag-id="${tabId(localTerminal)}"].active`)).not.toBeNull();
  expect(container.querySelector(".project-detail-main .sessions-view-title")?.textContent).toBe(projectA.name);
  expectLocalNotice("Local Daemon Tab with stale Remote A Project");

  await click(`[data-drop-path='${keyOf(projectB)}']`);
  expectRemoteNotice(hostB);
  await click(`[data-drag-id="${tabId(terminalB)}"]`);
  expectRemoteNotice(hostB);

  await click(`[data-drop-path='${keyOf(localProject)}']`);
  await click(".tab-group-item");
  expect(container.querySelector(".tab-group-item.active")?.textContent).toContain(mixedGroup.name);
  expect(container.querySelector(`[data-group-leaf="${tabId(localSplitTerminal)}"].group-pane-active`)).not.toBeNull();
  expectLocalNotice("mixed split first Local leaf with no saved focus");
  await focusPane(remoteSplitTerminal);
  expectRemoteNotice(hostB);
  await focusPane(localSplitTerminal);
  expectLocalNotice("mixed split focused Local leaf");

  await click('[aria-label="Settings"]');
  expect(container.querySelector(".tb-settings-btn.active")).not.toBeNull();
  expect(banner()).toBeNull();
  await clickText(".settings-nav-item", "Hosts");
  const localSection = [...container.querySelectorAll(".settings-section")].find(section => section.querySelector(".settings-section-title")?.textContent === "This computer");
  expect(localSection, "existing This computer Settings section").toBeDefined();
  expect(localSection!.textContent).toContain("Terminal setup 1.5.0 (3b9c9a89c10e) · keeps running");
  expect(localSection!.textContent).toContain("Different build");
  const replace = [...localSection!.querySelectorAll("button")].find(button => button.textContent?.trim() === "Replace with Desktop’s build");
  expect(replace, "existing Settings replacement control").toBeDefined();
  expect(replace!.disabled).toBe(false);
  expect(banner()).toBeNull();

  await click(".ds-home");
  expect(container.querySelector(".project-detail-split")).toBeNull();
  expect(banner()).toBeNull();
  await click(`[data-drag-id="${tabId(localTerminal)}"]`);
  expectLocalNotice("active Local Daemon Tab");

  await localStatus({ status: "upgrade-pending", daemonVersion: "1.4.0", updateAction: "update" });
  expectLocalNotice("native older version", "This computer is running xshell 1.4.0. This Desktop uses 1.5.0.");
  expect(banner()?.textContent ?? "").not.toContain("different build");
  await localStatus({});
  expectLocalNotice("native different build");
  await localStatus({ daemonBuild: null });
  expectLocalNotice("native different build with unknown Daemon build", "This computer is running a different build of xshell 1.5.0 (unknown build). This Desktop uses 7ab4537a9baf.");

  const excluded: { label: string; patch: Partial<HostStatus> }[] = [
    { label: "current", patch: { status: "connected", updateAction: null, daemonBuild: eligibleStatus(LOCAL_HOST).desktopBuild } },
    { label: "connected despite stale different-build/action fields", patch: { status: "connected" } },
    { label: "offline despite stale fields", patch: { status: "offline" } },
    { label: "reconnecting despite stale fields", patch: { status: "reconnecting" } },
    { label: "missing Daemon version", patch: { daemonVersion: null } },
    { label: "empty Daemon version", patch: { daemonVersion: "" } },
    { label: "preparing", patch: { phase: "preparing" } },
    { label: "upgrading", patch: { phase: "upgrading" } },
    { label: "incompatible older Daemon", patch: { status: "incompatible", incompatibleReason: "daemon-older" } },
  ];
  for (const { label, patch } of excluded) {
    await localStatus(patch);
    expect(banner()?.textContent ?? "", `excluded Local status: ${label}`).toBe("");
    expectNoControls();
  }
  await localStatus({});
  expectLocalNotice("reactive different-build reappearance");
  expect(container.querySelector(`[data-drag-id="${tabId(localTerminal)}"].active`)).not.toBeNull();
  expect(vi.mocked(invoke).mock.calls.filter(([command]) => command === "host_upgrade" || command === "local_daemon_set_persistent")).toEqual([]);
  expect(unexpectedNativeCalls).toEqual([]);
});
