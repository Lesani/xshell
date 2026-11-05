import { useCallback, useLayoutEffect, useState, type Dispatch, type SetStateAction } from "react";
import { invoke } from "@tauri-apps/api/core";
import { load } from "@tauri-apps/plugin-store";
import type { Tab } from "../types";
import { persistableTabs } from "../hosts/appTabs";
import { openTabsToPersist, type MigrationOutcome } from "../hosts/localMigration";
import { customTitleError, inProcessTitleTargetMatches, normalizeCustomTitle, type InProcessTitleTarget, type SaveInProcessTitle } from "./customTitle";

export interface OpenTabsSnapshot {
  tabs: Tab[];
  closing: ReadonlySet<string>;
  enabled: boolean;
  localDaemon: boolean;
  migration: MigrationOutcome;
}
export interface OpenTabsIO {
  set(tabs: Tab[]): Promise<void>;
  save(): Promise<void>;
  sync(): Promise<void>;
}
export const openTabsIO: OpenTabsIO = {
  async set(tabs) { const store = await load("settings.json", { defaults: {}, autoSave: true }); await store.set("open_tabs", tabs); },
  async save() { const store = await load("settings.json", { defaults: {}, autoSave: true }); await store.save(); },
  async sync() { await invoke("local_migration_sync_settings"); },
};

export function patchInProcessTitle(tabs: Tab[], target: InProcessTitleTarget, value: string | null, closing: ReadonlySet<string>): Tab[] {
  return tabs.map(tab => {
    if (closing.has(tab.id) || !inProcessTitleTargetMatches(target, tab)) return tab;
    const next = { ...tab };
    if (value === null) delete next.customTitle;
    else next.customTitle = value;
    return next;
  });
}
interface Receipt {
  target: InProcessTitleTarget;
  value: string | null;
  afterRevision: number;
  resolve(): void;
  reject(error: unknown): void;
}
const unavailable = () => new Error("This Terminal or its saved layout is no longer available for naming");

// One queue for this key only. Every queued operation reads the latest committed snapshot;
// explicit receipts certify set + save + fsync, never a React update or an autosave timer.
export class OpenTabsPersistence {
  private latest: OpenTabsSnapshot | null = null;
  private revision = 0;
  private chain: Promise<void> = Promise.resolve();
  private receipts: Receipt[] = [];
  constructor(private io: OpenTabsIO = openTabsIO) {}

  private serialized(s: OpenTabsSnapshot): Tab[] | null {
    if (!s.enabled || !s.migration.writesLayout) return null;
    const tabs = persistableTabs(s.tabs);
    return s.localDaemon ? openTabsToPersist(tabs, s.migration) : tabs;
  }
  private current(target: InProcessTitleTarget, s = this.latest): Tab | undefined {
    return s?.tabs.find(t => !s.closing.has(t.id) && inProcessTitleTargetMatches(target, t));
  }
  request(target: InProcessTitleTarget, value: string | null): Promise<void> {
    if (value !== null) {
      const error = customTitleError(value);
      if (error) throw new Error(error);
      value = normalizeCustomTitle(value);
    }
    if (!this.latest || !this.current(target) || !this.serialized(this.latest)) throw unavailable();
    return new Promise((resolve, reject) => this.receipts.push({ target, value, afterRevision: this.revision, resolve, reject }));
  }
  commit(snapshot: OpenTabsSnapshot): void {
    const previous = this.latest;
    if (previous && previous.tabs === snapshot.tabs && previous.closing === snapshot.closing
      && previous.enabled === snapshot.enabled && previous.localDaemon === snapshot.localDaemon && previous.migration === snapshot.migration) return;
    this.latest = snapshot;
    this.revision++;
    // Invalidate immediately, including while a prior disk operation is still in flight.
    this.receipts = this.receipts.filter(r => {
      if (this.current(r.target, snapshot)) return true;
      r.reject(unavailable()); return false;
    });
    this.chain = this.chain.catch(() => {}).then(() => this.write());
  }
  private async write(): Promise<void> {
    const snapshot = this.latest!;
    const active = this.receipts.filter(r => r.afterRevision < this.revision);
    this.receipts = this.receipts.filter(r => !active.includes(r));
    try {
      const next = this.serialized(snapshot);
      if (!next) { if (active.length) throw unavailable(); return; }
      for (const r of active) {
        const tab = this.current(r.target, snapshot);
        if (!tab || (tab.customTitle ?? null) !== r.value) throw unavailable();
      }
      await this.io.set(next);
      if (active.length) { await this.io.save(); await this.io.sync(); }
      for (const r of active) {
        const tab = this.current(r.target);
        if (!tab || !this.latest || !this.serialized(this.latest) || (tab.customTitle ?? null) !== r.value) r.reject(unavailable());
        else r.resolve();
      }
    } catch (error) {
      for (const r of active) r.reject(error);
      // Ordinary background persistence remains nonfatal. Failed receipts are never retried
      // or resolved by a later background write; retry is a new explicit request.
    }
  }
  idle(): Promise<void> { return this.chain; }
}

// Used by App and the actual menu integration tests. No I/O or receipt mutation occurs in
// the functional updater. Equal-value retries still create a committed state transition.
export function useOpenTabsPersistence(snapshot: OpenTabsSnapshot, setTabs: Dispatch<SetStateAction<Tab[]>>, io: OpenTabsIO = openTabsIO): SaveInProcessTitle {
  const [writer] = useState(() => new OpenTabsPersistence(io));
  useLayoutEffect(() => { writer.commit(snapshot); }, [writer, snapshot.tabs, snapshot.closing, snapshot.enabled, snapshot.localDaemon, snapshot.migration]);
  return useCallback(async (target, value) => {
    const pending = writer.request(target, value);
    const normalized = value === null ? null : normalizeCustomTitle(value);
    setTabs(tabs => patchInProcessTitle(tabs, target, normalized, snapshot.closing));
    return pending;
  }, [writer, setTabs, snapshot.closing]);
}
