import type { Tab } from "../types";
import { daemonHost } from "../hosts/localHost";

export type RightPanel = "files" | "git" | "closed";
export type RightPanels = Record<string, RightPanel>;
export const RIGHT_PANELS_SETTING = "terminal_right_panels";

const terminalKey = (host: string, terminal: string): string => JSON.stringify(["daemon", host, terminal]);
export const rightPanelKey = (tab: Tab): string => {
  const host = daemonHost(tab);
  return host ? terminalKey(host, tab.terminal!) : JSON.stringify(["in-process", tab.id]);
};

function parseKey(key: string): string[] | null {
  try {
    const parts: unknown = JSON.parse(key);
    if (!Array.isArray(parts) || !parts.every(p => typeof p === "string" && p.length > 0)) return null;
    if ((parts[0] === "daemon" && parts.length === 3) || (parts[0] === "in-process" && parts.length === 2)) return parts;
  } catch { /* An invalid stored key is missing, not an explicit close. */ }
  return null;
}

export function decodeRightPanels(value: unknown): RightPanels {
  if (!value || typeof value !== "object" || !("version" in value) || value.version !== 1 || !("entries" in value)) return {};
  const entries = value.entries;
  if (!entries || typeof entries !== "object" || Array.isArray(entries)) return {};
  return Object.fromEntries(Object.entries(entries).filter(([key, panel]) => parseKey(key) && (panel === "files" || panel === "git" || panel === "closed")));
}

// Preserve TerminalTab's existing default, including its shellMode fallback (not agent id).
export const defaultRightPanel = (tab: Tab, fileExplorerOnStart: boolean): RightPanel =>
  fileExplorerOnStart && (tab.shellMode || "claude") === "claude" ? "files" : "closed";

export function setRightPanel(panels: RightPanels, tab: Tab, panel: RightPanel): RightPanels {
  const key = rightPanelKey(tab);
  return panels[key] === panel ? panels : { ...panels, [key]: panel };
}

export function ensureRightPanels(panels: RightPanels, tabs: readonly Tab[], fileExplorerOnStart: boolean): RightPanels {
  let next = panels;
  for (const tab of tabs) if (next[rightPanelKey(tab)] === undefined) next = setRightPanel(next, tab, defaultRightPanel(tab, fileExplorerOnStart));
  return next;
}

export function adoptRightPanel(panels: RightPanels, host: string, from: string, to: string): RightPanels {
  const source = terminalKey(host, from), target = terminalKey(host, to);
  if (source === target || panels[source] === undefined) return panels;
  const next = { ...panels };
  if (next[target] === undefined) next[target] = next[source];
  delete next[source];
  return next;
}

// Only pass current authoritative lists here, after startup hydration. Null/offline cache
// absence is not removal. Opens (and adopted owners not yet listed) still await confirmation.
export function pruneRightPanels(panels: RightPanels, host: string, listed: readonly string[] | null | undefined, pending: ReadonlySet<string> = new Set()): RightPanels {
  if (!listed) return panels;
  const keep = new Set([...listed, ...pending]);
  return withoutKeys(panels, key => {
    const parts = parseKey(key);
    return parts?.[0] === "daemon" && parts[1] === host && !keep.has(parts[2]);
  });
}

export function removeRightPanelHost(panels: RightPanels, host: string): RightPanels {
  return withoutKeys(panels, key => {
    const parts = parseKey(key);
    return parts?.[0] === "daemon" && parts[1] === host;
  });
}

function withoutKeys(panels: RightPanels, remove: (key: string) => boolean): RightPanels {
  const removed = Object.keys(panels).filter(remove);
  if (!removed.length) return panels;
  const next = { ...panels };
  for (const key of removed) delete next[key];
  return next;
}
