// @vitest-environment happy-dom
import { act, type ComponentProps } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { TabBar } from "./TabBar";
import { Sidebar } from "./Sidebar";
import { HomeView } from "./HomeView";
import { ProjectEditorDialog } from "./ProjectEditorDialog";
import { AGENT_IDS, type AgentId } from "../agents";
import { asProjectKey } from "../hosts/projectKey";
import type { ProjectInfo, ProjectSettings, Tab } from "../types";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(async (command: string) => command === "read_image_base64" ? "data:image/png;base64,fixture" : undefined), Channel: class {} }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
vi.mock("@tauri-apps/api/window", () => ({ getCurrentWindow: () => ({}) }));
vi.mock("../tabs/localAgentStatus", () => ({ useLocalAgentStatuses: () => new Map() }));

const project: ProjectInfo = { name: "Project Name", path: "/p", encoded_name: "-p", session_count: 0, last_active: "" };
const tab: Tab = { id: "t", type: "terminal", title: "Terminal", projectName: project.name, projectPath: project.path };
const noop = () => {};
const homeProps: ComponentProps<typeof HomeView> = {
  projects: [project], allProjects: [project], activeCountByProject: new Map(), selectedProject: null, projectIcons: {}, recentSessions: [], projectSessions: [], openSessionIds: new Set(), loading: false, sessionsLoading: false, contextTreeEnabled: false,
  showSessionRowMetrics: false, showSessionRowMetricsCodex: false, showSessionRowMetricsOpencode: false, showProjectStatsChart: false, projectStatsView: "cost",
  onChangeProjectStatsView: noop, onOpenSession: noop, onOpenSessionBackground: noop, onSelectProject: noop, onNewChat: noop, onAddProject: noop, onRemoveProject: noop, onEditProject: noop, onSaveFolders: noop,
};
let root: Root, container: HTMLDivElement;
beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  vi.stubGlobal("ResizeObserver", class { observe() {} disconnect() {} });
  container = document.createElement("div"); document.body.append(container); root = createRoot(container);
});
afterEach(async () => { await act(async () => { root.unmount(); }); container.remove(); document.documentElement.removeAttribute("data-theme"); vi.unstubAllGlobals(); });

async function renderIcons(settings: ProjectSettings) {
  const projectIcons = { "/p": settings };
  await act(async () => { root.render(<>
    <section data-boundary="tab"><TabBar tabs={[tab]} entries={[{ kind: "tab", id: tab.id, tab }]} closingTabIds={new Set()} activeTabId="t" selectedProject={null} hoveredProjectKey={null} linkedProjectKey={asProjectKey("/p")} activeTabProject={project} openSessionIds={new Set()} projectIcons={projectIcons} pinnedProjects={[project]} sidebarCollapsed={false} defaultShell="bash" updateAvailable={false} installedAgents={Object.fromEntries(AGENT_IDS.map(a => [a, false])) as Record<AgentId, boolean>} onExpandSidebar={noop} onSelectTab={noop} onCloseTab={noop} onReorderTabs={noop} onNewChat={noop} onNewChatInActive={noop} onNewShellInContext={noop} onOpenSession={noop} onNewShell={noop} onRenameGroup={noop} onGoHome={noop} onOpenSettings={noop} onToggleSidebar={noop} /></section>
    <section data-boundary="sidebar"><Sidebar projects={[project]} projectIcons={projectIcons} selectedProject={project} activeCountByProject={new Map()} sidebarLayout={[{ kind: "folder", id: "folder", name: "Folder", collapsed: true, projectPaths: [asProjectKey("/p")] }]} onLayoutChange={noop} onSelectProject={noop} onGoHome={noop} onRemoveProject={noop} onEditProject={noop} onHoverProject={noop} onOpenSettings={noop} onAddProject={noop} onCollapse={noop} activeTabId="t" linkedProjectKey={asProjectKey("/p")} showRateLimit={false} showRateLimitCodex={false} updateAvailable={false} /></section>
    <section data-boundary="home"><HomeView {...homeProps} projectIcons={projectIcons} /></section>
    <section data-boundary="detail"><HomeView {...homeProps} projectIcons={projectIcons} selectedProject={project} /></section>
    <section data-boundary="editor"><ProjectEditorDialog key={JSON.stringify(settings)} project={project} settings={settings} onSave={noop} onClose={noop} /></section>
  </>); });
}
function textBoundaries() {
  const selectors = [
    '[data-boundary="tab"] .tab-project-icon',
    '[data-boundary="sidebar"] .ds-folder-header .ds-project.ds-icon-mini',
    '[data-boundary="sidebar"] .ds-folder-body .ds-project',
    '[data-boundary="home"] .project-pill > div',
    '[data-boundary="detail"] div[style*="width: 40px"]',
    '[data-boundary="editor"] div[style*="width: 56px"]',
  ];
  return selectors.map(selector => {
    const icon = container.querySelector<HTMLElement>(selector);
    expect(icon, selector).not.toBeNull(); return icon!;
  });
}

describe.each(["light", "dark"])("mounted Project icon foreground (%s)", theme => {
  it.each([["#ffffff", "#000000"], ["#ffff00", "#000000"], ["#808080", "#000000"], ["#123456", "#ffffff"]])("applies better contrast for %s to every real text/initials boundary, including Sidebar mini previews", async (color, expected) => {
    document.documentElement.setAttribute("data-theme", theme);
    for (const icon of ["AB", undefined]) {
      await renderIcons({ color, icon });
      for (const boundary of textBoundaries()) expect(boundary.style.color).toBe(expected);
      // Existing selected/linked ring classes remain untouched in this slice.
      expect(container.querySelector(".tab-project-icon-linked")).not.toBeNull();
    }
  });
  it.each(["🚀", "🇦🇹", "1️⃣", "img:/fixture.png"])("does not tint or filter artwork %s", async icon => {
    document.documentElement.setAttribute("data-theme", theme);
    await renderIcons({ color: "#ffffff", icon });
    const artwork = icon.startsWith("img:")
      ? [...container.querySelectorAll<HTMLElement>('section img[src="data:image/png;base64,fixture"]')]
      : textBoundaries();
    expect(artwork).toHaveLength(6);
    for (const boundary of artwork) {
      expect(boundary.style.color).not.toBe("#000000");
      expect(boundary.style.color).not.toBe("#ffffff");
      expect(boundary.style.filter).toBe("");
    }
  });
  it("retains existing theme fallbacks for missing or malformed configured colors", async () => {
    document.documentElement.setAttribute("data-theme", theme);
    for (const color of [undefined, "not-a-color"]) {
      await renderIcons({ color, icon: "AB" });
      for (const boundary of textBoundaries()) expect(["", "var(--text-primary)"]).toContain(boundary.style.color);
    }
  });
});
