import { beforeEach, describe, expect, it, vi } from "vitest";
const invoke = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...a: unknown[]) => invoke(...a), Channel: class { onmessage = () => {}; } }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
const store = { reload: vi.fn(), get: vi.fn(), set: vi.fn(), delete: vi.fn(), save: vi.fn() };
vi.mock("@tauri-apps/plugin-store", () => ({ load: vi.fn(async () => store) }));

import { registry, type RegistrySnapshot } from "./registry";
import type { HostStatus } from "./types";
import { localMigrationDeps } from "./localMigrationDeps";
import { migrateLocalTabs, type Journal, type MigrationOp } from "./localMigration";
import type { Tab } from "../types";

const op: MigrationOp = { fromId: "a", uuid: "u-a", spec: { cwd: "/p" }, meta: { title: "A" } };

describe("localMigrationDeps", () => {
  beforeEach(() => { vi.restoreAllMocks(); invoke.mockReset(); for (const f of Object.values(store)) f.mockReset(); });


  it("name_handoff_uses_existing_update_only_after_capability_and_live_target_checks", async () => {
    const status = { status: "connected", configGeneration: 2, daemonCapabilities: ["term.custom-title"] } as HostStatus;
    let snap = { configs: [], status: { local: status }, live: { local: [{ terminal: "u-a" }] } } as unknown as RegistrySnapshot;
    vi.spyOn(registry, "getSnapshot").mockImplementation(() => snap);
    await localMigrationDeps.updateName("u-a", "Local name");
    expect(invoke).toHaveBeenCalledExactlyOnceWith("host_term_update", { host: "local", terminal: "u-a", meta: { customTitle: "Local name" } });
    for (const statusChange of [{ daemonCapabilities: [] }, { status: "offline" }]) {
      invoke.mockClear(); snap = { ...snap, status: { local: { ...status, ...statusChange } as HostStatus } };
      await expect(localMigrationDeps.updateName("u-a", "No")).rejects.toThrow("unavailable"); expect(invoke).not.toHaveBeenCalled();
    }
    snap = { ...snap, status: { local: status }, live: { local: [] } };
    await expect(localMigrationDeps.updateName("u-a", "No")).rejects.toThrow("unavailable"); expect(invoke).not.toHaveBeenCalled();
    snap = { ...snap, live: { local: [{ terminal: "u-a" }] as never } };
    invoke.mockImplementationOnce(async () => { snap = { ...snap, status: { local: { ...status, configGeneration: 3 } } }; });
    await expect(localMigrationDeps.updateName("u-a", "Changed backend")).rejects.toThrow("changed during name handoff");
  });

  it("named_migration_runs_real_open_update_journal_and_settings_adapters_in_durable_order", async () => {
    const source: Tab = { id: "a", type: "terminal", title: "Auto", sessionId: "s1", projectPath: "/p", customTitle: "Local name" };
    const status = { status: "connected", configGeneration: 2, daemonCapabilities: ["term.custom-title"] } as HostStatus;
    const snap = { configs: [], status: { local: status }, live: { local: [] } } as unknown as RegistrySnapshot;
    vi.spyOn(registry, "getSnapshot").mockReturnValue(snap);
    let journal: Journal | null = null;
    const values: Record<string, unknown> = { open_tabs: [source], open_groups: [], terminal_zoom: {} }, order: string[] = [];
    store.get.mockImplementation(async (key: string) => values[key]);
    store.set.mockImplementation(async (key: string, value: unknown) => { order.push(`set ${key}`); values[key] = value; });
    store.save.mockImplementation(async () => { order.push("save"); });
    invoke.mockImplementation(async (cmd: string, args?: Record<string, unknown>) => {
      if (cmd === "local_migration_journal_read") return structuredClone(journal);
      if (cmd === "local_migration_lock") return true;
      if (cmd === "local_migration_journal_write") { journal = structuredClone(args!.journal as Journal); order.push(`journal ${journal.names?.a.state ?? "base"}`); }
      if (cmd === "host_term_open") {
        expect(args!.meta).not.toHaveProperty("customTitle"); order.push("open");
        snap.live.local = [{ terminal: "u-a", spec: args!.spec, meta: args!.meta, createdAtMs: 1, pid: 1, exitCode: null }] as never;
      }
      if (cmd === "host_term_detach") order.push("detach");
      if (cmd === "host_term_update") {
        expect(journal?.names?.a.state).toBe("attempted"); order.push("update"); snap.live.local![0].meta.customTitle = "Local name";
      }
      if (cmd === "local_migration_sync_settings") order.push("sync");
      if (cmd === "local_migration_journal_clear") { order.push("clear"); journal = null; }
      if (cmd === "local_migration_unlock") order.push("unlock");
    });
    const out = await migrateLocalTabs({ openTabs: [source], openGroups: [], zoom: {} }, { fullscreenRendering: true, forceSyncOutput: true }, {
      ...localMigrationDeps, ready: async () => [], live: () => snap.live.local!,
      listed: async () => { order.push("list"); return true; }, uuid: async () => "u-a",
    });
    expect(out.heldBack).toEqual([]); expect(values.open_tabs).toEqual([]); await out.settle();
    expect(order).toEqual(["journal base", "open", "detach", "list", "journal attempted", "update", "journal acknowledged",
      "set open_tabs", "set open_groups", "set terminal_zoom", "save", "sync", "clear", "unlock"]);
    expect(out.migrated[0].meta.customTitle).toBe("Local name");
  });

  it("M2: open detaches its temporary sinks on success and on failure", async () => {
    invoke.mockImplementation(async (cmd: string) => { if (cmd === "host_term_open") return { pid: 1 }; });
    await localMigrationDeps.open(op);
    expect(invoke.mock.calls.map(c => c[0])).toEqual(["host_term_open", "host_term_detach"]);
    expect(invoke.mock.calls[0][1]).toMatchObject({ host: "local", terminal: "u-a", spec: op.spec, meta: op.meta });
    invoke.mockReset();
    invoke.mockImplementation(async (cmd: string) => { if (cmd === "host_term_open") throw { code: "timeout", message: "t" }; });
    await expect(localMigrationDeps.open(op)).rejects.toMatchObject({ code: "timeout" });
    expect(invoke.mock.calls.map(c => c[0])).toEqual(["host_term_open", "host_term_detach"]);
  });

  it("write sets the three keys, saves once, then fsyncs", async () => {
    const order: string[] = [];
    store.set.mockImplementation(async (k: string) => { order.push(`set ${k}`); });
    store.save.mockImplementation(async () => { order.push("save"); });
    invoke.mockImplementation(async (cmd: string) => { order.push(cmd); });
    await localMigrationDeps.write({ openTabs: [], openGroups: [], zoom: {} });
    // 1: the save is made durable before the journal can be retired.
    expect(order).toEqual(["set open_tabs", "set open_groups", "set terminal_zoom", "save", "local_migration_sync_settings"]);
  });

  it("A: the journal and the guard go through the Rust commands", async () => {
    invoke.mockResolvedValue(null);
    await localMigrationDeps.writeJournal({ version: 1, sent: ["a"] });
    expect(await localMigrationDeps.readJournal()).toBeNull();
    await localMigrationDeps.clearJournal();
    await localMigrationDeps.guard();
    expect(invoke.mock.calls).toEqual([
      ["local_migration_journal_write", { journal: { version: 1, sent: ["a"] } }],
      ["local_migration_journal_read"],
      ["local_migration_journal_clear"],
      ["local_migration_guard"],
    ]);
  });

  it("E: read reloads from disk ignoring defaults, so a key deleted on disk reads as gone", async () => {
    // A fake store: the cache still has the key, the disk no longer does.
    let cache: Record<string, unknown> = { open_tabs: [{ id: "x" }], open_groups: [] };
    const disk: Record<string, unknown> = { open_groups: [] };
    store.reload.mockImplementation(async (o?: { ignoreDefaults?: boolean }) => { cache = o?.ignoreDefaults ? { ...disk } : { ...cache, ...disk }; });
    store.get.mockImplementation(async (k: string) => cache[k]);
    const r = await localMigrationDeps.read();
    expect(store.reload).toHaveBeenCalledWith({ ignoreDefaults: true });
    expect(r.openTabs).toBeUndefined();
    invoke.mockRejectedValue("no app data dir");
    expect(await localMigrationDeps.lock(2000)).toBe(false);
  });
});
