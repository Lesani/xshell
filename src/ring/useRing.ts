import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { RingStatus } from "./types";

// The Ring as Settings → Mobile shows it: `ring_status` once, then every `ring:status`.
export function useRing() {
  const [status, setStatus] = useState<RingStatus | null>(null);
  const mounted = useRef(false);

  useEffect(() => {
    mounted.current = true;
    let unlisten: (() => void) | null = null;
    let cancelled = false;
    listen<RingStatus>("ring:status", e => { if (mounted.current) setStatus(e.payload); })
      .then(u => { if (cancelled) u(); else unlisten = u; })
      .catch(() => {});
    invoke<RingStatus>("ring_status").then(s => { if (mounted.current) setStatus(s); }).catch(() => {});
    return () => {
      mounted.current = false;
      cancelled = true;
      unlisten?.();
    };
  }, []);

  const enable = useCallback(async (startOver: boolean) => {
    const s = await invoke<RingStatus>("ring_enable", { startOver });
    if (mounted.current) setStatus(s);
  }, []);

  const setRelayUrl = useCallback(async (url: string) => {
    const s = await invoke<RingStatus>("ring_set_relay_url", { url });
    if (mounted.current) setStatus(s);
  }, []);

  return { status, enable, setRelayUrl };
}
