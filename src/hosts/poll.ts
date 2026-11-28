import { registry, type RegistrySnapshot } from "./registry";
import type { HostId } from "./types";

// Polls that read from a Host. Over the Relay every request and every reply is a message the
// Relay counts (and, on Cloudflare, bills), so a Host reached that way is asked a quarter as
// often, and nothing is asked while the window is hidden. Local and SSH Hosts keep their pace.

export const RELAY_POLL_EVERY = 4;

/** Whether `host`'s requests travel the Relay now. The Local Host (undefined) never does. */
export function viaRelay(host: HostId | undefined, snap: RegistrySnapshot = registry.getSnapshot()): boolean {
  if (!host) return false;
  return snap.available.find(h => h.id === host || h.aliases.includes(host))?.activeRoute === "relay";
}

/** Whether a poll's tick number `n` (1, 2, 3 …) is one on which `host` is asked. */
export function pollDue(host: HostId | undefined, n: number, snap: RegistrySnapshot = registry.getSnapshot()): boolean {
  return !viaRelay(host, snap) || n % RELAY_POLL_EVERY === 0;
}

type Doc = Pick<Document, "visibilityState" | "addEventListener" | "removeEventListener">;

/**
 * Calls `fn(n)` every `ms` while the window is visible, and once when it becomes visible
 * again after a tick was skipped. With `host`, only on the ticks `pollDue` allows; a caller
 * that asks several Hosts passes none and checks `pollDue(host, n)` for each. Returns stop.
 */
export function startPoll(fn: (n: number) => void, ms: number, host?: () => HostId | undefined, doc: Doc = document): () => void {
  let n = 0, missed = false;
  const hidden = () => doc.visibilityState === "hidden";
  const id = setInterval(() => {
    if (hidden()) { missed = true; return; }
    n++;
    if (!host || pollDue(host(), n)) fn(n);
  }, ms);
  const visible = () => {
    if (hidden() || !missed) return;
    missed = false;
    // A catch-up asks every Host, like the first poll after a mount.
    fn(RELAY_POLL_EVERY);
  };
  doc.addEventListener("visibilitychange", visible);
  return () => { clearInterval(id); doc.removeEventListener("visibilitychange", visible); };
}
