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
vi.mock("../tabs/localAgentStatus", () => ({ useLocalAgentStatuses: () => new Map(), localAgentStatus: { completionOf: () => undefined } }));
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

describe("TabBar stationary leading-icon attention", () => {
  function working(agent = "pi") {
    tabs[0].agent = agent as Tab["agent"];
    store.snap.status.local.daemonCapabilities = ["agent.status", "term.completion-attention"];
    store.snap.live.local![0].agentStatus = "working";
  }
  it.each(["initials", "emoji", "image", "dot"])("rotates only the outer ring of %s identity, selected or inactive", async kind => {
    working();
    if (kind === "dot") tabs[0].projectName = "";
    if (kind === "emoji") props.projectIcons = { "/p": { icon: "🦀", color: "#123456" } };
    if (kind === "image") {
      props.projectIcons = { "/p": { icon: "img:/private/project.png" } };
      vi.mocked(invoke).mockResolvedValue("data:image/png;base64,aGVsbG8=");
    }
    await act(async () => { root.render(<TabBar {...props} />); });
    const icon = tabEl().querySelector(kind === "dot" ? ".tab-dot" : ".tab-project-icon")!;
    expect(icon.parentElement?.classList.contains("tab-attention-working")).toBe(true);
    expect(icon.className).not.toMatch(/working|linked/);
    expect(tabEl().getAttribute("aria-label")).toContain("Working");
    if (kind === "initials") expect(icon.textContent).toBe("PR");
    if (kind === "emoji") expect(icon.textContent).toBe("🦀");
    if (kind === "image") expect(icon.querySelector("img")?.getAttribute("src")).toBe("data:image/png;base64,aGVsbG8=");
    props.activeTabId = "shell"; render();
    expect(tabEl().classList.contains("active")).toBe(false);
    expect(tabEl().querySelector(".tab-attention-working")).not.toBeNull();
    expect(tabEl("shell").querySelector(".tab-attention-working")).toBeNull();
  });
  it.each(["claude", "codex", "pi"])("%s working ignores read/handled; finished unread is exact; needs-you is static", agent => {
    working(agent);
    const id = "00000000-0000-4000-8000-000000000001";
    const info = store.snap.live.local![0];
    info.completionAttention = { completionId: id, inputHandled: true };
    render(); expect(tabEl().querySelector(".tab-attention-working")).not.toBeNull();
    info.agentStatus = "finished"; render(); expect(tabEl().querySelector(".tab-attention-unread")).toBeNull();
    info.completionAttention.inputHandled = false; render(); expect(tabEl().querySelector(".tab-attention-unread")).not.toBeNull();
    props.completionSeen = { [JSON.stringify(["daemon", "local", "uuid"])]: id }; render();
    expect(tabEl().querySelector(".tab-attention-unread")).toBeNull();
    props.completionSeen = {}; props.viewingIds = new Set(["tab"]); render();
    expect(tabEl().querySelector(".tab-attention-unread")).toBeNull();
    info.agentStatus = "needs-you"; render(); expect(tabEl().querySelector(".tab-attention-needs-you")).not.toBeNull();
    expect(tabEl().getAttribute("aria-label")).toContain("Needs you");
    info.agentStatus = "ended"; render(); expect(tabEl().querySelector(".tab-attention-needs-you")).toBeNull();
  });
  it("never animates stale, closing, exited or raw; preserves truthful labels and permission tooltip without decorations", () => {
    working(); tabs[0].skipPermissions = true; render();
    expect(tabEl().getAttribute("aria-label")).toContain("permission");
    replaceStatus({ status: "reconnecting" }); expect(tabEl().querySelector(".tab-attention-working")).toBeNull();
    expect(tabEl().getAttribute("aria-label")).toContain("Working (last known)");
    replaceStatus({ status: "connected" }); props.closingTabIds.add("tab"); render();
    expect(tabEl().querySelector(".tab-attention-working")).toBeNull(); props.closingTabIds.clear();
    store.snap.live.local![0].exitCode = 0; store.snap.live.local![0].agentStatus = "ended"; render();
    expect(tabEl().getAttribute("aria-label")).toContain("Ended"); expect(tabEl().querySelector(".tab-attention-working")).toBeNull();
    tabs[0].shellMode = "raw"; render(); expect(tabEl().querySelector(".tab-attention-working")).toBeNull();
    props.entries = [{ kind: "group", id: "g", group: { id: "g", name: "Group", layout: { kind: "leaf", tabId: "tab" } } }];
    render(); expect(container.querySelector(".tab-group-icon")).not.toBeNull();
    expect(container.querySelector(".tab-skip-perms-icon, .tab-agent-status, .tab-project-icon-linked, .tab-pi-working")).toBeNull();
  });
});

