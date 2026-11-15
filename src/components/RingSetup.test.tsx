// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { RingSetup } from "./RingSetup";
import { C } from "../ring/copy";
import { FAR, WORDS, ringStatus } from "./ringTestKit";
import type { JoinEvent, JoinOverview, RingStatus } from "../ring/types";

const native = vi.hoisted(() => ({ invoke: vi.fn(), handlers: new Map<string, (event: { payload: unknown }) => void>(), settings: new Map<string, unknown>() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: native.invoke }));
vi.mock("@tauri-apps/plugin-store", () => ({ load: async () => ({
  get: async (k: string) => native.settings.get(k) ?? null,
  set: async (k: string, v: unknown) => { native.settings.set(k, v); },
  save: async () => {},
}) }));
vi.mock("@tauri-apps/api/event", () => ({ listen: async (name: string, handler: (event: { payload: unknown }) => void) => { native.handlers.set(name, handler); return () => native.handlers.delete(name); } }));

let container: HTMLDivElement;
let root: Root;
let commands: Record<string, (args: Record<string, unknown>) => unknown>;
let onClose: ReturnType<typeof vi.fn>;
let onDone: ReturnType<typeof vi.fn>;
const calls = () => native.invoke.mock.calls.map(([c]) => c as string);
const argsOf = (command: string) => native.invoke.mock.calls.filter(([c]) => c === command).map(([, a]) => a as Record<string, unknown>);

function setup(s: RingStatus, overview: JoinOverview = { owner: true, joins: [] }, extra: typeof commands = {}) {
  commands = { ring_status: () => s, ring_join_overview: () => overview, ring_join_cancel: () => undefined, ...extra };
  native.invoke.mockImplementation(async (command: string, args: Record<string, unknown>) => {
    const f = commands[command];
    if (!f) throw new Error(`unexpected ${command}`);
    return f(args ?? {});
  });
}
async function mount(props: Partial<Parameters<typeof RingSetup>[0]> = {}) {
  await act(async () => { root.render(<RingSetup variant="first" onClose={onClose} onDone={onDone} {...props} />); });
}
const emit = async (name: string, payload: unknown) => { await act(async () => { native.handlers.get(name)!({ payload }); }); };
const q = (sel: string) => container.querySelector<HTMLElement>(sel);
const click = async (sel: string) => { await act(async () => { q(sel)!.click(); }); };
const text = () => container.textContent ?? "";
const joinEvent = (operation: string, state: JoinEvent["state"], over: Partial<JoinEvent> = {}): JoinEvent => ({ operation, origin: "gui", state, expiresAt: FAR, ...over });
async function showWords(): Promise<string> {
  commands.ring_join_start = (a) => ({ operation: a.operation, words: WORDS, expiresAt: FAR });
  await click("[data-ring-join-action]");
  return argsOf("ring_join_start")[0].operation as string;
}

beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  native.invoke.mockReset();
  native.handlers.clear();
  native.settings.clear();
  onClose = vi.fn();
  onDone = vi.fn();
  container = document.createElement("div");
  document.body.append(container);
  root = createRoot(container);
});
afterEach(async () => {
  await act(async () => { root.unmount(); });
  container.remove();
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

describe("first launch", () => {
  it("shows the two choices and Skip, with distinct headings and buttons", async () => {
    setup(ringStatus());
    await mount();
    expect(q("h1")?.textContent).toBe(C["first.title"]);
    expect(text()).toContain(C["first.create.title"]);
    expect(text()).toContain(C["first.join.title"]);
    expect(q("[data-ring-create]")?.textContent).toBe(C["first.create.action"]);
    expect(q("[data-ring-join-action]")?.textContent).toBe(C["first.join.action"]);
    expect(q("[data-ring-skip]")?.textContent).toBe(C["first.skip"]);
  });

  it("Create Ring calls ring_create_computer once with the chosen relay; shows Creating then done", async () => {
    native.settings.set("ring_relay_url", "wss://own.example.com");
    let resolve!: (v: unknown) => void;
    setup(ringStatus(), undefined, { ring_create_computer: () => new Promise(r => { resolve = r; }) });
    await mount();
    await act(async () => {});
    await click("[data-ring-create]");
    await click("[data-ring-create]").catch(() => {});
    expect(text()).toContain(C["create.progress"]);
    await act(async () => { resolve(ringStatus({ membership: "member" })); });
    expect(argsOf("ring_create_computer")).toEqual([{ relayUrl: "wss://own.example.com" }]);
    expect(onDone).toHaveBeenCalledWith(C["create.done"]);
  });

  it("a double click starts one Create", async () => {
    let resolve!: (v: unknown) => void;
    setup(ringStatus(), undefined, { ring_create_computer: () => new Promise(r => { resolve = r; }) });
    await mount();
    await act(async () => { q("[data-ring-create]")!.click(); q("[data-ring-create]")?.click(); });
    await act(async () => { resolve(ringStatus({ membership: "member" })); });
    expect(argsOf("ring_create_computer")).toHaveLength(1);
  });

  it("Create offline shows create.offline with Retry, never the raw error", async () => {
    setup(ringStatus(), undefined, { ring_create_computer: () => { throw "offline: relay timed out (os error 110)"; } });
    await mount();
    await click("[data-ring-create]");
    expect(text()).toContain(C["create.offline"]);
    expect(q("[data-ring-create]")?.textContent).toBe(C["create.retry"]);
    expect(text()).not.toContain("os error");
    expect(text()).not.toContain("offline:");
  });

  it("Show words renders words and countdown; no Confirm or Decline button exists after a completing event", async () => {
    setup(ringStatus());
    await mount();
    const op = await showWords();
    const words = q("[data-ring-words]")!;
    expect(words.children[0].textContent).toBe("acid baker cable dance");
    expect(words.children[1].textContent).toBe("eagle fable gamma habit");
    expect(text()).toMatch(/Expires in \d+:\d\d/);
    expect(text()).toContain(C["join.lifetime"]);
    expect(text()).toContain(C["join.focus"]);
    expect(text()).toContain(C["join.waiting"]);
    await emit("ring:join", joinEvent(op, "completing"));
    expect(q("[data-ring-words]")).toBeNull();
    expect(text()).toContain(C["join.finishing"]);
    const labels = Array.from(container.querySelectorAll("button")).map(b => b.textContent);
    expect(labels).not.toContain("Confirm");
    expect(labels).not.toContain("Decline");
    await emit("ring:join", joinEvent(op, "installed"));
    expect(onDone).toHaveBeenCalledWith(C["join.done"]);
  });

  it("Cancel calls ring_join_cancel and returns to choices", async () => {
    setup(ringStatus());
    await mount();
    const op = await showWords();
    await click("[data-ring-cancel]");
    expect(argsOf("ring_join_cancel")).toEqual([{ operation: op }]);
    expect(q("[data-ring-words]")).toBeNull();
    expect(q("[data-ring-create]")).not.toBeNull();
  });

  for (const how of ["unmount", "visible=false", "pagehide"] as const) {
    it(`${how} cancels the shown words`, async () => {
      setup(ringStatus());
      await mount();
      const op = await showWords();
      if (how === "unmount") { await act(async () => { root.unmount(); }); root = createRoot(container); }
      if (how === "visible=false") await mount({ visible: false });
      if (how === "pagehide") await act(async () => { window.dispatchEvent(new Event("pagehide")); });
      expect(argsOf("ring_join_cancel")).toEqual([{ operation: op }]);
    });
  }

  it("losing OS focus does not cancel the words", async () => {
    setup(ringStatus());
    await mount();
    await showWords();
    await act(async () => { window.dispatchEvent(new Event("blur")); document.dispatchEvent(new Event("visibilitychange")); });
    expect(calls()).not.toContain("ring_join_cancel");
    expect(q("[data-ring-words]")).not.toBeNull();
  });

  it("a start reply after Cancel is not rendered and is cancelled again (U1)", async () => {
    let resolve!: (v: unknown) => void;
    setup(ringStatus(), undefined, { ring_join_start: () => new Promise(r => { resolve = r; }) });
    await mount();
    await click("[data-ring-join-action]");
    const op = argsOf("ring_join_start")[0].operation as string;
    await click("[data-ring-cancel]");
    expect(argsOf("ring_join_cancel")).toEqual([{ operation: op }]);
    await act(async () => { resolve({ operation: op, words: WORDS, expiresAt: FAR }); });
    expect(q("[data-ring-words]")).toBeNull();
    expect(argsOf("ring_join_cancel")).toEqual([{ operation: op }, { operation: op }]);
  });

  it("a start reply after unmount is not rendered and is cancelled (U1)", async () => {
    let resolve!: (v: unknown) => void;
    setup(ringStatus(), undefined, { ring_join_start: () => new Promise(r => { resolve = r; }) });
    await mount();
    await click("[data-ring-join-action]");
    const op = argsOf("ring_join_start")[0].operation as string;
    await act(async () => { root.unmount(); });
    root = createRoot(container);
    await act(async () => { resolve({ operation: op, words: WORDS, expiresAt: FAR }); });
    expect(container.textContent).not.toContain("acid");
    expect(argsOf("ring_join_cancel").map(a => a.operation)).toContain(op);
  });

  it("backend cancelled shows join.cancelled with New words", async () => {
    setup(ringStatus());
    await mount();
    const op = await showWords();
    await emit("ring:join", joinEvent(op, "cancelled"));
    expect(text()).toContain(C["join.cancelled"]);
    await click("[data-ring-new-code]");
    expect(argsOf("ring_join_start")).toHaveLength(2);
    expect(argsOf("ring_join_start")[1].operation).not.toBe(op);
  });

  it("Skip stores ring_setup_skipped=true and calls onClose", async () => {
    setup(ringStatus());
    await mount();
    await click("[data-ring-skip]");
    await act(async () => {});
    expect(native.settings.get("ring_setup_skipped")).toBe(true);
    expect(onClose).toHaveBeenCalledOnce();
  });

  it("a follower window says so and offers no action", async () => {
    setup(ringStatus({ connection: "other-window" }), { owner: false, joins: [] });
    await mount();
    expect(text()).toContain(C["ring.follower"]);
    expect(q("[data-ring-create]")!.hasAttribute("disabled")).toBe(true);
    expect(q("[data-ring-join-action]")!.hasAttribute("disabled")).toBe(true);
  });
});

describe("registration wait (first start)", () => {
  it("not-registered: Create waits and calls the command once registration clears", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    let overview: JoinOverview = { owner: true, unavailable: "not-registered", joins: [] };
    setup(ringStatus(), undefined, { ring_join_overview: () => overview, ring_create_computer: () => ringStatus({ membership: "member" }) });
    await mount();
    await click("[data-ring-create]");
    expect(text()).toContain(C["create.progress"]);
    expect(calls()).not.toContain("ring_create_computer");
    await act(async () => { await vi.advanceTimersByTimeAsync(2000); });
    expect(calls()).not.toContain("ring_create_computer");
    overview = { owner: true, joins: [] };
    await act(async () => { await vi.advanceTimersByTimeAsync(2000); });
    expect(argsOf("ring_create_computer")).toHaveLength(1);
  });

  it("leaving during the registration wait calls nothing afterwards", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    let overview: JoinOverview = { owner: true, unavailable: "not-registered", joins: [] };
    setup(ringStatus(), undefined, { ring_join_overview: () => overview, ring_join_start: (a) => ({ operation: a.operation, words: WORDS, expiresAt: FAR }) });
    await mount();
    await click("[data-ring-join-action]");
    await act(async () => { root.unmount(); });
    root = createRoot(container);
    overview = { owner: true, joins: [] };
    await act(async () => { await vi.advanceTimersByTimeAsync(10_000); });
    expect(calls()).not.toContain("ring_join_start");
    expect(calls()).not.toContain("ring_join_cancel");
  });

  it("registration that never clears ends with create.notRegistered after 30 s", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    setup(ringStatus(), { owner: true, unavailable: "not-registered", joins: [] }, { ring_create_computer: () => ringStatus() });
    await mount();
    await click("[data-ring-create]");
    await act(async () => { await vi.advanceTimersByTimeAsync(31_000); });
    expect(calls()).not.toContain("ring_create_computer");
    expect(text()).toContain(C["create.notRegistered"]);
  });
});

