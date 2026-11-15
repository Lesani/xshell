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
const closePrompt = async (id: string, terminals: number | null) => { await act(async () => { native.handlers.get("local:quit-prompt")!({ payload: { id, terminals, kind: "close" } }); }); };

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

  it("failed: keeps the dialog open with the count that did not stop; the same id then acts", async () => {
    resolveReply = () => { throw "failed:2 sessions did not stop"; };
    await prompt("q1", 3);
    await click("[data-quit-stop]");
    expect(q("[role=dialog]")).not.toBeNull();
    expect(text()).toContain("Couldn't stop 2 terminals.");
    expect(text()).not.toContain("sessions did not stop");
    // Stop again succeeds under the same id and closes.
    resolveReply = () => null;
    await click("[data-quit-stop]");
    expect(resolves().map(r => [r.id, r.choice])).toEqual([["q1", "stop"], ["q1", "stop"]]);
    expect(q("[role=dialog]")).toBeNull();
  });

  it("after a failed Stop, Keep running and Cancel act on the same id", async () => {
    resolveReply = () => { throw "failed:1 sessions did not stop"; };
    await prompt("q1", 1);
    await click("[data-quit-stop]");
    resolveReply = () => null;
    await click("[data-quit-keep]");
    expect(resolves()[1]).toEqual({ id: "q1", choice: "keep", remember: false });
    expect(q("[role=dialog]")).toBeNull();
    resolveReply = () => { throw "failed:1 sessions did not stop"; };
    await prompt("q2", 1);
    await click("[data-quit-stop]");
    await click("[data-quit-cancel]");
    expect(resolves()[3]).toEqual({ id: "q2", choice: "cancel", remember: false });
  });

  it("a failed save of Do not ask again closes the dialog and says the setting was not saved", async () => {
    resolveReply = () => { throw "failed:0 disk full"; };
    await prompt("q1", 1);
    await click("[data-quit-remember]");
    await click("[data-quit-keep]");
    // The backend dropped this prompt: no dialog whose buttons would do nothing.
    expect(q("[role=dialog]")).toBeNull();
    expect(q("[data-quit-setting-failed]")?.textContent).toContain(C["quit.setting.failed"]);
    expect(text()).not.toContain("disk full");
    // The next close asks again.
    await prompt("q2", 1);
    expect(q("[data-quit-setting-failed]")).toBeNull();
    expect(q("[role=dialog]")).not.toBeNull();
  });

  it("keeps keyboard focus inside, makes the app behind inert and restores focus on cancel", async () => {
    const outside = document.createElement("button");
    outside.textContent = "behind";
    container.prepend(outside);
    outside.focus();
    await prompt("q1", 2);
    const dialog = q("[role=dialog]")!;
    expect(dialog.contains(document.activeElement)).toBe(true);
    expect(outside.hasAttribute("inert")).toBe(true);
    const items = Array.from(dialog.querySelectorAll<HTMLElement>("button:not([disabled]), input:not([disabled])"));
    const first = items[0], last = items[items.length - 1];
    last.focus();
    await act(async () => { document.dispatchEvent(new KeyboardEvent("keydown", { key: "Tab", bubbles: true })); });
    expect(document.activeElement).toBe(first);
    await act(async () => { document.dispatchEvent(new KeyboardEvent("keydown", { key: "Tab", shiftKey: true, bubbles: true })); });
    expect(document.activeElement).toBe(last);
    outside.focus(); // even if focus escaped, Tab brings it back inside
    await act(async () => { document.dispatchEvent(new KeyboardEvent("keydown", { key: "Tab", bubbles: true })); });
    expect(dialog.contains(document.activeElement)).toBe(true);
    await click("[data-quit-cancel]");
    expect(outside.hasAttribute("inert")).toBe(false);
    expect(document.activeElement).toBe(outside);
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

  describe("close kind (Windows: closing ends this machine's terminals)", () => {
    it("asks with the count: no Keep, no Stop, no remember checkbox; Cancel has focus", async () => {
      await closePrompt("q1", 1);
      expect(q("[data-quit-prompt]")?.getAttribute("data-quit-prompt")).toBe("close");
      expect(q("#quit-prompt-title")?.textContent).toBe("Close 1 terminal on this machine?");
      expect(text()).toContain(C["quit.close.body"]);
      expect(q("[data-quit-keep]")).toBeNull();
      expect(q("[data-quit-stop]")).toBeNull();
      expect(q("[data-quit-remember]")).toBeNull();
      expect(q("[data-quit-close]")?.textContent).toBe("Close xshell");
      expect(document.activeElement).toBe(q("[data-quit-cancel]"));
      await closePrompt("q2", 3);
      expect(q("#quit-prompt-title")?.textContent).toBe("Close 3 terminals on this machine?");
    });

    it("an unknown count asks without a number", async () => {
      await closePrompt("q1", null);
      expect(q("#quit-prompt-title")?.textContent).toBe(C["quit.close.title.unknown"]);
      expect(text()).toContain(C["quit.close.body.unknown"]);
      expect(text()).not.toContain("null");
    });

    it("Close sends choice close (never remember); the dialog goes", async () => {
      await closePrompt("q1", 2);
      await click("[data-quit-close]");
      expect(resolves()).toEqual([{ id: "q1", choice: "close", remember: false }]);
      expect(q("[role=dialog]")).toBeNull();
    });

    it("Escape and Cancel send cancel; the window stays", async () => {
      await closePrompt("q1", 2);
      await act(async () => { window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" })); });
      expect(resolves()).toEqual([{ id: "q1", choice: "cancel", remember: false }]);
      expect(q("[role=dialog]")).toBeNull();
      await closePrompt("q2", null);
      await click("[data-quit-cancel]");
      expect(resolves().map(r => [r.id, r.choice])).toEqual([["q1", "cancel"], ["q2", "cancel"]]);
    });

    it("a prompt without a kind is the keep prompt", async () => {
      await prompt("q1", 2);
      expect(q("[data-quit-keep]")).not.toBeNull();
      expect(q("[data-quit-close]")).toBeNull();
    });
  });

  it("a stale prompt closes the dialog (the window stays)", async () => {
    resolveReply = () => { throw "stale: no such quit prompt"; };
    await prompt("q1", 1);
    await click("[data-quit-keep]");
    expect(q("[role=dialog]")).toBeNull();
  });
});
