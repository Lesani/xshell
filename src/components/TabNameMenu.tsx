import { useEffect, useRef } from "react";
import { fmt } from "../hosts/strings";

// Deliberately local to Terminal Tab naming, not a general menu framework.
export function TabNameMenu({ anchor, x, y, unavailable, hasCustomTitle, busy, onRename, onAutomatic, onClose }: {
  anchor: HTMLElement; x: number; y: number; unavailable: string | null; hasCustomTitle: boolean; busy: boolean;
  onRename: () => void; onAutomatic: () => void; onClose: () => void;
}) {
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const menu = ref.current;
    const first = menu?.querySelector<HTMLElement>("[role=menuitem]:not(:disabled)");
    (first || menu)?.focus();
    const outside = (e: PointerEvent) => { if (!menu?.contains(e.target as Node)) onClose(); };
    document.addEventListener("pointerdown", outside, true);
    return () => { document.removeEventListener("pointerdown", outside, true); if (anchor.isConnected) anchor.focus(); };
  }, [anchor, onClose]);
  return <div ref={ref} className="tab-name-menu" role="menu" aria-label={fmt("tab.rename.menuA11y")} tabIndex={-1}
    style={{ left: Math.max(4, Math.min(x, window.innerWidth - 288)), top: Math.max(4, Math.min(y, window.innerHeight - 170)) }}
    onKeyDown={e => {
      e.stopPropagation();
      if (e.key === "Escape" || e.key === "Tab") { e.preventDefault(); onClose(); }
      if (["ArrowDown", "ArrowUp", "Home", "End"].includes(e.key)) {
        e.preventDefault();
        const items = Array.from(e.currentTarget.querySelectorAll<HTMLButtonElement>("button:not(:disabled)"));
        const i = items.indexOf(document.activeElement as HTMLButtonElement);
        const next = e.key === "Home" ? 0 : e.key === "End" ? items.length - 1 : (i + (e.key === "ArrowUp" ? -1 : 1) + items.length) % items.length;
        items[next]?.focus();
      }
    }}>
    <button role="menuitem" disabled={busy || !!unavailable} onClick={onRename}>{fmt("tab.rename.action")}</button>
    <button role="menuitem" disabled={busy || !!unavailable || !hasCustomTitle} onClick={onAutomatic}>{fmt("tab.rename.automatic")}</button>
    {unavailable && <p role="status">{unavailable}</p>}
  </div>;
}
