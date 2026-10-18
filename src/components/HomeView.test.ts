import { createElement, type ComponentProps } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { expect, it, vi } from "vitest";
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(), Channel: class {} }));
import { HomeView } from "./HomeView";
import { sessionFolderId } from "../hosts/projectKey";
import type { ProjectInfo, SessionInfo } from "../types";

const project: ProjectInfo = { path: "/p", name: "p", encoded_name: "-p", session_count: 1, last_active: "" };
const pi: SessionInfo = { id: "custom", agent: "pi", title: "Pi conversation", project_name: "p", project_path: "/p", timestamp: "2026-10-18T00:00:00Z", git_branch: "", claude_version: "", message_count: 1, tool_use_count: 0, duration_ms: 0, model: "", context_tokens: 30, context_limit: 0, cost_usd: 1.25, is_authoritative_stats: false, daily_cost: { "2026-10-18": 1.25 }, daily_tokens: { "2026-10-18": [10, 20, 30, 40] }, total_input_tokens: 10, total_cache_creation_tokens: 20, total_cache_read_tokens: 30, total_output_tokens: 40, rate_limit_5h_pct: null, rate_limit_7d_pct: null };
const props: ComponentProps<typeof HomeView> = {
  projects: [project], allProjects: [project], activeCountByProject: new Map(), selectedProject: project, projectIcons: {}, recentSessions: [], projectSessions: [pi], openSessionIds: new Set(), loading: false, sessionsLoading: false, contextTreeEnabled: false,
  showSessionRowMetrics: false, showSessionRowMetricsCodex: false, showSessionRowMetricsOpencode: false, showProjectStatsChart: true, projectStatsView: "cost",
  onChangeProjectStatsView: () => {}, onOpenSession: () => {}, onOpenSessionBackground: () => {}, onSelectProject: () => {}, onNewChat: () => {}, onAddProject: () => {}, onRemoveProject: () => {}, onEditProject: () => {}, onSaveFolders: () => {},
};

it("HomeView shows recorded Pi cost without Claude hook authority or fabricated context percentages", () => {
  const html = renderToStaticMarkup(createElement(HomeView, props));
  expect(html).toContain("Recorded cost (Pi)");
  expect(html).toContain("session-cost");
  expect(html).not.toContain("session-metric-ctx");
  expect(html).not.toContain("Connect Claude Code");
});

it("HomeView honors the existing cost chart preference for Pi", () => {
  const html = renderToStaticMarkup(createElement(HomeView, { ...props, showProjectStatsChart: false }));
  expect(html).toContain("Daily tokens");
  expect(html).not.toContain("Recorded cost (Pi)");
});

it("Pi and Claude sharing a raw ID stay distinct in project folders and history", () => {
  const claude: SessionInfo = { ...pi, agent: "claude", title: "Claude conversation", cost_usd: 0, daily_cost: {} };
  const html = renderToStaticMarkup(createElement(HomeView, { ...props, projectSessions: [claude, pi], projectIcons: { "/p": { folders: [{ id: "f", name: "Pi folder", collapsed: false, sessionIds: [sessionFolderId(pi)] }] } } }));
  expect(html.match(/Pi conversation/g)).toHaveLength(1);
  expect(html.match(/Claude conversation/g)).toHaveLength(1);
});
