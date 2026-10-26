import { describe, expect, it, vi } from "vitest";
import type { Tab } from "../types";
import { handlePiShiftEnter } from "./piInput";

const pi: Pick<Tab, "agent" | "shellMode"> = { agent: "pi" };
const key = (overrides: Partial<KeyboardEvent> = {}) => ({
  type: "keydown", key: "Enter", code: "NumpadEnter", shiftKey: true,
  altKey: false, ctrlKey: false, metaKey: false, isComposing: false,
  preventDefault: vi.fn(), ...overrides,
} as unknown as KeyboardEvent);

describe("handlePiShiftEnter", () => {
  it("sends exactly one CSI-u event through xterm input and suppresses the default CR", () => {
    const ev = key();
    const input = vi.fn();
    expect(handlePiShiftEnter(pi, ev, input)).toBe(false);
    expect(input.mock.calls).toEqual([["\x1b[13;2u", true]]);
    expect(ev.preventDefault).toHaveBeenCalledOnce();
  });

  it.each([
    { agent: "claude" }, { agent: "codex" }, {},
    { agent: "pi", shellMode: "raw" },
  ] as Pick<Tab, "agent" | "shellMode">[])("bypasses non-Pi and raw Shell identity %j", (tab) => {
    const ev = key(), input = vi.fn();
    expect(handlePiShiftEnter(tab, ev, input)).toBe(true);
    expect(input).not.toHaveBeenCalled();
    expect(ev.preventDefault).not.toHaveBeenCalled();
  });

  it.each([
    { shiftKey: false }, { altKey: true }, { ctrlKey: true }, { metaKey: true },
    { isComposing: true }, { type: "keyup" }, { type: "keypress" },
    { key: "Process" }, { key: "ä", code: "Enter" },
  ])("leaves Enter, Alt+Enter, other modifiers and composition to xterm: %j", (change) => {
    const ev = key(change), input = vi.fn();
    expect(handlePiShiftEnter(pi, ev, input)).toBe(true);
    expect(input).not.toHaveBeenCalled();
    expect(ev.preventDefault).not.toHaveBeenCalled();
  });
});
