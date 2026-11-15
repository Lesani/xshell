import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { JoinOverview, RingStatus } from "./types";

// What App needs of the Ring: the status (`ring_status`, then every `ring:status`) for the start
// view and the removal notice, and whether Create/Join exist on this platform at all.
export function useRingStatus(): { status: RingStatus | null; overview: JoinOverview | null | undefined } {
  const [status, setStatus] = useState<RingStatus | null>(null);
  // undefined while loading; null when it could not be read.
  const [overview, setOverview] = useState<JoinOverview | null | undefined>(undefined);
  useEffect(() => {
    let live = true;
    let unlisten: (() => void) | undefined;
    listen<RingStatus>("ring:status", e => { if (live && e.payload) setStatus(e.payload); })
      .then(u => { if (live) unlisten = u; else u(); })
      .catch(() => {});
    // An event always wins over the first read (the read may be older).
    invoke<RingStatus>("ring_status").then(s => { if (live && s) setStatus(prev => prev ?? s); }).catch(() => {});
    invoke<JoinOverview>("ring_join_overview").then(o => { if (live) setOverview(o ?? null); }).catch(() => { if (live) setOverview(null); });
    return () => { live = false; unlisten?.(); };
  }, []);
  return { status, overview };
}
