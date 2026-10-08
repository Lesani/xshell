import { describe, expect, it } from "vitest";
import { MetaSync, localEdits } from "./metaSync";
import { reconcile, applyReconcile, tabFromTerminal } from "./reconcile";
import type { Tab } from "../types";
import type { TerminalInfo } from "./types";

const H = "h_ab12cd34";
const info = (uuid: string, sessionId: string | null, title?: string): TerminalInfo =>
  ({ terminal: uuid, spec: { cwd: "/p", agent: "codex", sessionId }, meta: title ? { title } : {}, createdAtMs: 1, pid: 1, exitCode: null });

describe("metaSync", () => {
  it("emits sessionId when a linked tab differs (title-sync link)", () => {
    const m = new MetaSync();
    const before = tabFromTerminal(H, info("u", null, "New Chat"));
    const after: Tab = { ...before, sessionId: "s1", title: "Refactor" };
    for (const e of localEdits([before], [after])) m.markDirty(e.tabId, e.field, e.value);
    const ups = m.takeUpdates([after]);
    expect(ups).toHaveLength(1);
    expect(ups[0]).toMatchObject({ host: H, terminal: "u", sessionId: "s1", meta: { title: "Refactor" } });
    // in flight: not sent twice
    expect(m.takeUpdates([after])).toEqual([]);
  });

  it("emits meta.title on rename", () => {
    const m = new MetaSync();
    const t = tabFromTerminal(H, info("u", "s1", "Old"));
    for (const e of localEdits([t], [{ ...t, title: "New" }])) m.markDirty(e.tabId, e.field, e.value);
    expect(m.takeUpdates([{ ...t, title: "New" }])[0]).toMatchObject({ meta: { title: "New" } });
    expect(m.takeUpdates([{ ...t, title: "New" }])[0]?.sessionId).toBeUndefined();
  });

  it("nothing when equal", () => {
    const m = new MetaSync();
    const t = tabFromTerminal(H, info("u", "s1", "Same"));
    expect(localEdits([t], [{ ...t }])).toEqual([]);
    expect(m.takeUpdates([t])).toEqual([]);
  });

  it("nothing for local tabs", () => {
    const local: Tab = { id: "terminal-1", type: "terminal", title: "A" };
    expect(localEdits([local], [{ ...local, title: "B", sessionId: "s" }])).toEqual([]);
    const m = new MetaSync();
    m.markDirty("terminal-1", "title", "B");
    expect(m.takeUpdates([{ ...local, title: "B" }])).toEqual([]);
  });

  it("echo clears the entry; failure retries once then drops", () => {
    const m = new MetaSync();
    const t = tabFromTerminal(H, info("u", "s1", "Old"));
    m.markDirty(t.id, "title", "New");
    let u = m.takeUpdates([t])[0];
    m.settled(u, true);
    m.observe(H, [info("u", "s1", "New")], [t]);
    expect(m.isDirty(t.id, "title")).toBe(false);

    m.markDirty(t.id, "title", "X");
    u = m.takeUpdates([t])[0];
    m.settled(u, false);
    expect(m.isDirty(t.id, "title")).toBe(true);
    u = m.takeUpdates([t])[0];
    expect(u.meta).toEqual({ title: "X" });
    m.settled(u, false);
    expect(m.isDirty(t.id, "title")).toBe(false);
    expect(m.takeUpdates([t])).toEqual([]);
  });
});

// Two Desktops (A, B) on one Terminal. The daemon list is the only channel between them.
describe("metaSync without echo loops (amendment 17)", () => {
  function desktop() {
    const m = new MetaSync();
    let tabs: Tab[] = [];
    const sent: { title?: string; sessionId?: string }[] = [];
    return {
      m, sent,
      get tabs() { return tabs; },
      receive(list: TerminalInfo[]) {
        m.observe(H, list, tabs);
        const d = reconcile(tabs, H, list, new Set(), (id, f) => m.isDirty(id, f));
        tabs = applyReconcile({ tabs, groups: [], activeLeafByGroup: {} }, d).tabs;
        this.flush();
      },
      edit(patch: Partial<Tab>) {
        const next = tabs.map(t => ({ ...t, ...patch }));
        for (const e of localEdits(tabs, next)) m.markDirty(e.tabId, e.field, e.value);
        tabs = next;
        this.flush();
      },
      flush() {
        for (const u of m.takeUpdates(tabs)) { sent.push({ title: u.meta?.title, sessionId: u.sessionId }); m.settled(u, true); }
      },
    };
  }

  it("incoming rename/branch from another Desktop is not echoed", () => {
    const a = desktop();
    a.receive([info("u", "s1", "Start")]);
    a.receive([info("u", "s2", "Renamed elsewhere")]);
    expect(a.tabs[0]).toMatchObject({ title: "Renamed elsewhere", sessionId: "s2" });
    expect(a.sent).toEqual([]);
  });

  it("local edit interleaved with an incoming one converges to the last writer without loops", () => {
    const a = desktop();
    const b = desktop();
    let daemon = [info("u", "s1", "Start")];
    a.receive(daemon); b.receive(daemon);
    // A renames locally; before its update reaches the daemon, B's rename lands.
    a.edit({ title: "From A" });
    b.edit({ title: "From B" });
    daemon = [info("u", "s1", "From B")];      // B's update applied first
    a.receive(daemon); b.receive(daemon);
    daemon = [info("u", "s1", "From A")];      // then A's (last writer)
    a.receive(daemon); b.receive(daemon);
    expect(a.tabs[0].title).toBe("From A");
    expect(b.tabs[0].title).toBe("From A");
    expect(a.sent).toEqual([{ title: "From A", sessionId: undefined }]);
    expect(b.sent).toEqual([{ title: "From B", sessionId: undefined }]);
    // nothing further is sent: no loop
    a.receive(daemon); b.receive(daemon);
    expect(a.sent).toHaveLength(1);
    expect(b.sent).toHaveLength(1);
    expect(a.m.isDirty(a.tabs[0].id, "title")).toBe(false);
  });
});
