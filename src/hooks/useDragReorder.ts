import { useEffect, useRef, useState } from "react";

export interface ReorderDrop {
  sourceIndex: number;
  destinationGap: number;
  targetIndex: number;
}
interface Options<T> {
  items: T[];
  direction: "vertical" | "horizontal";
  itemSelector: string;
  onReorder: (newItems: T[], drop: ReorderDrop) => void;
  // Atomic identity, independent of metadata updates. A changed list cancels the gesture.
  itemKey?: (item: T) => string;
}

export function useDragReorder<T>({ items, direction, itemSelector, onReorder, itemKey }: Options<T>) {
  const [dragIdx, setDragIdx] = useState<number | null>(null);
  const [overIdx, setOverIdx] = useState<number | null>(null);
  const current = useRef({ items, direction, itemSelector, onReorder, itemKey });
  current.current = { items, direction, itemSelector, onReorder, itemKey };
  const stateRef = useRef<{ startIdx: number; startX: number; startY: number; dragging: boolean; keys: unknown[]; gap: number | null; target: number | null } | null>(null);
  const reset = () => {
    stateRef.current = null;
    document.body.style.cursor = "";
    setDragIdx(null); setOverIdx(null);
  };
  const valid = () => {
    const s = stateRef.current;
    const c = current.current;
    return !!s && s.keys.length === c.items.length && s.keys.every((key, i) => key === (c.itemKey ? c.itemKey(c.items[i]) : c.items[i]));
  };
  useEffect(() => { if (stateRef.current && !valid()) reset(); });
  const onPointerDown = (e: React.PointerEvent, idx: number) => {
    if (e.button !== 0) return;
    stateRef.current = { startIdx: idx, startX: e.clientX, startY: e.clientY, dragging: false, keys: items.map(item => itemKey ? itemKey(item) : item), gap: null, target: null };
  };

  useEffect(() => {
    const onMove = (e: PointerEvent) => {
      const s = stateRef.current;
      if (!s) return;
      if (!valid()) { reset(); return; }
      if (!s.dragging && Math.hypot(e.clientX - s.startX, e.clientY - s.startY) > 6) {
        s.dragging = true;
        setDragIdx(s.startIdx);
        document.body.style.cursor = "grabbing";
      }
      if (!s.dragging) return;
      const c = current.current;
      const item = document.elementFromPoint(e.clientX, e.clientY)?.closest(c.itemSelector);
      s.gap = null; s.target = null;
      if (item) {
        const idx = Number(item.getAttribute("data-idx"));
        if (Number.isInteger(idx) && idx >= 0 && idx < c.items.length) {
          const rect = item.getBoundingClientRect();
          const mid = c.direction === "vertical" ? rect.top + rect.height / 2 : rect.left + rect.width / 2;
          const p = c.direction === "vertical" ? e.clientY : e.clientX;
          s.target = idx; s.gap = p < mid ? idx : idx + 1;
        }
      }
      setOverIdx(s.gap);
    };
    const onUp = () => {
      const s = stateRef.current;
      if (s?.dragging && valid() && s.gap !== null && s.target !== null) {
        const c = current.current;
        const target = s.gap > s.startIdx ? s.gap - 1 : s.gap;
        if (target !== s.startIdx) {
          const next = [...c.items];
          const [moved] = next.splice(s.startIdx, 1);
          next.splice(target, 0, moved);
          c.onReorder(next, { sourceIndex: s.startIdx, destinationGap: s.gap, targetIndex: s.target });
        }
      }
      reset();
    };
    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup", onUp);
    window.addEventListener("pointercancel", reset);
    return () => {
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup", onUp);
      window.removeEventListener("pointercancel", reset);
      document.body.style.cursor = "";
    };
  }, []);
  return { dragIdx, overIdx, onPointerDown };
}
