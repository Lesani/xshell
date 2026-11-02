import type { Tab } from "../types";

// Shared names override presentation only. Raw Shell keeps its Project-primary layout
// without an override, and retains both Project and automatic shell context with one.
export function tabName(tab: Tab, projectName = tab.projectName || "") {
  const raw = tab.shellMode === "raw";
  const primary = tab.customTitle || (raw ? projectName || tab.title : tab.title);
  const secondary = raw
    ? (tab.customTitle ? [projectName, tab.title].filter(Boolean).join(" · ") : tab.title)
    : projectName;
  return { primary, secondary, tooltip: secondary ? `${primary} — ${secondary}` : primary };
}

export const effectiveTabName = (tab: Tab) => tab.customTitle || tab.title;
