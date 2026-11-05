// @vitest-environment happy-dom
import { act, StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createWorkingRingClock, useWorkingRingClock } from "./useWorkingRingClock";

let now: number, nextId: number, visibility: string | undefined;
let pending: Map<number, () => void>, media: MediaQueryList;
let mediaListeners: Set<EventListenerOrEventListenerObject>;
let root: HTMLDivElement;
let cleanups: (() => void)[];
const time = {
  now: () => now,
  setTimeout: vi.fn((callback: () => void, delay: number) => {
    expect(delay).toBe(100);
    const id = ++nextId;
    pending.set(id, callback);
    return id;
  }),
  clearTimeout: vi.fn((id: number) => { pending.delete(id); }),
};
function fire(at: number) {
  now = at;
  const [id, callback] = [...pending][0];
  pending.delete(id);
  callback();
}
function changeVisibility(value: string | undefined) {
  visibility = value;
  document.dispatchEvent(new Event("visibilitychange"));
}
function changeMotion(reduce: boolean) {
  Object.defineProperty(media, "matches", { configurable: true, value: reduce });
  for (const listener of mediaListeners) {
    if (typeof listener === "function") listener(new Event("change"));
    else listener.handleEvent(new Event("change"));
  }
}
function controller() {
  const clock = createWorkingRingClock(root, time);
  cleanups.push(() => clock.dispose());
  return clock;
}
const angle = () => root.style.getPropertyValue("--tab-attention-angle");
beforeEach(() => {
  now = 0; nextId = 0; visibility = "visible"; pending = new Map(); cleanups = [];
  mediaListeners = new Set();
  media = { matches: false,
    addEventListener: vi.fn((_event, listener) => { mediaListeners.add(listener); }),
    removeEventListener: vi.fn((_event, listener) => { mediaListeners.delete(listener); }),
  } as unknown as MediaQueryList;
  vi.spyOn(window, "matchMedia").mockReturnValue(media);
  vi.spyOn(document, "visibilityState", "get").mockImplementation(() => visibility as DocumentVisibilityState);
  root = document.createElement("div");
  root.innerHTML = '<span class="tab-attention-working"></span>';
  document.body.append(root);
  time.setTimeout.mockClear(); time.clearTimeout.mockClear();
});
afterEach(() => {
  for (const cleanup of cleanups) cleanup();
  root.remove();
  vi.restoreAllMocks(); vi.unstubAllGlobals();
});

