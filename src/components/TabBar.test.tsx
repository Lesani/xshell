// @vitest-environment happy-dom
import { act, useSyncExternalStore } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { RegistrySnapshot } from "../hosts/registry";
import type { HostStatus } from "../hosts/types";
import type { Tab } from "../types";
import { TabBar } from "./TabBar";
import { AGENT_IDS, type AgentId } from "../agents";
import { invoke } from "@tauri-apps/api/core";

const store = vi.hoisted(() => ({ snap: null as unknown as RegistrySnapshot, listeners: new Set<() => void>() }));
vi.mock("../hosts/registry", async original => ({ ...await original<typeof import("../hosts/registry")>(), registry: {
  getSnapshot: () => store.snap, subscribe: (listener: () => void) => { store.listeners.add(listener); return () => { store.listeners.delete(listener); }; }, getStatus: (host: string) => store.snap.status[host],
} }));
vi.mock("../hosts/useHosts", () => ({ useHostsSnapshot: () => useSyncExternalStore(listener => { store.listeners.add(listener); return () => { store.listeners.delete(listener); }; }, () => store.snap) }));
vi.mock("../tabs/localAgentStatus", () => ({ useLocalAgentStatuses: () => new Map() }));
vi.mock("@tauri-apps/api/window", () => ({ getCurrentWindow: () => ({}) }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(), Channel: class {} }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
const status: HostStatus = { host: "local", status: "connected", phase: null, lastError: null, errorHint: null, daemonVersion: "1.5.0", desktopVersion: "1.5.0", protocol: 1, os: "linux", arch: "x86_64", incompatibleReason: null, nextRetryAt: null, sinceMs: 0, configGeneration: 2, daemonCapabilities: ["term.custom-title"] };
let root: Root, container: HTMLDivElement;
let tabs: Tab[], props: Parameters<typeof TabBar>[0];
beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  vi.stubGlobal("ResizeObserver", class { observe() {} disconnect() {} });
  Element.prototype.scrollIntoView = vi.fn();
  vi.mocked(invoke).mockReset().mockResolvedValue(undefined);
  tabs = [{ id: "tab", type: "terminal", title: "Auto agent", customTitle: "Shared agent", projectName: "Project", projectPath: "/p", terminal: "uuid" }, { id: "shell", type: "terminal", title: "Bash", customTitle: "Shared shell", shellMode: "raw", terminal: "shell-uuid", projectName: "Project", projectPath: "/p" }];
  store.snap = { configs: [], status: { local: { ...status } }, live: { local: tabs.map(t => ({ terminal: t.terminal!, meta: { title: t.title, customTitle: t.customTitle }, spec: { cwd: "/p" }, createdAtMs: 0, pid: 1, exitCode: null })) }, agents: {}, epochs: {} };
  container = document.createElement("div"); document.body.append(container); root = createRoot(container);
  props = { tabs, entries: tabs.map(tab => ({ kind: "tab", id: tab.id, tab })), closingTabIds: new Set(), activeTabId: "tab", selectedProject: null, hoveredProjectKey: null, linkedProjectKey: null, activeTabProject: null, openSessionIds: new Set(), projectIcons: {}, pinnedProjects: [], sidebarCollapsed: false, defaultShell: "bash", updateAvailable: false, installedAgents: Object.fromEntries(AGENT_IDS.map(a => [a, true])) as Record<AgentId, boolean>, onExpandSidebar: vi.fn(), onSelectTab: vi.fn(), onCloseTab: vi.fn(), onReorderTabs: vi.fn(), onNewChat: vi.fn(), onNewChatInActive: vi.fn(), onNewShellInContext: vi.fn(), onOpenSession: vi.fn(), onNewShell: vi.fn(), onRenameGroup: vi.fn(), onGoHome: vi.fn(), onOpenSettings: vi.fn(), onToggleSidebar: vi.fn() };
});
afterEach(() => { act(() => root.unmount()); container.remove(); store.listeners.clear(); vi.unstubAllGlobals(); });
const render = () => act(() => root.render(<TabBar {...props} />));
const tabEl = (id = "tab") => container.querySelector<HTMLElement>(`[data-drag-id="${id}"]`)!;
const button = (label: string) => Array.from(container.querySelectorAll("button")).find(b => b.textContent === label)!;
const context = () => act(() => { tabEl().dispatchEvent(new MouseEvent("contextmenu", { button: 2, clientX: 20, clientY: 30, bubbles: true, cancelable: true })); });
function replaceStatus(change: Partial<HostStatus>) {
  act(() => { store.snap = { ...store.snap, status: { local: { ...store.snap.status.local, ...change } } }; for (const l of store.listeners) l(); });
}

