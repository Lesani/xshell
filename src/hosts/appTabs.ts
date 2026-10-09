import type { Group, Tab } from "../types";
import { collectLeafIds } from "../layout";
import { restoreGroupIds, tabFromTerminal } from "./reconcile";
import { LOCAL_HOST, tabHostOf } from "./localHost";
import { byDaemonOrder, isLegacyLocal } from "./localMigration";
import type { HostId, TerminalInfo } from "./types";

// Pure pieces of App's Tab persistence, restore and Host bookkeeping, so they can be tested
// without rendering App.

// Daemon Tabs (`terminal` set: Remote, and Local ones in local Daemon mode) are mirrored from
// their Daemon, never persisted in `open_tabs`. Returns `tabs` itself when nothing is dropped.
export function persistableTabs(tabs: Tab[]): Tab[] {
  return tabs.some(t => t.terminal) ? tabs.filter(t => !t.terminal) : tabs;
}

const byCreated = (list: readonly TerminalInfo[]) => list.slice().sort((a, b) => a.createdAtMs - b.createdAtMs);

export interface RestoreInput {
  // `open_tabs` as saved; in local Daemon mode, the Tabs the migration left in-process.
  saved: Tab[] | null | undefined;
  // Configured Remote Host ids.
  hosts: HostId[];
  // New Local Tabs run in the Daemon (`local_host_info` said "daemon").
  localDaemon: boolean;
  // The cached `terminals` list per wire Host id.
  cached: (host: HostId) => TerminalInfo[] | null;
  // Local Daemon mode: Terminals the migration of saved in-process Tabs confirmed (from the
  // live list, so they restore with their groups even before the cache has them).
  migrated?: TerminalInfo[];
}

// The Tabs a start restores: saved in-process Tabs that have a session, then every cached
// Daemon Tab per Remote Host, then (in local Daemon mode only) the Local Daemon Tabs: the
// cached ones and the migrated ones merged, in the Daemon's order. In in-process mode the
// cached "local" list is kept but its Tabs are not mounted.
export function restorableTabs(r: RestoreInput): Tab[] {
  const saved = (r.saved ?? []).filter(isLegacyLocal);
  const remote = r.hosts.flatMap(h => byCreated(r.cached(h) ?? []).map(info => tabFromTerminal(h, info)));
  let local: Tab[] = [];
  if (r.localDaemon) {
    const merged = new Map((r.cached(LOCAL_HOST) ?? []).map(i => [i.terminal, i]));
    for (const i of r.migrated ?? []) merged.set(i.terminal, i); // the live entry is newer
    local = [...merged.values()].sort(byDaemonOrder).map(info => tabFromTerminal(undefined, info));
  }
  return [...saved, ...remote, ...local];
}

// The saved groups whose leaves are all restored (and at least two), and the restored Tabs with
// their groupId: cached Daemon Tabs get it back from the kept layouts (amendment 22); a groupId
// pointing at a group that is not kept (a leftover of an earlier bug) is scrubbed.
export function restoreGroups(restorable: Tab[], saved: Group[] | null | undefined): { tabs: Tab[]; groups: Group[] } {
  const restoredIds = new Set(restorable.map(t => t.id));
  const groups = (saved ?? []).filter(g => {
    const leaves = collectLeafIds(g.layout);
    return leaves.length >= 2 && leaves.every(id => restoredIds.has(id));
  });
  const valid = new Set(groups.map(g => g.id));
  const tabs = restoreGroupIds(restorable, groups).map(t => (t.groupId && !valid.has(t.groupId)) ? { ...t, groupId: undefined } : t);
  return { tabs, groups };
}

// The offline cache is needed with Remote Hosts and in local Daemon mode (its cached Local
// Daemon Tabs keep their groups across a restart).
export const needsCache = (hostCount: number, localDaemon: boolean): boolean => hostCount > 0 || localDaemon;

// The registry's lists by wire id, as reconcile wants them: "local" is the Tab host undefined.
export function listsForReconcile(fresh: readonly [HostId, TerminalInfo[]][]): [HostId | undefined, TerminalInfo[]][] {
  return fresh.map(([h, list]) => [tabHostOf(h), list]);
}

// Hosts whose Projects and sessions come over the link. The Local Host's always come from the
// in-process path (stamped with `host` undefined), even when its Terminals run in a Daemon.
export const isRemoteDataHost = (host: HostId): boolean => host !== LOCAL_HOST;

// One Host's fetched items (Projects or sessions) into a by-Host record, stamped with
// `host`. Returns `prev` itself for "local" (its data is the in-process path's, under the
// record's own "local" key) and for a failed fetch (`items` null).
export function withHostData<T extends { host?: HostId }>(prev: Record<string, T[]>, host: HostId, items: T[] | null): Record<string, T[]> {
  if (!items || !isRemoteDataHost(host)) return prev;
  return { ...prev, [host]: items.map(x => ({ ...x, host })) };
}
