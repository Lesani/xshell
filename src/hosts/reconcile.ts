import type { Group, Tab } from "../types";
import { removeLeaf, collectLeafIds } from "../layout";
import { getShellById } from "../shells";
import { AGENT_IDS, type AgentId } from "../agents";
import type { HostId, TerminalInfo } from "./types";

// A Daemon's Terminals are the source of truth for its Host, Remote or Local (ADR-0001,
// ADR-0005): the Desktop shows exactly one Tab per listed Terminal. These pure functions turn a
// `terminals` list into Tab changes. `host` undefined is the Local Host; its in-process Tabs
// (no `terminal`) are never touched.

export const REMOTE_TAB_PREFIX = "remote-";
export const remoteTabId = (uuid: string) => `${REMOTE_TAB_PREFIX}${uuid}`;

const str = (v: unknown): string | undefined => (typeof v === "string" && v ? v : undefined);
const num = (v: unknown): number | undefined => (typeof v === "number" && Number.isFinite(v) ? v : undefined);

function basename(p: string): string {
  return p.replace(/[\\/]+$/, "").split(/[\\/]/).pop() || "";
}

export function tabFromTerminal(host: HostId | undefined, info: TerminalInfo): Tab {
  const { spec, meta } = info;
  const raw = spec.shellMode === "raw";
  const agent = (AGENT_IDS as string[]).includes(spec.agent ?? "") ? (spec.agent as AgentId) : undefined;
  const shellName = spec.shellId ? getShellById(spec.shellId)?.name : undefined;
  const tab: Tab = {
    id: remoteTabId(info.terminal),
    type: "terminal",
    terminal: info.terminal,
    title: str(meta.title) || (raw ? (shellName || "Shell") : "Session"),
    projectPath: spec.cwd,
    projectName: str(meta.projectName) || basename(spec.cwd) || "~",
    shellMode: raw ? "raw" : "claude",
    createdAt: num(meta.createdAt) ?? info.createdAtMs,
  };
  if (host) tab.host = host;
  if (spec.sessionId) tab.sessionId = spec.sessionId;
  if (agent) tab.agent = agent;
  if (spec.shellId) tab.shellId = spec.shellId;
  if (spec.skipPermissions) tab.skipPermissions = true;
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
  host: HostId | undefined,
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
    // Compared as booleans, so turning it off on the Host clears it here too.
    const skip = !!info.spec.skipPermissions;
    if (skip !== !!t.skipPermissions) next = { ...next, skipPermissions: skip };
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

// Amendment 22: at startup, cached Daemon Tabs (any Host) get their groupId back from the
// persisted group layouts (leaf ids are the stable `remote-<uuid>`).
export function restoreGroupIds(tabs: Tab[], groups: Group[]): Tab[] {
  const groupOf = new Map<string, string>();
  for (const g of groups) for (const id of collectLeafIds(g.layout)) groupOf.set(id, g.id);
  return tabs.map(t => {
    if (!t.terminal) return t;
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
export function reconcileHosts(tabs: Tab[], lists: [HostId | undefined, TerminalInfo[]][], o: ReconcileOptions): HostsReconcile {
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

// ── An open answered with another Terminal (xshell#41) ──────────────

// Where showing `tab` lands: its group with `tab` as the group's active leaf, or the Tab.
export interface Focus { activeTabId: string; leaf?: { groupId: string; tabId: string } }
export const focusOf = (tab: Tab): Focus =>
  tab.groupId ? { activeTabId: tab.groupId, leaf: { groupId: tab.groupId, tabId: tab.id } } : { activeTabId: tab.id };

export interface Adopted extends ReconcileState {
  removed: string[];
  // `from`'s Tab was the one shown.
  shown: boolean;
  // Where to go when it was shown (null when it was not): `to`'s Tab or, while `to` is not
  // listed yet, Home.
  focus: Focus | null;
  // Set when `focus` is Home: the Terminal whose Tab to show once the Host lists it.
  focusWhenListed: string | null;
}

// Home stands in for a Tab that is not listed yet. Any other selection, the user's or an
// automatic repair such as a group's dissolution, cancels the deferred focus.
export const keepsFocusWhenListed = (activeTabId: string) => activeTabId === "home";

// The Daemon answered the open of Terminal `from` with `to`, which already runs its agent
// session: `from`'s Tab (and its group leaf) gives way to `to`'s.
export function adoptTab(s: ReconcileState, activeTabId: string, from: string, to: string): Adopted {
  const fromId = remoteTabId(from);
  const gone = s.tabs.find(t => t.id === fromId);
  const shown = activeTabId === fromId || (!!gone?.groupId && activeTabId === gone.groupId && s.activeLeafByGroup[gone.groupId] === fromId);
  const removed = gone ? [fromId] : [];
  const tabs = gone ? s.tabs.filter(t => t.id !== fromId) : s.tabs;
  const groups = applyGroupRemovals(s.groups, removed);
  const activeLeafByGroup = applyFocusRemovals(s.activeLeafByGroup, groups, removed);
  const owner = tabs.find(t => t.id === remoteTabId(to));
  const base = { tabs, groups, activeLeafByGroup, removed, shown };
  if (!shown) return { ...base, focus: null, focusWhenListed: null };
  if (owner) return { ...base, focus: focusOf(owner), focusWhenListed: null };
  // Home, even when `from` was a pane of a group that survives: the group's own focus repair
  // (or its dissolution) would otherwise select another Tab and cancel the deferred focus.
  return { ...base, focus: { activeTabId: "home" }, focusWhenListed: to };
}

// Groups left with one leaf or none dissolve; their Tabs become standalone. When the selected
// entry was such a group, its surviving Tab (or Home) is selected instead.
export function dissolveSmallGroups(tabs: Tab[], groups: Group[], activeTabId: string): { tabs: Tab[]; groups: Group[]; dissolved: string[]; activeTabId: string } | null {
  const dissolved = groups.filter(g => collectLeafIds(g.layout).length <= 1).map(g => g.id);
  if (dissolved.length === 0) return null;
  const next = tabs.map(t => t.groupId && dissolved.includes(t.groupId) ? { ...t, groupId: undefined } : t);
  let active = activeTabId;
  if (dissolved.includes(activeTabId)) {
    const survivors = tabs.filter(t => t.groupId && dissolved.includes(t.groupId));
    active = survivors[0]?.id || "home";
  }
  return { tabs: next, groups: groups.filter(g => !dissolved.includes(g.id)), dissolved, activeTabId: active };
}
