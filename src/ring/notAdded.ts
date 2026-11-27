import { useCallback, useEffect, useRef, useState } from "react";
import { load } from "@tauri-apps/plugin-store";
import type { RingStatus } from "./types";

// "This machine wasn't added" (U3, finishing → none): remembered in settings.json while a Join
// or Create is being finished, so the outcome still shows on a screen that was not open when it
// was decided (also after a restart). Cleared by membership, a new Create/Join, or dismissal.

export const FINISHING_SETTING = "ring_finishing_pending";

async function store() {
  return load("settings.json", { defaults: {}, autoSave: true });
}

export function useNotAdded(status: Pick<RingStatus, "membership"> | null) {
  const [pending, setPending] = useState<boolean | null>(null);
  const live = useRef(true);
  useEffect(() => {
    live.current = true;
    store().then(s => s.get<boolean>(FINISHING_SETTING)).then(v => { if (live.current) setPending(prev => prev ?? v === true); }).catch(() => { if (live.current) setPending(prev => prev ?? false); });
    return () => { live.current = false; };
  }, []);
  const save = useCallback((v: boolean) => {
    setPending(v);
    store().then(s => s.set(FINISHING_SETTING, v)).catch(() => {});
  }, []);
  const membership = status?.membership;
  useEffect(() => {
    if (membership === "finishing" && pending !== true) save(true);
    else if (membership === "member" && pending) save(false);
  }, [membership, pending, save]);
  const notAdded = membership === "none" && pending === true;
  const clear = useCallback(() => { if (pending) save(false); }, [pending, save]);
  return { notAdded, clear };
}