describe("TabBar Working ring clock", () => {
  it("skips delayed phases in one write, never bursts, and does not rewrite unchanged phases", () => {
    controller();
    const write = vi.spyOn(root.style, "setProperty");
    fire(350);
    expect(angle()).toBe("90deg");
    expect(write).toHaveBeenCalledExactlyOnceWith("--tab-attention-angle", "90deg");
    expect(pending.size).toBe(1);
    fire(1550); // Same phase one rotation later: no redundant style write.
    expect(write).toHaveBeenCalledTimes(1);
    fire(1700);
    expect(angle()).toBe("150deg");
    expect(write).toHaveBeenCalledTimes(2);
    expect(time.setTimeout).toHaveBeenCalledTimes(4);
    expect(pending.size).toBe(1);
  });

  it("holds phase while hidden and resumes without charging hidden wall time or a burst", () => {
    controller(); fire(200);
    const write = vi.spyOn(root.style, "setProperty");
    now = 250; changeVisibility("hidden");
    expect(pending.size).toBe(0);
    expect(angle()).toBe("60deg");
    now = 10000; changeVisibility("visible");
    expect(write).not.toHaveBeenCalled();
    expect(pending.size).toBe(1);
    fire(10100);
    expect(angle()).toBe("90deg");
    expect(write).toHaveBeenCalledTimes(1);
  });

  it.each([undefined, "unknown", "visible"])("keeps nonhidden %s and visible-unfocused live", value => {
    vi.spyOn(document, "hasFocus").mockReturnValue(false);
    visibility = value;
    controller();
    window.dispatchEvent(new Event("blur"));
    fire(100);
    expect(angle()).toBe("30deg");
    expect(pending.size).toBe(1);
  });

  it("initial and changed reduced-motion stop writes and timers, then resume on the same phase", () => {
    changeMotion(true);
    const write = vi.spyOn(root.style, "setProperty");
    controller();
    expect(pending.size).toBe(0); expect(write).not.toHaveBeenCalled();
    changeMotion(false); fire(100);
    expect(angle()).toBe("30deg");
    write.mockClear(); changeMotion(true);
    expect(pending.size).toBe(0); expect(write).not.toHaveBeenCalled();
    now = 5000; changeMotion(false);
    expect(angle()).toBe("30deg"); expect(write).not.toHaveBeenCalled();
    fire(5100); expect(angle()).toBe("60deg");
  });

  it("subscribes to media before reading its initial state", () => {
    vi.mocked(media.addEventListener).mockImplementation((_event, listener) => {
      mediaListeners.add(listener);
      changeMotion(true);
    });
    controller();
    expect(pending.size).toBe(0); expect(angle()).toBe("");
  });

  it("missing media API falls back to a static Working indicator without throwing", () => {
    vi.stubGlobal("matchMedia", undefined);
    expect(controller).not.toThrow();
    expect(root.querySelector(".tab-attention-working")).not.toBeNull();
    expect(pending.size).toBe(0); expect(angle()).toBe("");
    expect(root.getAttribute("data-attention-motion-paused")).toBe("true");
  });

  it("no rendered Working means zero callbacks/writes, including after events and repeated reconciliation", () => {
    root.innerHTML = "<span></span>";
    const write = vi.spyOn(root.style, "setProperty");
    const clock = controller();
    clock.reconcile(); changeVisibility("hidden"); changeVisibility("visible");
    changeMotion(true); changeMotion(false);
    expect(pending.size).toBe(0); expect(write).not.toHaveBeenCalled();
    expect(time.setTimeout).not.toHaveBeenCalled();
  });

  it("queued callbacks cannot write or reschedule after pause, membership reset, or disposal", () => {
    const clock = controller();
    const stale = [...pending.values()][0];
    changeVisibility("hidden");
    now = 100; stale();
    expect(angle()).toBe("0deg"); expect(pending.size).toBe(0);
    changeVisibility("visible");
    const beforeReset = [...pending.values()][0];
    root.innerHTML = ""; clock.reconcile();
    root.innerHTML = '<span class="tab-attention-working"></span>'; clock.reconcile();
    beforeReset();
    expect(angle()).toBe("0deg"); expect(pending.size).toBe(1);
    const beforeDispose = [...pending.values()][0];
    root.style.setProperty("color", "red"); root.dataset.unrelated = "keep";
    clock.dispose(); clock.dispose();
    const write = vi.spyOn(root.style, "setProperty");
    beforeDispose(); clock.reconcile(); changeVisibility("hidden"); changeMotion(true);
    expect(pending.size).toBe(0); expect(write).not.toHaveBeenCalled();
    expect(angle()).toBe(""); expect(root.hasAttribute("data-attention-motion-paused")).toBe(false);
    expect(root.style.color).toBe("red"); expect(root.dataset.unrelated).toBe("keep");
    expect(mediaListeners.size).toBe(0);
  });

  it("checks positive hidden/reduced state even if the environment event has not arrived", () => {
    controller();
    const write = vi.spyOn(root.style, "setProperty");
    visibility = "hidden";
    fire(100);
    expect(pending.size).toBe(0); expect(write).not.toHaveBeenCalled();
    changeVisibility("visible");
    Object.defineProperty(media, "matches", { configurable: true, value: true });
    fire(200);
    expect(pending.size).toBe(0); expect(write).not.toHaveBeenCalled();
  });

  it("hook StrictMode replay and root replacement cancel stale callbacks and remove exact listeners", () => {
    vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
    const callbacks: (() => void)[] = [];
    const timers = new Map<number, () => void>();
    vi.spyOn(window, "setTimeout").mockImplementation(callback => {
      const fn = callback as () => void;
      callbacks.push(fn); const id = callbacks.length; timers.set(id, fn);
      return id as unknown as ReturnType<typeof window.setTimeout>;
    });
    vi.spyOn(window, "clearTimeout").mockImplementation(id => { timers.delete(Number(id)); });
    const add = vi.spyOn(document, "addEventListener"), remove = vi.spyOn(document, "removeEventListener");
    function Harness({ identity }: { identity: string }) {
      const ref = useWorkingRingClock();
      return <div key={identity} ref={ref}><span className="tab-attention-working" /></div>;
    }
    const reactRoot = createRoot(root);
    const render = (identity: string) => act(() => reactRoot.render(<StrictMode><Harness identity={identity} /></StrictMode>));
    render("a");
    const old = root.firstElementChild as HTMLElement;
    expect(timers.size).toBe(1);
    expect(callbacks).toHaveLength(2);
    render("b");
    const replacement = root.firstElementChild as HTMLElement;
    expect(replacement).not.toBe(old);
    expect(old.style.getPropertyValue("--tab-attention-angle")).toBe("");
    expect(old.hasAttribute("data-attention-motion-paused")).toBe(false);
    const write = vi.spyOn(replacement.style, "setProperty");
    callbacks[0](); callbacks[1]();
    expect(write).not.toHaveBeenCalled(); expect(timers.size).toBe(1);
    act(() => reactRoot.unmount());
    callbacks[2]();
    expect(write).not.toHaveBeenCalled(); expect(timers.size).toBe(0);
    const listeners = add.mock.calls.filter(([event]) => event === "visibilitychange").map(([, listener]) => listener);
    expect(listeners).toHaveLength(3);
    expect(remove.mock.calls.filter(([event]) => event === "visibilitychange").map(([, listener]) => listener)).toEqual(listeners);
    expect(vi.mocked(media.removeEventListener).mock.calls).toEqual(vi.mocked(media.addEventListener).mock.calls);
    expect(mediaListeners.size).toBe(0);
  });
});
