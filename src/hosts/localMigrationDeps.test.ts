import { beforeEach, describe, expect, it, vi } from "vitest";
const invoke = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...a: unknown[]) => invoke(...a), Channel: class { onmessage = () => {}; } }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
const store = { reload: vi.fn(), get: vi.fn(), set: vi.fn(), delete: vi.fn(), save: vi.fn() };
vi.mock("@tauri-apps/plugin-store", () => ({ load: vi.fn(async () => store) }));

import { localMigrationDeps } from "./localMigrationDeps";
import type { MigrationOp } from "./localMigration";

const op: MigrationOp = { fromId: "a", uuid: "u-a", spec: { cwd: "/p" }, meta: { title: "A" } };

describe("localMigrationDeps", () => {
  beforeEach(() => { invoke.mockReset(); for (const f of Object.values(store)) f.mockReset(); });

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
