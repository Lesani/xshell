import { invoke, Channel } from "@tauri-apps/api/core";
import { load } from "@tauri-apps/plugin-store";
import type { Group, Tab } from "../types";
import { registry } from "./registry";
import { cache } from "./cache";
import { LOCAL_HOST } from "./localHost";
import { READY_MS, type Journal, type MigrationDeps } from "./localMigration";
import { MIGRATION_NS, uuidV5 } from "./uuidV5";
import type { TerminalInfo } from "./types";

// The real dependencies of `migrateLocalTabs`: the settings store, the migration lock,
// journal and settings guard (src-tauri local_migration.rs), and the local Daemon through the
// registry and host_term_*.

const settings = () => load("settings.json", { defaults: {}, autoSave: true });

// Resolves with `get()` once it is non-null (re-checked on every registry change), or with
// null after `ms`.
export function waitForRegistry<T>(get: () => T | null | undefined, ms: number): Promise<T | null> {
  return new Promise(resolve => {
    let settled = false;
    let unsub: () => void = () => {};
    const done = (v: T | null) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      unsub();
      resolve(v);
    };
    const check = () => { const v = get(); if (v !== null && v !== undefined) done(v); };
    const timer = setTimeout(() => done(null), ms);
    unsub = registry.subscribe(check);
    check();
  });
}

const localList = (): TerminalInfo[] | null => registry.getSnapshot().live[LOCAL_HOST] ?? null;

export const localMigrationDeps: MigrationDeps = {
  lock: (waitMs) => invoke<boolean>("local_migration_lock", { waitMs }).catch(() => false),
  unlock: () => invoke("local_migration_unlock").then(() => {}, () => {}),
  guard: () => invoke("local_migration_guard").then(() => {}),
  async read() {
    const s = await settings();
    // Another instance may have written, or deleted keys, since this one loaded the file.
    await s.reload({ ignoreDefaults: true });
    const [openTabs, openGroups, zoom] = await Promise.all([
      s.get<Tab[]>("open_tabs"),
      s.get<Group[]>("open_groups"),
      s.get<Record<string, number>>("terminal_zoom"),
    ]);
    return { openTabs, openGroups, zoom };
  },
  async write(l) {
    const s = await settings();
    await s.set("open_tabs", l.openTabs);
    await s.set("open_groups", l.openGroups);
    await s.set("terminal_zoom", l.zoom);
    await s.save();
    await invoke("local_migration_sync_settings");
  },
  readJournal: () => invoke<Journal | null>("local_migration_journal_read"),
  writeJournal: (journal) => invoke("local_migration_journal_write", { journal }).then(() => {}),
  clearJournal: () => invoke("local_migration_journal_clear").then(() => {}),
  async ready() {
    const ac = new AbortController();
    const timer = setTimeout(() => ac.abort(), READY_MS);
    const t0 = Date.now();
    try {
      await registry.waitUsable(LOCAL_HOST, ac.signal);
      return await waitForRegistry(localList, Math.max(0, READY_MS - (Date.now() - t0)));
    } catch (_) {
      return null;
    } finally {
      clearTimeout(timer);
    }
  },
  live: () => localList() ?? [],
  cached: () => cache.terminals(LOCAL_HOST),
  async listed(uuid, ms) {
    return (await waitForRegistry(() => (localList()?.some(i => i.terminal === uuid) ? true : null), ms)) === true;
  },
  async open(op) {
    // Throwaway sinks: the Tab attaches its own once it is shown.
    const onData = new Channel<ArrayBuffer>();
    const onExit = new Channel<unknown>();
    try {
      await invoke("host_term_open", { host: LOCAL_HOST, terminal: op.uuid, spec: op.spec, meta: op.meta, cols: 80, rows: 24, onData, onExit });
    } finally {
      await invoke("host_term_detach", { host: LOCAL_HOST, terminal: op.uuid }).catch(() => {});
    }
  },
  uuid: (tabId) => uuidV5(tabId, MIGRATION_NS),
  now: () => Date.now(),
};