describe("Project ordering pointer gaps", () => {
  const rect = { left: 300, top: 0, width: 100, height: 30, right: 400, bottom: 30, x: 300, y: 0, toJSON() {} };
  function fixture() {
    tabs = [
      { id: "a", type: "terminal", title: "a", projectPath: "/p" },
      { id: "b", type: "terminal", title: "b", projectPath: "/p" },
      { id: "c", type: "terminal", title: "c", projectPath: "/q" },
      { id: "d", type: "terminal", title: "d", projectPath: "/q" },
    ];
    props = { ...props, tabs, entries: tabs.map(tab => ({ kind: "tab", id: tab.id, tab })), groupTabsByProject: true };
    render();
  }
  function down(id: string) { act(() => { tabEl(id).dispatchEvent(new PointerEvent("pointerdown", { button: 0, bubbles: true, clientX: 10, clientY: 10 })); }); }
  function move(target: string, x: number) {
    vi.spyOn(tabEl(target), "getBoundingClientRect").mockReturnValue(rect);
    document.elementFromPoint = vi.fn(() => tabEl(target));
    act(() => { window.dispatchEvent(new PointerEvent("pointermove", { clientX: x, clientY: 10 })); });
  }
  function up(type = "pointerup") { act(() => { window.dispatchEvent(new PointerEvent(type)); }); }
  function result() { const calls = vi.mocked(props.onReorderTabs).mock.calls; return calls[calls.length - 1]?.[0].map(t => t.id); }
  it.each([
    ["a", "b", 390, ["b", "a", "c", "d"]],
    ["a", "c", 390, ["c", "d", "a", "b"]],
    ["d", "a", 310, ["c", "d", "a", "b"]],
    ["c", "a", 390, ["a", "b", "c", "d"]],
  ])("pointer midpoint/source-removal from %s to %s (%s)", (source, target, x, expected) => {
    fixture(); down(source as string); move(target as string, x as number); up(); expect(result()).toEqual(expected);
  });
  it("previews the actual whole-block destination rather than a gap inside the target Project", () => {
    fixture(); down("a"); move("c", 390);
    expect(tabEl("d").querySelector(".tab-drop-line-after")).not.toBeNull();
    expect(tabEl("c").querySelector(".tab-drop-line-after")).toBeNull();
    expect(tabEl("d").querySelector(".tab-drop-line-before")).toBeNull();
    up(); expect(result()).toEqual(["c", "d", "a", "b"]);
    down("d"); move("b", 310);
    expect(tabEl("a").querySelector(".tab-drop-line-before")).not.toBeNull();
    expect(tabEl("a").querySelector(".tab-drop-line-after")).toBeNull();
    expect(tabEl("b").querySelector(".tab-drop-line-before")).toBeNull();
    up(); expect(result()).toEqual(["c", "d", "a", "b"]);
  });
  it("keeps raw entry-gap previews for off-mode and within-block moves", () => {
    fixture(); props = { ...props, groupTabsByProject: false }; render();
    down("a"); move("c", 390);
    expect(tabEl("c").querySelector(".tab-drop-line-after")).not.toBeNull();
    expect(tabEl("d").querySelector(".tab-drop-line-before")).not.toBeNull();
    expect(tabEl("d").querySelector(".tab-drop-line-after")).toBeNull();
    up(); expect(result()).toEqual(["b", "c", "a", "d"]);
    props = { ...props, groupTabsByProject: true }; render();
    down("a"); move("b", 390);
    expect(tabEl("b").querySelector(".tab-drop-line-after")).not.toBeNull();
    expect(tabEl("c").querySelector(".tab-drop-line-before")).not.toBeNull();
    up(); expect(result()).toEqual(["b", "a", "c", "d"]);
  });
  it("off-mode moves just one atomic entry; pointercancel and changed entries cancel; outside clears a prior gap", () => {
    fixture(); props = { ...props, groupTabsByProject: false }; render();
    down("a"); move("c", 390); up(); expect(result()).toEqual(["b", "c", "a", "d"]);
    vi.mocked(props.onReorderTabs).mockClear();
    down("a"); move("c", 390); up("pointercancel"); expect(props.onReorderTabs).not.toHaveBeenCalled();
    down("a"); move("c", 390);
    props = { ...props, entries: props.entries.slice(1), tabs: tabs.slice(1) }; render(); up();
    expect(props.onReorderTabs).not.toHaveBeenCalled(); expect(container.querySelector(".tab-dragging")).toBeNull();
    down("b"); move("c", 390); document.elementFromPoint = vi.fn(() => null);
    act(() => { window.dispatchEvent(new PointerEvent("pointermove", { clientX: 500, clientY: 500 })); }); up();
    expect(props.onReorderTabs).not.toHaveBeenCalled();
  });
  it("mixed Group is an opaque entry, not a leaf-index source; single-Project Group moves with its block", async () => {
    const { tabBarEntries } = await import("../tabs/projectOrder");
    fixture();
    tabs = [...tabs, { id: "x", type: "terminal", title: "x", projectPath: "/q", groupId: "mix" }, { id: "y", type: "terminal", title: "y", projectPath: "/p", groupId: "mix" }, { id: "p1", type: "terminal", title: "p1", projectPath: "/p", groupId: "pure" }, { id: "p2", type: "terminal", title: "p2", projectPath: "/p", groupId: "pure" }];
    const group = (id: string, a: string, b: string): import("../types").Group => ({ id, name: id, layout: { kind: "split", direction: "col", ratio: .5, children: [{ kind: "leaf", tabId: a }, { kind: "leaf", tabId: b }] } });
    const { normalizeProjectTabs } = await import("../tabs/projectOrder");
    const groups = [group("mix", "x", "y"), group("pure", "p1", "p2")];
    tabs = normalizeProjectTabs(tabs, groups);
    props = { ...props, tabs, entries: tabBarEntries(tabs, groups) }; render();
    expect([...container.querySelectorAll(".tab-item[data-idx]")].map(e => e.getAttribute("data-idx"))).toEqual(["0", "1", "2", "3", "4", "5"]);
    down("pure"); move("d", 390); up();
    expect(result()).toEqual(["c", "d", "a", "b", "p1", "p2", "x", "y"]);
    vi.mocked(props.onReorderTabs).mockClear(); down("mix"); move("a", 310); up();
    expect(result()).toEqual(["x", "y", "a", "b", "p1", "p2", "c", "d"]);
  });
});