describe("ring:status is authoritative (U3)", () => {
  it("finishing shows Finishing… with no buttons; finishing → none returns to the two choices with finish.notAdded", async () => {
    setup(ringStatus({ membership: "finishing", finishingOffline: true }));
    await mount();
    expect(text()).toContain(C["finish.title"]);
    expect(text()).toContain(C["finish.offline"]);
    expect(container.querySelectorAll("button")).toHaveLength(0);
    await emit("ring:status", ringStatus({ membership: "none" }));
    expect(q("[data-ring-not-added]")?.textContent).toBe(C["finish.notAdded"]);
    expect(q("[data-ring-create]")).not.toBeNull();
    expect(q("[data-ring-join-action]")).not.toBeNull();
    expect(q("[data-ring-retry]")).toBeNull();
    expect(text()).not.toContain(C["join.failed"]);
  });

  it("finish.notAdded still shows after a remount (the finishing operation is remembered)", async () => {
    setup(ringStatus({ membership: "finishing" }));
    await mount();
    await act(async () => {});
    expect(native.settings.get("ring_finishing_pending")).toBe(true);
    await act(async () => { root.unmount(); });
    root = createRoot(container);
    setup(ringStatus({ membership: "none" }));
    await mount();
    await act(async () => {});
    expect(q("[data-ring-not-added]")?.textContent).toBe(C["finish.notAdded"]);
    // Starting again clears it.
    commands.ring_join_start = (a) => ({ operation: a.operation, words: WORDS, expiresAt: FAR });
    await click("[data-ring-join-action]");
    await act(async () => {});
    expect(native.settings.get("ring_finishing_pending")).toBe(false);
  });

  it("a removal during Approved. Finishing… shows the removed screen; Join again then works", async () => {
    setup(ringStatus());
    await mount();
    const op = await showWords();
    await emit("ring:join", joinEvent(op, "completing"));
    expect(text()).toContain(C["join.finishing"]);
    await emit("ring:status", ringStatus({ membership: "removed", removed: { ringId: "r", at: 11 } }));
    expect(q("h1")?.textContent).toBe(C["removed.title"]);
    expect(text()).not.toContain(C["join.finishing"]);
    await click("[data-ring-join-action]");
    expect(argsOf("ring_join_start")).toHaveLength(2);
    expect(q("[data-ring-words]")).not.toBeNull();
  });

  it("finishing → member shows done", async () => {
    setup(ringStatus({ membership: "finishing" }));
    await mount();
    await emit("ring:status", ringStatus({ membership: "member" }));
    expect(onDone).toHaveBeenCalledWith(C["join.done"]);
  });

  it("a removal arriving while the words show cancels them and shows the removed screen", async () => {
    setup(ringStatus());
    await mount();
    const op = await showWords();
    await emit("ring:status", ringStatus({ membership: "removed", removed: { ringId: "r", at: 9 } }));
    expect(argsOf("ring_join_cancel")).toEqual([{ operation: op }]);
    expect(q("h1")?.textContent).toBe(C["removed.title"]);
  });
});

