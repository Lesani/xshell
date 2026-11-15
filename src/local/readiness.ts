import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { localView, type LocalStatus } from "./localInfo";

// Start-up's live readiness gate (U5): waits until local Terminals can run (`ready`) or cannot
// (`failed`), from `local:daemon` and repeated `local_host_info` reads. `timeout` when neither
// happened in time. Outside Daemon mode there is nothing to wait for.
export type Readiness = "ready" | "failed" | "timeout" | "none";

export const READINESS_WAIT_MS = 30_000;
const POLL_MS = 500;

export async function liveLocalInfo(): Promise<LocalStatus | null> {
  return invoke<LocalStatus>("local_host_info").catch(() => null);
}

export function awaitLocalReadiness(first: LocalStatus | null, ms = READINESS_WAIT_MS): Promise<Readiness> {
  if (!first || first.mode === "in-process") return Promise.resolve("none");
  const v = localView(first);
  if (v !== "ok") return Promise.resolve(v);
  return new Promise<Readiness>(resolve => {
    let done = false;
    let unlisten: (() => void) | undefined;
    const finish = (r: Readiness) => {
      if (done) return;
      done = true;
      clearTimeout(deadline);
      clearInterval(poll);
      unlisten?.();
      resolve(r);
    };
    const see = (i: LocalStatus | null | undefined) => {
      const view = localView(i);
      if (view !== "ok") finish(view);
    };
    const deadline = setTimeout(() => finish("timeout"), ms);
    const poll = setInterval(() => { void liveLocalInfo().then(see); }, POLL_MS);
    listen<LocalStatus>("local:daemon", e => see(e.payload))
      .then(u => { if (done) u(); else unlisten = u; })
      .catch(() => {});
  });
}
