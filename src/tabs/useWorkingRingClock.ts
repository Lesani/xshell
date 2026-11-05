import { useLayoutEffect, useRef } from "react";

const angleProperty = "--tab-attention-angle";
const pausedAttribute = "data-attention-motion-paused";
const cadenceMs = 100;

interface RingClockTime {
  now(): number;
  setTimeout(callback: () => void, delay: number): number;
  clearTimeout(handle: number): void;
}

// One clock per TabBar, based on committed ring markup rather than duplicated status logic.
export function createWorkingRingClock(root: HTMLElement, time: RingClockTime = {
  now: () => performance.now(),
  setTimeout: (callback, delay) => window.setTimeout(callback, delay),
  clearTimeout: handle => window.clearTimeout(handle),
}) {
  const doc = root.ownerDocument;
  const media = doc.defaultView?.matchMedia?.("(prefers-reduced-motion: reduce)");
  let disposed = false, working = false, running = false;
  let generation = 0, elapsed = 0, startedAt = 0;
  let timer: number | undefined, phase: number | undefined, paused: boolean | undefined;

  const paint = () => {
    const next = Math.floor((elapsed + time.now() - startedAt) / cadenceMs) % 12;
    if (next !== phase) {
      root.style.setProperty(angleProperty, `${next * 30}deg`);
      phase = next;
    }
  };
  const schedule = () => {
    const scheduledGeneration = generation;
    timer = time.setTimeout(() => {
      if (disposed || !running || generation !== scheduledGeneration) return;
      timer = undefined;
      if (doc.visibilityState === "hidden" || media?.matches !== false) {
        synchronize();
        return;
      }
      paint();
      schedule();
    }, cadenceMs);
  };
  const stop = () => {
    if (running) elapsed += time.now() - startedAt;
    running = false;
    generation++;
    if (timer !== undefined) time.clearTimeout(timer);
    timer = undefined;
  };
  const synchronize = () => {
    const eligible = working && doc.visibilityState !== "hidden" && media?.matches === false;
    if (paused !== !eligible) {
      paused = !eligible;
      root.setAttribute(pausedAttribute, String(paused));
    }
    if (!eligible) stop();
    if (!working) {
      elapsed = 0;
      if (phase !== undefined) root.style.removeProperty(angleProperty);
      phase = undefined;
    } else if (eligible && !running) {
      running = true;
      startedAt = time.now();
      // Resuming retains the last painted phase; hidden wall time is not charged.
      if (phase === undefined) paint();
      schedule();
    }
  };
  const reconcile = () => {
    if (disposed) return;
    working = root.querySelector(".tab-attention-working") !== null;
    synchronize();
  };
  // Subscribe before reading eligibility, including when no Working ring exists yet.
  doc.addEventListener("visibilitychange", reconcile);
  media?.addEventListener?.("change", reconcile);
  reconcile();
  return {
    root,
    reconcile,
    dispose() {
      if (disposed) return;
      disposed = true;
      stop();
      doc.removeEventListener("visibilitychange", reconcile);
      media?.removeEventListener?.("change", reconcile);
      if (phase !== undefined) root.style.removeProperty(angleProperty);
      root.removeAttribute(pausedAttribute);
    },
  };
}

export function useWorkingRingClock() {
  const root = useRef<HTMLDivElement>(null);
  const clock = useRef<ReturnType<typeof createWorkingRingClock> | null>(null);
  useLayoutEffect(() => () => {
    clock.current?.dispose();
    clock.current = null;
  }, []);
  useLayoutEffect(() => {
    if (clock.current?.root !== root.current) {
      clock.current?.dispose();
      clock.current = root.current ? createWorkingRingClock(root.current) : null;
    } else {
      clock.current?.reconcile();
    }
  });
  return root;
}
