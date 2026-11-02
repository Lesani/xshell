// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { RenameTabDialog } from "./RenameTabDialog";
import { fmt } from "../hosts/strings";

let root: Root, container: HTMLDivElement, trigger: HTMLButtonElement;
beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  container = document.createElement("div"); document.body.append(container); root = createRoot(container);
  trigger = document.createElement("button"); document.body.append(trigger); trigger.focus();
});
afterEach(() => { act(() => root.unmount()); container.remove(); trigger.remove(); vi.unstubAllGlobals(); });
function render(overrides: Partial<Parameters<typeof RenameTabDialog>[0]> = {}) {
  const props = { returnFocus: trigger, seed: "Automatic", operation: null, error: null, unavailable: null, hasCustomTitle: true, onSubmit: vi.fn(), onClose: vi.fn(), ...overrides };
  act(() => root.render(<RenameTabDialog {...props} />)); return props;
}
const button = (label: string) => Array.from(container.querySelectorAll("button")).find(b => b.textContent === label)!;
function setDraft(value: string) {
  const input = container.querySelector("input")!;
  act(() => { Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(input, value); input.dispatchEvent(new Event("input", { bubbles: true })); });
}
function submit() { act(() => { container.querySelector("form")!.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true })); }); }

describe("RenameTabDialog", () => {
  it("named_dialog_initial_focus_selection_and_focus_returns_to_trigger", () => {
    render();
    const dialog = container.querySelector("[role=dialog]")!;
    expect(document.getElementById(dialog.getAttribute("aria-labelledby")!)?.textContent).toBe("Rename Tab");
    const input = container.querySelector("input")!;
    expect(container.querySelector(`label[for="${input.id}"]`)?.textContent).toBe("Tab name");
    expect(document.activeElement).toBe(input);
    expect([input.selectionStart, input.selectionEnd]).toEqual([0, "Automatic".length]);
    act(() => root.render(null)); expect(document.activeElement).toBe(trigger);
  });
  it("cancel_and_escape_send_nothing_and_tab_focus_is_contained", () => {
    const props = render();
    act(() => button("Cancel").click());
    act(() => container.querySelector("input")!.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true })));
    expect(props.onClose).toHaveBeenCalledTimes(2); expect(props.onSubmit).not.toHaveBeenCalled();
    button("Save").focus();
    act(() => button("Save").dispatchEvent(new KeyboardEvent("keydown", { key: "Tab", bubbles: true, cancelable: true })));
    expect(document.activeElement).toBe(container.querySelector("input"));
    act(() => container.querySelector("input")!.dispatchEvent(new KeyboardEvent("keydown", { key: "Tab", shiftKey: true, bubbles: true, cancelable: true })));
    expect(document.activeElement).toBe(button("Save"));
  });
  it("blank_controls_and_scalar_limit_validation", () => {
    const props = render();
    for (const [draft, copy] of [["   ", "tab.rename.blank"], ["bad\u0085", "tab.rename.controls"], ["📝".repeat(257), "tab.rename.tooLong"]] as const) {
      setDraft(draft); submit(); expect(container.querySelector("[role=alert]")?.textContent).toBe(fmt(copy));
    }
    expect(props.onSubmit).not.toHaveBeenCalled();
    setDraft("📝".repeat(256)); submit(); expect(props.onSubmit).toHaveBeenCalledExactlyOnceWith("📝".repeat(256));
    expect(container.querySelector("input")!.hasAttribute("maxlength")).toBe(false);
  });
  it("save_and_clear_pending_states_disable_duplicate_commands_but_allow_dismissal", () => {
    const props = render();
    act(() => button("Use automatic name").click()); expect(props.onSubmit).toHaveBeenCalledExactlyOnceWith(null);
    for (const operation of ["save", "clear"] as const) {
      render({ ...props, operation });
      expect(container.querySelector("[role=status]")?.textContent).toBe(fmt(operation === "save" ? "tab.rename.saving" : "tab.rename.clearing"));
      expect(button("Save").disabled).toBe(true); expect(button("Use automatic name").disabled).toBe(true);
      expect(button("Cancel").disabled).toBe(false);
      expect(container.querySelector("form")!.contains(document.activeElement)).toBe(true);
      submit();
    }
    expect(props.onSubmit).toHaveBeenCalledTimes(1);
  });
  it("failure_keeps_draft_and_error_including_indeterminate_and_offline", () => {
    const props = render(); setDraft("Draft kept"); submit();
    render({ ...props, error: fmt("tab.rename.notConfirmed") });
    expect(container.querySelector("input")!.value).toBe("Draft kept");
    expect(container.querySelector("[role=alert]")?.textContent).toBe(fmt("tab.rename.notConfirmed"));
    render({ ...props, unavailable: fmt("tab.rename.offline") });
    expect(button("Save").disabled).toBe(true);
    expect(container.querySelector("input")!.value).toBe("Draft kept");
  });
});
