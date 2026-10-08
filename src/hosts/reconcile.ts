import type { Group, Tab } from "../types";
import { removeLeaf, collectLeafIds } from "../layout";
import { getShellById } from "../shells";
import { AGENT_IDS, type AgentId } from "../agents";
import type { HostId, TerminalInfo } from "./types";

// A Remote Host's Terminals are the source of truth (ADR-0001): the Desktop shows exactly one
// Tab per listed Terminal. These pure functions turn a `terminals` list into Tab changes.

export const REMOTE_TAB_PREFIX = "remote-";
export const remoteTabId = (uuid: string) => `${REMOTE_TAB_PREFIX}${uuid}`;

const str = (v: unknown): string | undefined => (typeof v === "string" && v ? v : undefined);
const num = (v: unknown): number | undefined => (typeof v === "number" && Number.isFinite(v) ? v : undefined);

function basename(p: string): string {
  return p.replace(/[\\/]+$/, "").split(/[\\/]/).pop() || "";
}

export function tabFromTerminal(host: HostId, info: TerminalInfo): Tab {
  const { spec, meta } = info;
  const raw = spec.shellMode === "raw";
  const agent = (AGENT_IDS as string[]).includes(spec.agent ?? "") ? (spec.agent as AgentId) : undefined;
  const shellName = spec.shellId ? getShellById(spec.shellId)?.name : undefined;
  const tab: Tab = {
    id: remoteTabId(info.terminal),
    type: "terminal",
    host,
    terminal: info.terminal,
    title: str(meta.title) || (raw ? (shellName || "Shell") : "Session"),
    projectPath: spec.cwd,
    projectName: str(meta.projectName) || basename(spec.cwd) || "~",
    shellMode: raw ? "raw" : "claude",
    createdAt: num(meta.createdAt) ?? info.createdAtMs,
  };
  if (spec.sessionId) tab.sessionId = spec.sessionId;
  if (agent) tab.agent = agent;
  if (spec.shellId) tab.shellId = spec.shellId;
  return tab;
}

export interface ReconcileDelta {
  add: Tab[];
  remove: string[];      // tab ids
  update: Tab[];         // replacement tabs (same id)
  confirmed: string[];   // pending-open uuids now present in the list
}

// `pending`: uuids of Terminals this Desktop is opening (kept even while absent from the list).
// `isDirty`: local, not-yet-echoed edits (amendment 17) — the list never overwrites them.
export function reconcile(
  tabs: Tab[],
  host: HostId,
  list: TerminalInfo[],
  pending: ReadonlySet<string>,
  isDirty: (tabId: string, field: "title" | "sessionId") => boolean = () => false,
): ReconcileDelta {
  const listed = new Map(list.map(i => [i.terminal, i]));
  const mine = tabs.filter(t => t.host === host && t.terminal);
  const have = new Set(mine.map(t => t.terminal!));
  const remove: string[] = [];
  const update: Tab[] = [];
  for (const t of mine) {
    const info = listed.get(t.terminal!);
    if (!info) {
      if (!pending.has(t.terminal!)) remove.push(t.id);
      continue;
    }
    let next = t;
    const title = str(info.meta.title);
    if (title && title !== t.title && !isDirty(t.id, "title")) next = { ...next, title };
    const sid = info.spec.sessionId || undefined;
    if (sid && sid !== t.sessionId && !isDirty(t.id, "sessionId")) next = { ...next, sessionId: sid };
    const projectName = str(info.meta.projectName);
    if (projectName && projectName !== t.projectName) next = { ...next, projectName };
    if (next !== t) update.push(next);
  }
  const add = list
    .filter(i => !have.has(i.terminal))
    .sort((a, b) => a.createdAtMs - b.createdAtMs)
    .map(i => tabFromTerminal(host, i));
  const confirmed = list.filter(i => pending.has(i.terminal)).map(i => i.terminal);
  return { add, remove, update, confirmed };
}

export function isEmptyDelta(d: ReconcileDelta): boolean {
  return d.add.length === 0 && d.remove.length === 0 && d.update.length === 0;
}

// Pure piece used by every setter: apply a delta to a tab list. Returns `tabs` itself when
// nothing changes (keeps reference identity, so applying the same list twice is a no-op).
export function applyTabDelta(tabs: Tab[], d: ReconcileDelta): Tab[] {
  if (isEmptyDelta(d)) return tabs;
  const removed = new Set(d.remove);
  const updated = new Map(d.update.map(t => [t.id, t]));
  const out: Tab[] = [];
  for (const t of tabs) {
    if (removed.has(t.id)) continue;
    const u = updated.get(t.id);
    // Keep Desktop-local fields (groupId, lastActiveAt) from the current tab.
    out.push(u ? { ...u, groupId: t.groupId, lastActiveAt: t.lastActiveAt } : t);
  }
  const ids = new Set(out.map(t => t.id));
  for (const t of d.add) if (!ids.has(t.id)) out.push(t);
  return out;
}

