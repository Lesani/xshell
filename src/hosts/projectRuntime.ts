import type { ProjectInfo, Tab } from "../types";
import { normalizeProjectPath } from "../utils";

// Runtime Project identity must not use the legacy case-folded sidebar layout key.
export function runtimeProjectKey(project: { host?: string; path: string }): string {
  return JSON.stringify([project.host ?? null, normalizeProjectPath(project.path)]);
}

export function runtimeTabProjectKey(tab: Pick<Tab, "host" | "projectPath">): string | null {
  return tab.projectPath ? runtimeProjectKey({ host: tab.host, path: tab.projectPath }) : null;
}

export function resolveTabProject(projects: ProjectInfo[], tab: Pick<Tab, "host" | "projectPath" | "projectName"> | undefined): ProjectInfo | null {
  if (!tab?.projectPath) return null;
  const key = runtimeTabProjectKey(tab);
  const found = projects.find(project => runtimeProjectKey(project) === key);
  if (found) return found;
  if (!tab.projectName) return null;
  return { name: tab.projectName, path: tab.projectPath, host: tab.host, encoded_name: "", session_count: 0, last_active: "" };
}

export function titlePollProjects(tabs: Pick<Tab, "host" | "projectPath">[]): Map<string, { host?: string; path: string }> {
  const projects = new Map<string, { host?: string; path: string }>();
  for (const tab of tabs) {
    const key = runtimeTabProjectKey(tab);
    if (key) projects.set(key, { host: tab.host, path: tab.projectPath! });
  }
  return projects;
}
