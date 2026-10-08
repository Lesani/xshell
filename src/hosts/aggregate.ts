import type { SessionInfo } from "../types";

// Merge several Hosts' recent-session lists (Local first): newest first, capped. When only
// the first list has entries it is returned as-is, so a Desktop with no Remote Hosts keeps
// exactly the Local order.
export function mergeRecent(lists: SessionInfo[][], limit = 100): SessionInfo[] {
  if (lists.length === 0) return [];
  const [first, ...rest] = lists;
  if (rest.every(l => l.length === 0)) return first;
  return lists.flat().sort((a, b) => b.timestamp.localeCompare(a.timestamp)).slice(0, limit);
}