describe("removed and old Ring data", () => {
  it("removed: Join again starts ring_join_start; Not now closes", async () => {
    setup(ringStatus({ membership: "removed", removed: { ringId: "r", at: 5 } }), undefined, { ring_join_start: (a) => ({ operation: a.operation, words: WORDS, expiresAt: FAR }) });
    await mount({ variant: "removed" });
    expect(text()).toContain(C["removed.title"]);
    expect(text()).toContain(C["removed.body"]);
    expect(q("[data-ring-skip]")?.textContent).toBe(C["removed.dismiss"]);
    await click("[data-ring-join-action]");
    expect(argsOf("ring_join_start")).toHaveLength(1);
    expect(q("[data-ring-words]")).not.toBeNull();
  });

  it("initial join (Join again from a notice) starts the words at once", async () => {
    setup(ringStatus({ membership: "removed", removed: { ringId: "r", at: 5 } }), undefined, { ring_join_start: (a) => ({ operation: a.operation, words: WORDS, expiresAt: FAR }) });
    await mount({ variant: "removed", initial: "join" });
    await act(async () => {});
    expect(argsOf("ring_join_start")).toHaveLength(1);
  });

  it("old: its notice and choices; Skip remembers this old Ring as seen", async () => {
    setup(ringStatus({ oldRing: { at: 77 } }));
    await mount({ variant: "old" });
    expect(q("h1")?.textContent).toBe(C["old.title"]);
    expect(text()).toContain(C["old.body"]);
    expect(text()).toContain(C["old.local"]);
    await click("[data-ring-skip]");
    await act(async () => {});
    expect(native.settings.get("ring_old_seen_at")).toBe(77);
    expect(native.settings.has("ring_setup_skipped")).toBe(false);
  });
});

describe("no dead UI", () => {
  it("never shows consent, upgrade, leave or in-process wording", async () => {
    for (const s of [ringStatus(), ringStatus({ membership: "removed", removed: { ringId: "r", at: 1 } }), ringStatus({ membership: "finishing" }), ringStatus({ oldRing: { at: 1 } })]) {
      setup(s);
      await mount({ variant: s.oldRing ? "old" : s.membership === "removed" ? "removed" : "first" });
      for (const dead of ["Join this Ring?", "Decline", "Upgrade Ring", "Leave this Ring", "inside the app", "Confirm"]) expect(text(), dead).not.toContain(dead);
      await act(async () => { root.unmount(); });
      root = createRoot(container);
    }
  });
});
