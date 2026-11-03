import type { Group, Tab } from "../types";
import { runtimeTabProjectKey } from "../hosts/projectRuntime";
import type { ReorderDrop } from "../hooks/useDragReorder";

export type TabBarEntry = { kind: "tab"; id: string; tab: Tab } | { kind: "group"; id: string; group: Group };
export const groupTabsSetting = (value: unknown): boolean => typeof value === "boolean" ? value : true;

export function tabBarEntries(tabs: Tab[], groups: Group[]): TabBarEntry[] {
  const seen = new Set<string>();
  const out: TabBarEntry[] = [];
  for (const tab of tabs) {
    const group = tab.groupId ? groups.find(g => g.id === tab.groupId) : undefined;
    if (!group) out.push({ kind: "tab", id: tab.id, tab });
    else if (!seen.has(group.id)) {
      seen.add(group.id);
      out.push({ kind: "group", id: group.id, group });
    }
  }
  return out;
}

function blockKey(entry: TabBarEntry, tabs: Tab[]): string {
  const members = entry.kind === "tab" ? [entry.tab] : tabs.filter(t => t.groupId === entry.id);
  const keys = members.map(runtimeTabProjectKey);
  const project = keys[0];
  return project && keys.every(k => k === project) ? `project:${project}` : `opaque:${entry.kind}:${entry.id}`;
}

export function flattenEntries(entries: TabBarEntry[], tabs: Tab[]): Tab[] {
  return entries.flatMap(e => e.kind === "tab" ? [e.tab] : tabs.filter(t => t.groupId === e.id));
}

export function normalizeProjectTabs(tabs: Tab[], groups: Group[]): Tab[] {
  const blocks = new Map<string, TabBarEntry[]>();
  for (const entry of tabBarEntries(tabs, groups)) {
    const key = blockKey(entry, tabs);
    const block = blocks.get(key) ?? [];
    block.push(entry);
    blocks.set(key, block);
  }
  const next = flattenEntries([...blocks.values()].flat(), tabs);
  return next.length === tabs.length && next.every((t, i) => t.id === tabs[i].id) ? tabs : next;
}

// Indices are atomic-entry gaps, never leaf indices. Cross-block drops use the hit
// entry's midpoint side, then move the whole source block at the target boundary.
export function projectDrop(entries: TabBarEntry[], tabs: Tab[], drop: ReorderDrop): TabBarEntry[] {
  const { sourceIndex, destinationGap, targetIndex } = drop;
  if (!entries[sourceIndex] || !entries[targetIndex]) return entries;
  const sourceKey = blockKey(entries[sourceIndex], tabs);
  const targetKey = blockKey(entries[targetIndex], tabs);
  if (sourceKey === targetKey) {
    const next = [...entries];
    const [entry] = next.splice(sourceIndex, 1);
    next.splice(destinationGap > sourceIndex ? destinationGap - 1 : destinationGap, 0, entry);
    return next;
  }
  const source = entries.filter(e => blockKey(e, tabs) === sourceKey);
  const rest = entries.filter(e => blockKey(e, tabs) !== sourceKey);
  const first = rest.findIndex(e => blockKey(e, tabs) === targetKey);
  const last = rest.length - 1 - [...rest].reverse().findIndex(e => blockKey(e, tabs) === targetKey);
  const boundary = destinationGap <= targetIndex ? first : last + 1;
  return [...rest.slice(0, boundary), ...source, ...rest.slice(boundary)];
}
