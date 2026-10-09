import { beforeEach, describe, expect, it, vi } from "vitest";

type Handler = (e: { payload: unknown }) => void;
const handlers: Record<string, Handler> = {};
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async (name: string, h: Handler) => { handlers[name] = h; return () => { delete handlers[name]; }; }) }));

import { LocalAgentStatusStore } from "./localAgentStatus";

describe("LocalAgentStatusStore", () => {
  let store: LocalAgentStatusStore;
  beforeEach(() => { store = new LocalAgentStatusStore(); });

  it("event updates and null clears", async () => {
    let calls = 0;
    store.subscribe(() => { calls++; });
    await store.start();
    const fire = (payload: unknown) => handlers["local:agent-status"]({ payload });
    fire({ id: "t1", status: "working", seq: 0 });
    fire({ id: "t2", status: "needs-you", seq: 1 });
    expect([...store.getSnapshot()]).toEqual([["t1", "working"], ["t2", "needs-you"]]);
    const before = store.getSnapshot();
    fire({ id: "t1", status: null, seq: 2 });
    expect(store.getSnapshot().get("t1")).toBeUndefined();
    expect(store.getSnapshot().get("t2")).toBe("needs-you");
    // A new snapshot per change, so React re-renders.
    expect(store.getSnapshot()).not.toBe(before);
    expect(calls).toBe(3);
  });

  it("an older event never overwrites a newer one", () => {
    // A Relaunch's reset (seq 5) delivered before the old run's last report (seq 4).
    store.apply({ id: "t", status: null, seq: 5 });
    store.apply({ id: "t", status: "finished", seq: 4 });
    expect(store.getSnapshot().get("t")).toBeUndefined();
    store.apply({ id: "t", status: "working", seq: 6 });
    store.apply({ id: "t", status: "ended", seq: 6 });
    expect(store.getSnapshot().get("t")).toBe("working");
  });

  it("unknown statuses read as none", () => {
    store.apply({ id: "t", status: "working", seq: 1 });
    store.apply({ id: "t", status: "from-the-future", seq: 2 });
    expect(store.getSnapshot().has("t")).toBe(false);
  });

  it("an unchanged status keeps the snapshot", () => {
    store.apply({ id: "t", status: "working", seq: 1 });
    const s = store.getSnapshot();
    store.apply({ id: "t", status: "working", seq: 2 });
    expect(store.getSnapshot()).toBe(s);
  });
});
