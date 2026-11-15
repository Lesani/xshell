// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { QuitPrompt } from "./QuitPrompt";
import { C } from "../ring/copy";

const native = vi.hoisted(() => ({ invoke: vi.fn(), handlers: new Map<string, (event: { payload: unknown }) => void>(), order: [] as string[] }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...a: unknown[]) => { native.order.push(`invoke:${a[0]}`); return native.invoke(...a); } }));
vi.mock("@tauri-apps/api/event", () => ({ listen: async (name: string, handler: (event: { payload: unknown }) => void) => { native.order.push(`listen:${name}`); native.handlers.set(name, handler); return () => native.handlers.delete(name); } }));

let container: HTMLDivElement;
let root: Root;
let resolveReply: (args: Record<string, unknown>) => unknown;
const resolves = () => native.invoke.mock.calls.filter(([c]) => c === "local_quit_resolve").map(([, a]) => a as Record<string, unknown>);
const q = (sel: string) => container.querySelector<HTMLElement>(sel);
const click = async (sel: string) => { await act(async () => { q(sel)!.click(); }); };
const text = () => container.textContent ?? "";
const prompt = async (id: string, terminals: number) => { await act(async () => { native.handlers.get("local:quit-prompt")!({ payload: { id, terminals } }); }); };

beforeEach(async () => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  native.invoke.mockReset();
  native.handlers.clear();
  native.order = [];
  resolveReply = () => null;
  native.invoke.mockImplementation(async (command: string, args: Record<string, unknown>) => {
    if (command === "local_quit_resolve") return resolveReply(args);
    return undefined;
  });
  container = document.createElement("div");
  document.body.append(container);
  root = createRoot(container);
  await act(async () => { root.render(<QuitPrompt />); });
});
afterEach(async () => {
  await act(async () => { root.unmount(); });
  container.remove();
  vi.unstubAllGlobals();
});

describe("quit prompt", () => {
  it("registers listen before invoking local_quit_prompt_ready", () => {
    expect(native.order).toEqual(["listen:local:quit-prompt", "invoke:local_quit_prompt_ready"]);
  });

  it("1 terminal: Keep 1 terminal running? / Stop it; 3: plural", async () => {
    expect(q("[role=dialog]")).toBeNull();
    await prompt("q1", 1);
    expect(q("#quit-prompt-title")?.textContent).toBe("Keep 1 terminal running?");
    expect(text()).toContain(C["quit.body"]);
    expect(q("[data-quit-stop]")?.textContent).toBe("Stop it");
    expect(q("[data-quit-keep]")?.textContent).toBe(C["quit.keep"]);
    expect(text()).toContain(C["quit.remember"]);
    await prompt("q2", 3);
    expect(q("#quit-prompt-title")?.textContent).toBe("Keep 3 terminals running?");
    expect(q("[data-quit-stop]")?.textContent).toBe("Stop them");
  });

  it("Keep with remember sends {id,choice:keep,remember:true}; Stop sends remember:false even if checked", async () => {
    await prompt("q1", 2);
    await click("[data-quit-remember]");
    await click("[data-quit-keep]");
    expect(resolves()).toEqual([{ id: "q1", choice: "keep", remember: true }]);
    expect(q("[role=dialog]")).toBeNull();
    await prompt("q2", 2);
    await click("[data-quit-remember]");
    await click("[data-quit-stop]");
    expect(resolves()[1]).toEqual({ id: "q2", choice: "stop", remember: false });
  });

  it("changed → More terminals started, 2 more … Stop them too?; next resolve uses the new id", async () => {
    resolveReply = () => ({ changed: { id: "q9", added: 2 } });
    await prompt("q1", 1);
    await click("[data-quit-stop]");
    expect(q("#quit-prompt-title")?.textContent).toBe(C["quit.more.title"]);
    expect(text()).toContain("2 more terminals started. Stop them too?");
    expect(q("[data-quit-remember]")).toBeNull();
    resolveReply = () => null;
    await click("[data-quit-stop]");
    expect(resolves()[1]).toEqual({ id: "q9", choice: "stop", remember: false });
    expect(q("[role=dialog]")).toBeNull();
  });

  it("failed: keeps the dialog open with the count that did not stop", async () => {
    resolveReply = () => { throw "failed:2 sessions did not stop"; };
    await prompt("q1", 3);
    await click("[data-quit-stop]");
    expect(q("[role=dialog]")).not.toBeNull();
    expect(text()).toContain("Couldn't stop 2 terminals.");
    expect(text()).not.toContain("sessions did not stop");
  });

  it("a failed save of Do not ask again says the setting was not saved", async () => {
    resolveReply = () => { throw "failed:0 disk full"; };
    await prompt("q1", 1);
    await click("[data-quit-remember]");
    await click("[data-quit-keep]");
    expect(text()).toContain(C["quit.setting.failed"]);
  });

  it("Escape sends cancel; the X and Cancel too", async () => {
    await prompt("q1", 1);
    await act(async () => { window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" })); });
    expect(resolves()).toEqual([{ id: "q1", choice: "cancel", remember: false }]);
    expect(q("[role=dialog]")).toBeNull();
    await prompt("q2", 1);
    await click(`[aria-label="${C["quit.cancel"]}"]`);
    await prompt("q3", 1);
    await click("[data-quit-cancel]");
    expect(resolves().map(r => [r.id, r.choice])).toEqual([["q1", "cancel"], ["q2", "cancel"], ["q3", "cancel"]]);
  });

  it("a stale prompt closes the dialog (the window stays)", async () => {
    resolveReply = () => { throw "stale: no such quit prompt"; };
    await prompt("q1", 1);
    await click("[data-quit-keep]");
    expect(q("[role=dialog]")).toBeNull();
  });
});
