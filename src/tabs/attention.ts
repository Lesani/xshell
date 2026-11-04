import type { Tab, Group } from "../types";
import type { TerminalInfo } from "../hosts/types";
import { daemonHost } from "../hosts/localHost";
import { collectLeafIds } from "../layout";
import type { TabAgentStatus } from "./agentStatus";

export const COMPLETION_ATTENTION_CAPABILITY = "term.completion-attention";
export const COMPLETION_SEEN_SETTING = "terminal_completion_seen";
export type CompletionSeen = Readonly<Record<string, string>>;
export const attentionKey = (tab: Tab): string => {
  const host = daemonHost(tab);
  return JSON.stringify(host ? ["daemon", host, tab.terminal] : ["in-process", tab.id]);
};
const partsOf = (key: string): string[] => {
  try { const v: unknown = JSON.parse(key); if (Array.isArray(v) && v.every(p => typeof p === "string" && p)) return v; } catch { /* Invalid stored key. */ }
  return [];
};
const uuid = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
export function decodeCompletionSeen(value: unknown): CompletionSeen {
  if (!value || typeof value !== "object" || !("version" in value) || value.version !== 1 || !("entries" in value) || !value.entries || typeof value.entries !== "object" || Array.isArray(value.entries)) return {};
  return Object.fromEntries(Object.entries(value.entries).filter(([key, id]) => {
    const p = partsOf(key);
    return p.length === 3 && p[0] === "daemon" && typeof id === "string" && uuid.test(id);
  }));
}
export function persistedCompletionSeen(seen: CompletionSeen): CompletionSeen {
  return Object.fromEntries(Object.entries(seen).filter(([key]) => partsOf(key)[0] === "daemon"));
}

// Both identities are explicit observations. An old callback must not replace a newer mark.
export function markCompletionSeen(seen: CompletionSeen, key: string, observed: string | undefined, current: string | undefined): CompletionSeen {
  return !observed || observed !== current || seen[key] === observed ? seen : { ...seen, [key]: observed };
}
export function pruneCompletionSeen(seen: CompletionSeen, host: string, listed: readonly string[] | null | undefined): CompletionSeen {
  if (!listed) return seen;
  const keep = new Set(listed);
  return removeSeen(seen, p => p[0] === "daemon" && p[1] === host && !keep.has(p[2]));
}
export function removeCompletionSeenHost(seen: CompletionSeen, host: string): CompletionSeen {
  return removeSeen(seen, p => p[0] === "daemon" && p[1] === host);
}
function removeSeen(seen: CompletionSeen, remove: (p: string[]) => boolean): CompletionSeen {
  const keys = Object.keys(seen).filter(k => remove(partsOf(k)));
  if (!keys.length) return seen;
  const next = { ...seen }; for (const key of keys) delete next[key]; return next;
}

export function visibleTerminalIds(tabs: readonly Tab[], groups: readonly Group[], active: string): ReadonlySet<string> {
  const group = groups.find(g => g.id === active);
  if (group) return new Set(collectLeafIds(group.layout));
  const tab = tabs.find(t => t.id === active && t.type === "terminal" && !t.groupId);
  return new Set(tab ? [tab.id] : []);
}
export type AttentionRing = "working" | "unread" | "needs-you" | null;
export function attentionRing(status: TabAgentStatus | null, info: TerminalInfo | undefined, seen: string | undefined, observed: string | undefined, viewing: boolean, closing: boolean): AttentionRing {
  if (!status || status.stale || closing || info?.exitCode != null) return null;
  if (status.status === "working") return "working";
  if (status.status === "needs-you") return "needs-you";
  if (status.status !== "finished" || !observed || viewing || seen === observed || info?.completionAttention?.inputHandled) return null;
  return "unread";
}
