import type { Tab } from "../types";
import type { HostId, TerminalInfo } from "./types";
import { daemonHost } from "./localHost";

// Pushes a Daemon Tab's (Remote, or Local on the wire Host "local") late-bound metadata (linked session id, title) to its Terminal with
// `host_term_update`, so every Desktop and the Daemon's restore see it (amendment 17).
//
// Only explicit local edits are sent: title-sync linking, a /rename picked up by the title
// poll, a /branch switch. Each edit is tracked as dirty → in-flight → acked, and stays tracked
// until a `terminals` list echoes the value. Differences that arrive from the Daemon's list are
// applied to the Tab by reconcile and never sent back, so two Desktops cannot echo each other.

export type MetaField = "sessionId" | "title";

interface Entry {
  value: string;
  state: "dirty" | "inflight" | "acked";
  attempts: number;
}

export interface MetaUpdate {
  tabId: string;
  host: HostId;
  terminal: string;
  sessionId?: string;
  meta?: { title: string };
  fields: MetaField[];
  values: Partial<Record<MetaField, string>>;
}

const MAX_ATTEMPTS = 2; // the first try plus one retry

function listedValue(info: TerminalInfo, field: MetaField): string | undefined {
  if (field === "sessionId") return info.spec.sessionId || undefined;
  const t = info.meta.title;
  return typeof t === "string" ? t : undefined;
}

export class MetaSync {
  private entries = new Map<string, Partial<Record<MetaField, Entry>>>();

  // A local edit of a remote tab. Re-marking the same value is a no-op (setState updaters may
  // run twice under StrictMode).
  markDirty(tabId: string, field: MetaField, value: string) {
    const fields = this.entries.get(tabId) ?? {};
    const cur = fields[field];
    if (cur && cur.value === value) return;
    fields[field] = { value, state: "dirty", attempts: 0 };
    this.entries.set(tabId, fields);
  }

  isDirty(tabId: string, field: MetaField): boolean {
    return !!this.entries.get(tabId)?.[field];
  }

  hasPending(): boolean {
    for (const f of this.entries.values()) for (const e of Object.values(f)) if (e?.state === "dirty") return true;
    return false;
  }

  // Dirty fields to send now (marked in-flight). One update per tab.
  takeUpdates(tabs: Tab[]): MetaUpdate[] {
    const out: MetaUpdate[] = [];
    for (const tab of tabs) {
      const host = daemonHost(tab);
      if (!host || !tab.terminal) continue;
      const fields = this.entries.get(tab.id);
      if (!fields) continue;
      const u: MetaUpdate = { tabId: tab.id, host, terminal: tab.terminal, fields: [], values: {} };
      for (const f of ["sessionId", "title"] as const) {
        const e = fields[f];
        if (!e || e.state !== "dirty") continue;
        e.state = "inflight";
        e.attempts++;
        u.fields.push(f);
        u.values[f] = e.value;
        if (f === "sessionId") u.sessionId = e.value;
        else u.meta = { title: e.value };
      }
      if (u.fields.length) out.push(u);
    }
    return out;
  }

  // Result of the host_term_update call for `u`.
  settled(u: MetaUpdate, ok: boolean) {
    const fields = this.entries.get(u.tabId);
    if (!fields) return;
    for (const f of u.fields) {
      const e = fields[f];
      if (!e || e.value !== u.values[f] || e.state !== "inflight") continue; // superseded by a newer edit
      if (ok) e.state = "acked";
      else if (e.attempts < MAX_ATTEMPTS) e.state = "dirty";
      else delete fields[f];
    }
    this.prune(u.tabId);
  }

  // A `terminals` list arrived. An echoed value clears the entry; an acked entry whose value
  // the list contradicts lost to a later writer and is dropped (last writer wins).
  observe(host: HostId, list: TerminalInfo[], tabs: Tab[]) {
    const byUuid = new Map(list.map(i => [i.terminal, i]));
    for (const tab of tabs) {
      if (!tab.terminal || daemonHost(tab) !== host) continue;
      const fields = this.entries.get(tab.id);
      if (!fields) continue;
      const info = byUuid.get(tab.terminal);
      if (!info) continue;
      for (const f of ["sessionId", "title"] as const) {
        const e = fields[f];
        if (!e) continue;
        const v = listedValue(info, f);
        if (v === e.value || e.state === "acked") delete fields[f];
      }
      this.prune(tab.id);
    }
  }

  forget(tabId: string) {
    this.entries.delete(tabId);
  }

  private prune(tabId: string) {
    const f = this.entries.get(tabId);
    if (f && !f.sessionId && !f.title) this.entries.delete(tabId);
  }
}

// Which remote-tab fields changed between two tab lists — the local-edit detector used by
// App's setters (title-sync link/rename, branch switch).
export function localEdits(prev: Tab[], next: Tab[]): { tabId: string; field: MetaField; value: string }[] {
  const before = new Map(prev.map(t => [t.id, t]));
  const out: { tabId: string; field: MetaField; value: string }[] = [];
  for (const t of next) {
    if (!t.terminal) continue;
    const p = before.get(t.id);
    if (!p) continue;
    if (t.sessionId && t.sessionId !== p.sessionId) out.push({ tabId: t.id, field: "sessionId", value: t.sessionId });
    if (t.title !== p.title) out.push({ tabId: t.id, field: "title", value: t.title });
  }
  return out;
}

export const metaSync = new MetaSync();