// Drops removed leaves from group layouts; a group whose layout becomes null is dropped.
export function applyGroupRemovals(groups: Group[], removed: readonly string[]): Group[] {
  if (removed.length === 0) return groups;
  let changed = false;
  const out: Group[] = [];
  for (const g of groups) {
    let layout: Group["layout"] | null = g.layout;
    for (const id of removed) if (layout) layout = removeLeaf(layout, id);
    if (layout === g.layout) { out.push(g); continue; }
    changed = true;
    if (layout) out.push({ ...g, layout });
  }
  return changed ? out : groups;
}

// Amendment 22: a removed focused leaf moves focus to a surviving leaf of the same group.
export function applyFocusRemovals(active: Record<string, string>, groups: Group[], removed: readonly string[]): Record<string, string> {
  if (removed.length === 0) return active;
  const gone = new Set(removed);
  let changed = false;
  const next = { ...active };
  for (const [gid, leaf] of Object.entries(active)) {
    if (!gone.has(leaf)) continue;
    changed = true;
    const g = groups.find(x => x.id === gid);
    const survivor = g ? collectLeafIds(g.layout).find(id => !gone.has(id)) : undefined;
    if (survivor) next[gid] = survivor; else delete next[gid];
  }
  return changed ? next : active;
}

export interface ReconcileState { tabs: Tab[]; groups: Group[]; activeLeafByGroup: Record<string, string> }

export function applyReconcile(s: ReconcileState, d: ReconcileDelta): ReconcileState & { removed: string[] } {
  const tabs = applyTabDelta(s.tabs, d);
  const groups = applyGroupRemovals(s.groups, d.remove);
  const activeLeafByGroup = applyFocusRemovals(s.activeLeafByGroup, groups, d.remove);
  return { tabs, groups, activeLeafByGroup, removed: d.remove };
}

// Amendment 22: at startup, cached remote tabs get their groupId back from the persisted
// group layouts (leaf ids are the stable `remote-<uuid>`).
export function restoreGroupIds(tabs: Tab[], groups: Group[]): Tab[] {
  const groupOf = new Map<string, string>();
  for (const g of groups) for (const id of collectLeafIds(g.layout)) groupOf.set(id, g.id);
  return tabs.map(t => {
    if (!t.host) return t;
    const gid = groupOf.get(t.id);
    return gid && t.groupId !== gid ? { ...t, groupId: gid } : t;
  });
}

// ── Several Hosts in one pass (one tabs/groups/focus transaction) ───

export interface ReconcileOptions {
  pending: ReadonlySet<string>;
  isDirty?: (tabId: string, field: "title" | "sessionId") => boolean;
  // Terminals whose Tab the user closed (close still in progress): never re-added.
  isClosing?: (uuid: string) => boolean;
}

export interface HostsReconcile {
  deltas: ReconcileDelta[];
  removed: string[];   // tab ids removed across all Hosts
  confirmed: string[]; // pending-open uuids now listed
}

// Reconciles every Host's latest list against one evolving tab list, so the result can be
// applied as a single transaction.
export function reconcileHosts(tabs: Tab[], lists: [HostId, TerminalInfo[]][], o: ReconcileOptions): HostsReconcile {
  const deltas: ReconcileDelta[] = [];
  const removed: string[] = [];
  const confirmed: string[] = [];
  let cur = tabs;
  for (const [host, list] of lists) {
    const visible = o.isClosing ? list.filter(i => !o.isClosing!(i.terminal)) : list;
    const d = reconcile(cur, host, visible, o.pending, o.isDirty);
    confirmed.push(...d.confirmed);
    if (isEmptyDelta(d)) continue;
    deltas.push(d);
    removed.push(...d.remove);
    cur = applyTabDelta(cur, d);
  }
  return { deltas, removed, confirmed };
}

// Pure pieces of the transaction, each usable as a functional state update.
export const applyTabDeltas = (tabs: Tab[], deltas: ReconcileDelta[]): Tab[] => deltas.reduce(applyTabDelta, tabs);

// Applies every Host's delta at once: groups lose all removed leaves together, and focus
// moves only to a leaf that survives every removal.
export function applyHostsReconcile(s: ReconcileState, r: HostsReconcile): ReconcileState {
  const tabs = applyTabDeltas(s.tabs, r.deltas);
  const groups = applyGroupRemovals(s.groups, r.removed);
  const activeLeafByGroup = applyFocusRemovals(s.activeLeafByGroup, groups, r.removed);
  return { tabs, groups, activeLeafByGroup };
}
