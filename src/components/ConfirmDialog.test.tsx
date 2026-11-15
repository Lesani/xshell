// @vitest-environment happy-dom
import { act, useState } from "react";
import { createRoot } from "react-dom/client";
import { expect, it, vi } from "vitest";
import { ConfirmDialog } from "./ConfirmDialog";

it("is modal: focus starts on Cancel, Tab/Shift+Tab stay inside, the app behind is inert, focus returns on cancel", async () => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  const container = document.createElement("div");
  document.body.append(container);
  const root = createRoot(container);
  function H() {
    const [open, setOpen] = useState(false);
    return (
      <>
        <button data-opener onClick={() => setOpen(true)}>Open</button>
        {open && <ConfirmDialog title="Remove this machine?" body="b" confirm="Remove" cancel="Cancel" onConfirm={() => {}} onCancel={() => setOpen(false)} />}
      </>
    );
  }
  await act(async () => { root.render(<H />); });
  const opener = container.querySelector<HTMLElement>("[data-opener]")!;
  opener.focus();
  await act(async () => { opener.click(); });
  const dialog = container.querySelector<HTMLElement>("[role=dialog]")!;
  expect(dialog.getAttribute("aria-modal")).toBe("true");
  const buttons = Array.from(dialog.querySelectorAll<HTMLButtonElement>("button"));
  const [close, cancel, confirm] = buttons;
  expect(document.activeElement).toBe(cancel);
  expect(opener.hasAttribute("inert")).toBe(true);
  confirm.focus();
  await act(async () => { document.dispatchEvent(new KeyboardEvent("keydown", { key: "Tab", bubbles: true })); });
  expect(document.activeElement).toBe(close);
  await act(async () => { document.dispatchEvent(new KeyboardEvent("keydown", { key: "Tab", shiftKey: true, bubbles: true })); });
  expect(document.activeElement).toBe(confirm);
  await act(async () => { window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" })); });
  expect(container.querySelector("[role=dialog]")).toBeNull();
  expect(opener.hasAttribute("inert")).toBe(false);
  expect(document.activeElement).toBe(opener);
  await act(async () => { root.unmount(); });
  container.remove();
  vi.unstubAllGlobals();
});
