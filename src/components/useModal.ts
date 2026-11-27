import { useLayoutEffect, type RefObject } from "react";

// A modal dialog's keyboard and focus rules: everything outside it is inert (and hidden from
// assistive technology) while it is open, Tab and Shift+Tab stay inside, and the element that
// had focus before gets it back when it closes. The dialog must be rendered into
// document.body (a portal) so that its siblings are the rest of the app.

const FOCUSABLE = 'button:not([disabled]), [href], input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])';

export function focusables(root: HTMLElement): HTMLElement[] {
  return Array.from(root.querySelectorAll<HTMLElement>(FOCUSABLE));
}

export function useModal(ref: RefObject<HTMLElement | null>, active: boolean) {
  // A layout effect: it sees the focus from before the dialog's own autofocus effects.
  useLayoutEffect(() => {
    const dialog = ref.current;
    if (!active || !dialog) return;
    const before = document.activeElement as HTMLElement | null;
    const made: Element[] = [];
    for (let node: Element = dialog; node.parentElement && node !== document.body; node = node.parentElement) {
      for (const el of Array.from(node.parentElement.children)) {
        if (el === node || el.hasAttribute("inert") || el.tagName === "SCRIPT") continue;
        el.setAttribute("inert", "");
        el.setAttribute("aria-hidden", "true");
        made.push(el);
      }
    }
    if (!dialog.contains(document.activeElement)) (focusables(dialog)[0] ?? dialog).focus();
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Tab") return;
      const list = focusables(dialog);
      if (list.length === 0) { e.preventDefault(); return; }
      const first = list[0], last = list[list.length - 1];
      const at = document.activeElement as HTMLElement | null;
      if (e.shiftKey && (at === first || !dialog.contains(at))) { e.preventDefault(); last.focus(); }
      else if (!e.shiftKey && (at === last || !dialog.contains(at))) { e.preventDefault(); first.focus(); }
    };
    document.addEventListener("keydown", onKey, true);
    return () => {
      document.removeEventListener("keydown", onKey, true);
      for (const el of made) { el.removeAttribute("inert"); el.removeAttribute("aria-hidden"); }
      if (before && document.contains(before)) before.focus();
    };
  }, [ref, active]);
}