describe("TabBar shared names", () => {
  it("custom_name_shows_in_shell_and_agent_tab_tooltips_and_accessible_names", () => {
    render();
    expect(tabEl().querySelector(".tab-item-title")?.textContent).toBe("Shared agent");
    expect(tabEl("shell").querySelector(".tab-item-title")?.textContent).toBe("Shared shell");
    expect(tabEl("shell").getAttribute("aria-label")).toContain("Shared shell — Project · Bash");
    act(() => tabEl("shell").dispatchEvent(new MouseEvent("mouseover", { bubbles: true })));
    expect(container.querySelector(".tab-tooltip")?.textContent).toBe("Shared shell — Project · Bash");
  });
  it("normal_tab_context_menu_does_not_rename_group_or_start_drag_and_outside_returns_focus", () => {
    render();
    act(() => {
      tabEl().dispatchEvent(new PointerEvent("pointerdown", { button: 2, bubbles: true }));
      window.dispatchEvent(new PointerEvent("pointermove", { clientX: 100, clientY: 20 }));
      window.dispatchEvent(new PointerEvent("pointerup"));
    });
    expect(props.onReorderTabs).not.toHaveBeenCalled(); expect(container.querySelector(".tab-dragging")).toBeNull();
    context(); expect(container.querySelector("[role=menu]")?.getAttribute("aria-label")).toBe("Tab name actions");
    expect(document.activeElement).toBe(button("Rename Tab…")); expect(props.onRenameGroup).not.toHaveBeenCalled();
    act(() => document.body.dispatchEvent(new PointerEvent("pointerdown", { bubbles: true })));
    expect(container.querySelector("[role=menu]")).toBeNull(); expect(document.activeElement).toBe(tabEl());
  });
  it("keyboard_menu_arrows_escape_and_native_button_activation_open_dialog", () => {
    render(); tabEl().focus();
    act(() => tabEl().dispatchEvent(new KeyboardEvent("keydown", { key: "F10", shiftKey: true, bubbles: true, cancelable: true })));
    act(() => button("Rename Tab…").dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true, cancelable: true })));
    expect(document.activeElement).toBe(button("Use automatic name"));
    act(() => button("Use automatic name").dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowUp", bubbles: true, cancelable: true })));
    expect(document.activeElement).toBe(button("Rename Tab…"));
    act(() => button("Rename Tab…").dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true })));
    expect(container.querySelector("[role=menu]")).toBeNull(); expect(document.activeElement).toBe(tabEl());
    context(); expect(button("Rename Tab…").tagName).toBe("BUTTON"); act(() => button("Rename Tab…").click());
    expect(container.querySelector("input")!.value).toBe("Shared agent");
    act(() => button("Cancel").click()); expect(document.activeElement).toBe(tabEl()); expect(invoke).not.toHaveBeenCalled();
  });
  it.each([
    { userAgent: "Linux", modifier: "ctrlKey" },
    { userAgent: "Macintosh", modifier: "metaKey" },
  ] as const)("Quick_Actions_shortcut_preserves_naming_focus_and_resumes_after_close ($modifier)", ({ userAgent, modifier }) => {
    vi.stubGlobal("navigator", { userAgent });
    const shortcut = (target: Element) => act(() => {
      target.dispatchEvent(new KeyboardEvent("keydown", { key: "k", [modifier]: true, bubbles: true, cancelable: true }));
    });
    const escape = (target: Element) => act(() => {
      target.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true, cancelable: true }));
    });
    render(); context(); act(() => button("Rename Tab…").click());
    const renameInput = container.querySelector<HTMLInputElement>("[role=dialog] input")!;
    expect(document.activeElement).toBe(renameInput);
    shortcut(renameInput);
    expect(container.querySelector(".ts-overlay")).toBeNull();
    expect(document.activeElement).toBe(renameInput);
    expect(renameInput.value).toBe("Shared agent");
    act(() => button("Cancel").click());
    shortcut(tabEl());
    let quickInput = container.querySelector<HTMLInputElement>(".ts-search-input")!;
    expect(quickInput).not.toBeNull(); expect(document.activeElement).toBe(quickInput);
    escape(quickInput);
    context();
    const menuItem = button("Rename Tab…");
    expect(document.activeElement).toBe(menuItem);
    shortcut(menuItem);
    expect(container.querySelector(".ts-overlay")).toBeNull();
    expect(document.activeElement).toBe(menuItem);
    escape(menuItem);
    expect(container.querySelector("[role=menu]")).toBeNull();
    shortcut(tabEl());
    quickInput = container.querySelector<HTMLInputElement>(".ts-search-input")!;
    expect(quickInput).not.toBeNull(); expect(document.activeElement).toBe(quickInput);
    expect(invoke).not.toHaveBeenCalled();
  });
  it("disabled_rename_explains_old_daemon_and_offline_without_local_success", () => {
    render(); replaceStatus({ daemonCapabilities: [] }); context();
    expect(button("Rename Tab…").disabled).toBe(true);
    expect(container.querySelector("[role=status]")?.textContent).toContain("Update xshell on this Host");
    replaceStatus({ status: "offline" });
    expect(container.querySelector("[role=status]")?.textContent).toContain("Connect to this Host");
    expect(tabEl().textContent).toContain("Shared agent"); expect(invoke).not.toHaveBeenCalled();
  });
  it("clear_is_explicit_pending_then_authoritative_observation_and_indeterminate_error", async () => {
    let reject!: (reason: unknown) => void;
    vi.mocked(invoke).mockReturnValue(new Promise((_, no) => { reject = no; }));
    render(); context(); act(() => button("Use automatic name").click());
    expect(invoke).toHaveBeenCalledExactlyOnceWith("host_term_update", { host: "local", terminal: "uuid", meta: { customTitle: null } });
    expect(container.querySelector("[role=status]")?.textContent).toBe("Restoring automatic name…");
    expect(tabEl().textContent).toContain("Shared agent");
    // A list may show a changed name before a response, but cannot certify the save.
    props = { ...props, tabs: [{ ...tabs[0], customTitle: undefined, title: "Latest automatic" }, tabs[1]] };
    props.entries = props.tabs.map(tab => ({ kind: "tab", id: tab.id, tab })); render();
    await act(async () => { reject({ code: "indeterminate", message: "directory sync" }); });
    expect(container.querySelector("[role=alert]")?.textContent).toContain("Couldn’t confirm");
    expect(tabEl().textContent).toContain("Latest automatic");
    expect(button("Use automatic name").disabled).toBe(false); // deliberate idempotent retry, even after observed clear
    expect(container.querySelector("input")!.value).toBe("Shared agent");
  });
  it("menu_captures_target_and_dismisses_on_reconfiguration_before_any_command", () => {
    render(); context();
    replaceStatus({ configGeneration: 3 });
    expect(container.querySelector("[role=menu]")).toBeNull();
    expect(container.querySelector("[role=dialog]")).toBeNull(); expect(invoke).not.toHaveBeenCalled();
  });
  it("clear_is_disabled_without_an_override_and_rejected_clear_can_retry_even_after_observed_removal", async () => {
    props.tabs = [{ ...tabs[0], customTitle: undefined }, tabs[1]];
    props.entries = props.tabs.map(tab => ({ kind: "tab", id: tab.id, tab })); render(); context();
    expect(button("Use automatic name").disabled).toBe(true);
    act(() => button("Rename Tab…").click());
    expect(container.querySelector("input")!.value).toBe("Auto agent");
    act(() => button("Cancel").click());
    props.tabs = tabs; props.entries = tabs.map(tab => ({ kind: "tab", id: tab.id, tab })); render(); context();
    vi.mocked(invoke).mockRejectedValueOnce({ code: "indeterminate", message: "sync" });
    await act(async () => button("Use automatic name").click());
    props.tabs = [{ ...tabs[0], customTitle: undefined }, tabs[1]];
    props.entries = props.tabs.map(tab => ({ kind: "tab", id: tab.id, tab })); render();
    expect(button("Use automatic name").disabled).toBe(false);
    await act(async () => button("Use automatic name").click());
    expect(invoke).toHaveBeenCalledTimes(2);
    expect(vi.mocked(invoke).mock.calls[1]).toEqual(["host_term_update", { host: "local", terminal: "uuid", meta: { customTitle: null } }]);
    expect(container.querySelector("[role=dialog]")).toBeNull();
  });
  it("closing_captured_target_dismisses_dialog_and_late_failure_cannot_reopen_it", async () => {
    let reject!: (reason: unknown) => void;
    vi.mocked(invoke).mockReturnValue(new Promise((_, no) => { reject = no; }));
    render(); context(); act(() => button("Rename Tab…").click());
    act(() => container.querySelector("form")!.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true })));
    props = { ...props, tabs: [tabs[1]], entries: [{ kind: "tab", id: tabs[1].id, tab: tabs[1] }], activeTabId: "shell" }; render();
    expect(container.querySelector("[role=dialog]")).toBeNull();
    await act(async () => reject({ code: "remote", message: "late" }));
    expect(container.querySelector("[role=dialog]")).toBeNull(); expect(tabEl("shell").textContent).toContain("Shared shell");
  });
  it("group_right_click_still_edits_personal_group_and_home_has_no_name_menu", () => {
    props = { ...props, entries: [{ kind: "group", id: "g", group: { id: "g", name: "Personal", layout: { kind: "leaf", tabId: "tab" } } }, { kind: "tab", id: "home", tab: { id: "home", type: "home", title: "Home" } }] };
    render();
    act(() => container.querySelector(".tab-group-item")!.dispatchEvent(new MouseEvent("contextmenu", { bubbles: true, cancelable: true })));
    expect(container.querySelector(".tab-group-rename-input")).not.toBeNull(); expect(container.querySelector("[role=menu]")).toBeNull();
    act(() => tabEl("home").dispatchEvent(new MouseEvent("contextmenu", { bubbles: true, cancelable: true })));
    expect(container.querySelector("[role=menu]")).toBeNull();
  });
});

