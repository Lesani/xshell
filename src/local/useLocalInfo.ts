import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { LocalStatus } from "./localInfo";

// `local_host_info` once, then every `local:daemon`; `retry()` runs `local_daemon_retry` and
// applies its answer.
export function useLocalInfo() {
  const [info, setInfo] = useState<LocalStatus | null>(null);
  const mounted = useRef(false);
  // An event is newer than a reply that was in flight when it arrived.
  const seq = useRef(0);
  useEffect(() => {
    mounted.current = true;
    let cancelled = false;
    let unlisten: (() => void) | undefined;
    listen<LocalStatus>("local:daemon", e => { if (mounted.current && e.payload) { seq.current++; setInfo(e.payload); } })
      .then(u => { if (cancelled) u(); else unlisten = u; })
      .catch(() => {});
    const at = seq.current;
    invoke<LocalStatus>("local_host_info").then(i => { if (mounted.current && i && seq.current === at) setInfo(i); }).catch(() => {});
    return () => { mounted.current = false; cancelled = true; unlisten?.(); };
  }, []);
  const retry = useCallback(async () => {
    const at = seq.current;
    const i = await invoke<LocalStatus>("local_daemon_retry");
    if (mounted.current && i && seq.current === at) setInfo(i);
    return i;
  }, []);
  return { info, retry };
}
