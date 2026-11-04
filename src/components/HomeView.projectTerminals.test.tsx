// @vitest-environment happy-dom
import { act, type ComponentProps } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { HomeView } from "./HomeView";
import type { ProjectInfo, SessionInfo } from "../types";
import type { ProjectTerminal } from "../tabs/projectTerminals";
import { sessionFolderId, sessionKeyOf } from "../hosts/projectKey";
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(), Channel: class {} }));
const project: ProjectInfo = { path: "/p", name: "p", encoded_name: "-p", session_count: 1, last_active: "" };
const pi: SessionInfo = { id: "custom", agent: "pi", title: "Pi conversation", project_name: "p", project_path: "/p", timestamp: "2026-10-18T00:00:00Z", git_branch: "", claude_version: "", message_count: 1, tool_use_count: 0, duration_ms: 0, model: "", context_tokens: 30, context_limit: 0, cost_usd: 1.25, is_authoritative_stats: false, daily_cost: { "2026-10-18": 1.25 }, daily_tokens: { "2026-10-18": [10, 20, 30, 40] }, total_input_tokens: 10, total_cache_creation_tokens: 20, total_cache_read_tokens: 30, total_output_tokens: 40, rate_limit_5h_pct: null, rate_limit_7d_pct: null };
const props: ComponentProps<typeof HomeView> = {
  projects: [project], allProjects: [project], activeCountByProject: new Map(), selectedProject: project, projectIcons: {}, recentSessions: [], projectSessions: [pi], openSessionIds: new Set(), loading: false, sessionsLoading: false, contextTreeEnabled: false,
  showSessionRowMetrics: false, showSessionRowMetricsCodex: false, showSessionRowMetricsOpencode: false, showProjectStatsChart: true, projectStatsView: "cost",
  onChangeProjectStatsView: () => {}, onOpenSession: () => {}, onOpenSessionBackground: () => {}, onSelectProject: () => {}, onNewChat: () => {}, onAddProject: () => {}, onRemoveProject: () => {}, onEditProject: () => {}, onSaveFolders: () => {},
};

