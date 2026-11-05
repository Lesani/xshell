import { describe, expect, it, vi } from "vitest";
import type { Tab } from "../types";
import { NO_MIGRATION } from "../hosts/localMigration";
import { captureCustomTitleTarget, type InProcessTitleTarget } from "./customTitle";
import { OpenTabsPersistence, openTabsIO, patchInProcessTitle, type OpenTabsSnapshot } from "./openTabsPersistence";

const store = vi.hoisted(() => ({ set: vi.fn(), save: vi.fn() }));
const invoke = vi.hoisted(() => vi.fn());
vi.mock("@tauri-apps/plugin-store", () => ({ load: vi.fn(async () => store) }));
vi.mock("@tauri-apps/api/core", () => ({ invoke, Channel: class {} }));
const original: Tab = { id: "a", type: "terminal", title: "Automatic", projectPath: "/p", createdAt: 1, agent: "pi" };
const target = captureCustomTitleTarget(original, {} as never) as InProcessTitleTarget;
const snapshot = (tabs: Tab[] = [original], over: Partial<OpenTabsSnapshot> = {}): OpenTabsSnapshot => ({ tabs, closing: new Set(), enabled: true, localDaemon: false, migration: NO_MIGRATION, ...over });
const deferred = () => { let resolve!: () => void; const promise = new Promise<void>(ok => { resolve = ok; }); return { promise, resolve }; };
const tick = async () => { for (let i = 0; i < 5; i++) await Promise.resolve(); };
function fixture() {
  let disk: Tab[] = [];
  const io = { set: vi.fn(async (tabs: Tab[]) => { disk = structuredClone(tabs); }), save: vi.fn(async () => {}), sync: vi.fn(async () => {}) };
  const writer = new OpenTabsPersistence(io);
  return { writer, io, get disk() { return disk; } };
}
function install(f: ReturnType<typeof fixture>, state = snapshot(), value: string | null = "Custom") {
  const pending = f.writer.request(target, value);
  const next = { ...state, tabs: patchInProcessTitle(state.tabs, target, value, state.closing) };
  f.writer.commit(next);
  return { pending, next };
}