describe("TabBar Pi leading-icon status", () => {
  function workingPi() {
    tabs[0].agent = "pi";
    store.snap.status.local.daemonCapabilities = ["agent.status"];
    store.snap.live.local![0].agentStatus = "working";
  }
  it.each(["initials", "emoji", "image", "dot"])("animates the actual %s identity when selected and inactive", async kind => {
    workingPi();
    if (kind === "dot") tabs[0].projectName = "";
    if (kind === "emoji") props.projectIcons = { "/p": { icon: "🦀", color: "#123456" } };
    if (kind === "image") {
      props.projectIcons = { "/p": { icon: "img:/private/project.png" } };
      vi.mocked(invoke).mockResolvedValue("data:image/png;base64,aGVsbG8=");
    }
    await act(async () => { root.render(<TabBar {...props} />); });
    const selector = kind === "dot" ? ".tab-dot" : ".tab-project-icon";
    const icon = tabEl().querySelector(selector)!;
    expect(icon.classList.contains("tab-pi-working")).toBe(true);
    expect(tabEl().querySelector(".tab-item-text")?.classList.contains("tab-pi-working")).toBe(false);
    if (kind === "initials") expect(icon.textContent).toBe("PR");
    if (kind === "emoji") expect(icon.textContent).toBe("🦀");
    if (kind === "image") expect(icon.querySelector("img")?.getAttribute("src")).toBe("data:image/png;base64,aGVsbG8=");
    expect(tabEl().querySelector('[aria-label="Agent status: Working"]')).not.toBeNull();
    props.activeTabId = "shell"; render();
    expect(tabEl().classList.contains("active")).toBe(false);
    expect(tabEl().querySelector(selector)?.classList.contains("tab-pi-working")).toBe(true);
    expect(tabEl("shell").querySelector(".tab-pi-working")).toBeNull();
    for (const status of ["finished", "ended", undefined] as const) {
      store.snap.live.local![0].agentStatus = status; render();
      expect(tabEl().querySelector(".tab-pi-working")).toBeNull();
    }
  });
  it("does not aggregate a working Pi into the group icon", () => {
    workingPi();
    props.entries = [{ kind: "group", id: "g", group: { id: "g", name: "Group", layout: { kind: "leaf", tabId: "tab" } } }];
    render(); expect(container.querySelector(".tab-pi-working")).toBeNull();
    expect(container.querySelector(".tab-group-icon")).not.toBeNull();
  });
  it("stale, closing, raw and other agents never animate", () => {
    workingPi(); render(); expect(tabEl().querySelector(".tab-pi-working")).not.toBeNull();
    replaceStatus({ status: "reconnecting" }); expect(tabEl().querySelector(".tab-pi-working")).toBeNull();
    expect(tabEl().getAttribute("aria-label")).toContain("Working (last known)");
    replaceStatus({ status: "connected" }); props.closingTabIds.add("tab"); render();
    expect(tabEl().querySelector(".tab-pi-working")).toBeNull(); props.closingTabIds.clear();
    for (const agent of ["claude", "codex"] as const) { tabs[0].agent = agent; render(); expect(tabEl().querySelector(".tab-pi-working")).toBeNull(); }
    tabs[0].agent = "pi"; tabs[0].shellMode = "raw"; render(); expect(tabEl().querySelector(".tab-pi-working")).toBeNull();
  });
});