let root: Root, container: HTMLDivElement;
beforeEach(() => {
  vi.clearAllMocks();
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  vi.stubGlobal("ResizeObserver", class { observe() {} disconnect() {} });
  container = document.createElement("div"); document.body.append(container); root = createRoot(container);
});
afterEach(() => { act(() => root.unmount()); container.remove(); vi.unstubAllGlobals(); });
const row = (tabId: string, sessionKey: string | null = null): ProjectTerminal => ({ tabId, title: `Open ${tabId}`, detail: "p", terminalKey: tabId, projectKey: '[null,"/p"]', kind: "agent", agent: "pi", state: "live", agentStatus: null, sessionKey });
const focus = vi.fn(), open = vi.fn(), background = vi.fn(), save = vi.fn();
function render(extra: Partial<ComponentProps<typeof HomeView>> = {}) { act(() => root.render(<HomeView {...props} onFocusTerminal={focus} onOpenSession={open} onOpenSessionBackground={background} onSaveFolders={save} {...extra} />)); }
function search(value: string) {
  const input = container.querySelector<HTMLInputElement>(".search-bar input")!;
  act(() => { Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(input, value); input.dispatchEvent(new Event("input", { bubbles: true })); });
}
it("mounted inventory precedes folders/loading with no-history agent and Shell; click focuses only, never resume", () => {
  const rows = [row("unlinked"), { ...row("shell"), kind: "shell" as const, title: "My Shell", state: "last-known" as const }];
  render({ projectTerminals: rows, sessionsLoading: true, loading: true });
  expect(container.querySelectorAll("[data-terminal-tab]")).toHaveLength(2);
  expect(container.textContent!.indexOf("Open unlinked")).toBeLessThan(container.textContent!.indexOf("Past sessions"));
  expect(container.textContent).toContain("Last known");
  act(() => container.querySelector<HTMLButtonElement>('[data-terminal-tab="unlinked"]')!.click());
  expect(focus).toHaveBeenCalledWith("unlinked"); expect(open).not.toHaveBeenCalled(); expect(background).not.toHaveBeenCalled();
  search("offline"); expect(container.querySelectorAll("[data-terminal-tab]")).toHaveLength(1);
  expect(container.querySelector("[data-terminal-tab]")?.getAttribute("data-terminal-tab")).toBe("shell");
});
it("deduplicates every search/folder/ungrouped branch without mutating raw sessions, stats or saved membership; closure reveals original folder", () => {
  const claude = { ...pi, agent: "claude" as const, title: "Claude conversation" };
  const ungrouped = { ...pi, id: "ungrouped", title: "Ungrouped history" };
  const folders = [{ id: "f", name: "Saved folder", collapsed: true, sessionIds: [sessionFolderId(pi)] }];
  const sessions = [pi, claude, ungrouped];
  const extra = { projectSessions: sessions, projectIcons: { "/p": { folders } }, projectTerminals: [row("pi", sessionKeyOf(pi)), row("ungrouped", sessionKeyOf(ungrouped))] };
  render(extra);
  expect(container.querySelectorAll(".session-item")).toHaveLength(1);
  expect(container.querySelector(".folder-count")?.textContent).toBe("0");
  expect(container.textContent).not.toContain("Pi conversation"); expect(container.textContent).not.toContain("Ungrouped history");
  act(() => container.querySelector<HTMLElement>(".folder-header")!.click());
  expect(container.textContent).not.toContain("Pi conversation");
  search("conversation");
  expect(container.querySelectorAll(".session-item")).toHaveLength(1);
  expect(container.textContent).toContain("Claude conversation"); expect(container.textContent).toContain("No open Terminals match");
  search("Open pi"); expect(container.querySelectorAll("[data-terminal-tab]")).toHaveLength(1);
  search("");
  render({ ...extra, projectTerminals: [] });
  expect(container.querySelector(".folder-body")?.textContent).toContain("Pi conversation");
  expect(container.querySelector(".ungrouped-zone")?.textContent).toContain("Ungrouped history");
  expect(folders).toEqual([{ id: "f", name: "Saved folder", collapsed: true, sessionIds: [sessionFolderId(pi)] }]);
  expect(sessions).toEqual([pi, claude, ungrouped]);
  expect(save).toHaveBeenCalledTimes(1); // Only the explicit folder toggle, not dedup/closure.
  expect(save.mock.calls[0][1][0].sessionIds).toEqual([sessionFolderId(pi)]);
});
it("distinct empty states and truthful pending/closing/ended text", () => {
  render({ projectSessions: [] }); expect(container.textContent).toContain("No open Terminals in this Project."); expect(container.textContent).toContain("No past sessions yet.");
  render({ projectTerminals: [ { ...row("pending"), state: "unconfirmed" }, { ...row("ended"), state: "ended" }, { ...row("closing"), state: "closing" }, { ...row("legacy"), state: "status-unavailable" }] });
  expect(container.textContent).toContain("Awaiting Host confirmation"); expect(container.textContent).toContain("Ended"); expect(container.textContent).toContain("Closing…"); expect(container.textContent).toContain("Process status unavailable");
});
it.each([false, true])("search copy covers open linked Terminals and past sessions while history loading=%s", sessionsLoading => {
  render({ projectTerminals: [row("linked", sessionKeyOf(pi))], projectSessions: [pi], sessionsLoading });
  const input = container.querySelector<HTMLInputElement>(".search-bar input")!;
  expect(input.placeholder).toBe("Search open Terminals and past sessions…");
  expect(container.querySelectorAll("[data-terminal-tab]")).toHaveLength(1);
  expect(container.querySelectorAll(".session-item")).toHaveLength(0);
  expect(container.textContent).not.toContain("Start a new chat to begin.");
  if (!sessionsLoading) expect(container.textContent).toContain("No past sessions yet.");
  search("Open linked");
  expect(container.querySelectorAll("[data-terminal-tab]")).toHaveLength(1);
});
it("does not suggest starting a chat when an open row exists but history is empty", () => {
  render({ projectTerminals: [row("linked", sessionKeyOf(pi))], projectSessions: [pi] });
  expect(container.textContent).toContain("No past sessions yet.");
  expect(container.textContent).not.toContain("Start a new chat to begin.");
  render({ projectTerminals: [], projectSessions: [] });
  expect(container.textContent).toContain("Start a new chat to begin.");
});