describe("OpenTabsPersistence", () => {
  it("production_adapter_sets_existing_key_then_explicitly_saves_and_syncs", async () => {
    const order: string[] = [];
    store.set.mockImplementation(async (key: string) => { order.push(`set ${key}`); });
    store.save.mockImplementation(async () => { order.push("save"); });
    invoke.mockImplementation(async (cmd: string) => { order.push(cmd); });
    const writer = new OpenTabsPersistence(openTabsIO);
    writer.commit(snapshot()); await writer.idle(); order.length = 0;
    const pending = writer.request(target, "Custom");
    writer.commit(snapshot([{ ...original, customTitle: "Custom" }]));
    await pending;
    expect(order).toEqual(["set open_tabs", "save", "local_migration_sync_settings"]);
  });
  it("receipt_waits_for_a_committed_patch_and_all_three_IO_boundaries", async () => {
    const f = fixture(); f.writer.commit(snapshot()); await f.writer.idle();
    const save = deferred(), sync = deferred();
    f.io.save.mockReturnValue(save.promise); f.io.sync.mockReturnValue(sync.promise);
    const pending = f.writer.request(target, "Custom"); let done = false; void pending.then(() => { done = true; });
    await f.writer.idle(); expect(f.io.save).not.toHaveBeenCalled();
    f.writer.commit(snapshot([{ ...original, customTitle: "Custom" }]));
    await tick(); expect(f.io.save).toHaveBeenCalledTimes(1); expect(done).toBe(false);
    save.resolve(); await tick(); expect(f.io.sync).toHaveBeenCalledTimes(1); expect(done).toBe(false);
    sync.resolve(); await pending; expect(done).toBe(true);
  });
  it.each(["set", "save", "sync"] as const)("%s_failure_is_not_rollback_or_automatic_retry_and_queue_recovers", async stage => {
    const f = fixture(); f.writer.commit(snapshot()); await f.writer.idle();
    f.io[stage].mockRejectedValueOnce(new Error(`${stage} fault`));
    const { pending, next } = install(f);
    await expect(pending).rejects.toThrow(`${stage} fault`);
    expect(next.tabs[0].customTitle).toBe("Custom");
    const latest = { ...next, tabs: [{ ...next.tabs[0], title: "Latest", sessionId: "discovered", lastActiveAt: 9 }] };
    f.writer.commit(latest); await f.writer.idle();
    expect(f.disk[0]).toMatchObject({ customTitle: "Custom", title: "Latest", sessionId: "discovered", lastActiveAt: 9 });
    const retry = install(f, latest); await retry.pending;
    expect(f.io.sync).toHaveBeenCalled();
    const clear = install(f, retry.next, null); await clear.pending;
    expect(f.disk[0]).not.toHaveProperty("customTitle");
    const clearedRetry = install(f, clear.next, null); await clearedRetry.pending;
    expect(f.disk[0]).not.toHaveProperty("customTitle");
  });
  it("delayed_old_normal_write_cannot_overwrite_new_name_or_latest_membership", async () => {
    const f = fixture(), old = deferred();
    f.io.set.mockImplementationOnce(async () => { await old.promise; });
    f.writer.commit(snapshot()); await tick();
    const { pending, next } = install(f);
    const b: Tab = { id: "b", type: "terminal", title: "Other" };
    f.writer.commit({ ...next, tabs: [b, { ...next.tabs[0], title: "New auto", sessionId: "s", lastActiveAt: 22 }] });
    old.resolve(); await pending; await f.writer.idle();
    expect(f.disk.map(t => t.id)).toEqual(["b", "a"]);
    expect(f.disk[1]).toMatchObject({ title: "New auto", customTitle: "Custom", sessionId: "s", lastActiveAt: 22 });
    expect(f.io.set.mock.calls.slice(1).every(c => c[0][1]?.customTitle === "Custom")).toBe(true);
  });
  it("delayed_name_write_drains_newest_title_order_open_close_and_other_name_without_state_rollback", async () => {
    const f = fixture(); f.writer.commit(snapshot()); await f.writer.idle();
    const delay = deferred(); f.io.save.mockReturnValueOnce(delay.promise);
    const { pending, next } = install(f); await tick();
    const b: Tab = { id: "b", type: "terminal", title: "B", customTitle: "Other name" };
    f.writer.commit({ ...next, tabs: [b, { ...next.tabs[0], title: "Latest", sessionId: "s", groupId: "g", lastActiveAt: 42 }] });
    f.writer.commit({ ...next, tabs: [{ ...next.tabs[0], title: "Latest", sessionId: "s", groupId: "g", lastActiveAt: 42 }] });
    delay.resolve(); await pending; await f.writer.idle();
    expect(f.disk).toEqual([{ ...next.tabs[0], title: "Latest", sessionId: "s", groupId: "g", lastActiveAt: 42 }]);
  });
  it.each(["removed", "closing", "replacement", "daemon"])("%s_before_queued_write_cancels_receipt_and_never_reinserts_source", async change => {
    const f = fixture(), old = deferred();
    f.io.set.mockReturnValueOnce(old.promise); f.writer.commit(snapshot()); await tick();
    const { pending, next } = install(f);
    const cancelled = expect(pending).rejects.toThrow("no longer available");
    let current = snapshot();
    if (change === "removed") current = snapshot([]);
    if (change === "closing") current = snapshot(next.tabs, { closing: new Set(["a"]) });
    if (change === "replacement") current = snapshot([{ ...original, createdAt: 2 }]);
    if (change === "daemon") current = snapshot([{ ...original, terminal: "uuid" }]);
    f.writer.commit(current); await cancelled; old.resolve(); await f.writer.idle();
    expect(f.io.save).not.toHaveBeenCalled();
    expect(f.disk).toEqual(change === "daemon" ? [] : current.tabs);
  });
  it.each(["removed", "closing", "replacement", "daemon"])("%s_during_name_IO_cannot_get_a_success_receipt_or_revive_a_tab", async change => {
    const f = fixture(); f.writer.commit(snapshot()); await f.writer.idle();
    const delay = deferred(); f.io.save.mockReturnValueOnce(delay.promise);
    const { pending, next } = install(f); await tick();
    const cancelled = expect(pending).rejects.toThrow("no longer available");
    const tabs = change === "removed" ? [] : change === "replacement" ? [{ ...original, createdAt: 2 }]
      : change === "daemon" ? [{ ...original, terminal: "uuid" }] : next.tabs;
    f.writer.commit(snapshot(tabs, { closing: new Set(change === "closing" ? ["a"] : []) }));
    delay.resolve(); await cancelled; await f.writer.idle();
    expect(f.disk).toEqual(change === "daemon" ? [] : tabs);
  });
  it("failed_normal_queue_item_does_not_poison_an_explicit_save", async () => {
    const f = fixture(); f.io.set.mockRejectedValueOnce(new Error("normal failure"));
    f.writer.commit(snapshot()); await f.writer.idle();
    await install(f).pending; expect(f.disk[0].customTitle).toBe("Custom");
  });
  it("startup_disabled_nonowner_and_settlement_guards_refuse_explicit_writes_and_held_sources_are_merged", async () => {
    for (const over of [{ enabled: false }, { migration: { ...NO_MIGRATION, writesLayout: false } }, { localDaemon: true }]) {
      const f = fixture(); f.writer.commit(snapshot([original], over)); await f.writer.idle();
      expect(() => f.writer.request(target, "No")).toThrow("no longer available");
      expect(f.io.set).not.toHaveBeenCalled();
    }
    const f = fixture(), held: Tab = { ...original, id: "held", customTitle: "Held" };
    const state = snapshot([original], { localDaemon: true, migration: { ...NO_MIGRATION, ownsOpenTabs: true, heldBack: [held] } });
    f.writer.commit(state); await f.writer.idle(); await install(f, state).pending;
    expect(f.disk.map(t => [t.id, t.customTitle])).toEqual([["a", "Custom"], ["held", "Held"]]);
  });
  it("a_guard_enabled_after_registration_is_failure_not_success", async () => {
    const f = fixture(); f.writer.commit(snapshot()); await f.writer.idle();
    const pending = f.writer.request(target, "Custom");
    f.writer.commit(snapshot([{ ...original, customTitle: "Custom" }], { enabled: false }));
    await expect(pending).rejects.toThrow("no longer available"); expect(f.io.save).not.toHaveBeenCalled();
  });
  it("field_patch_preserves_concurrent_metadata_and_rejects_launch_identity_changes_not_session_discovery", () => {
    const discovered = { ...original, title: "Updated", sessionId: "s", lastActiveAt: 2, groupId: "g" };
    expect(patchInProcessTitle([discovered], target, "Custom", new Set())[0]).toEqual({ ...discovered, customTitle: "Custom" });
    for (const changed of [{ createdAt: 2 }, { projectPath: "/other" }, { agent: "codex" as const }, { shellMode: "raw" as const }, { shellId: "zsh" }, { host: "h" }, { terminal: "uuid" }]) {
      const replacement = { ...original, ...changed };
      expect(patchInProcessTitle([replacement], target, "Custom", new Set())[0]).toBe(replacement);
    }
    expect(patchInProcessTitle([original], target, "Custom", new Set(["a"]))[0]).toBe(original);
  });
  it("identical_StrictMode_commit_does_not_duplicate_IO_and_invalid_drafts_never_write", async () => {
    const f = fixture(), state = snapshot(); f.writer.commit(state); f.writer.commit({ ...state }); await f.writer.idle();
    expect(f.io.set).toHaveBeenCalledTimes(1);
    for (const value of ["", "\tname", "x".repeat(257)]) expect(() => f.writer.request(target, value)).toThrow();
    await f.writer.idle(); expect(f.io.set).toHaveBeenCalledTimes(1);
  });
});
