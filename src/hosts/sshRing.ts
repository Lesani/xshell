// Add to Ring on a saved SSH Host's row (#115): what the row shows, and the sentence for each
// error code of `ring_add_ssh_host` (src-tauri/src/ring/ssh_host.rs, errors `code: detail`).

import type { RingStatus } from "../ring/types";
import type { StringKey } from "./strings";
import type { AvailableHost, HostConfig, HostStatus } from "./types";
import { isUsableStatus } from "./registry";

// `hidden`: this Desktop is in no Ring, or the Host runs Windows (AM6); `member`: the quiet
// label; `add`: the button; `offline`: the button, disabled with the reason.
export type SshRingView = "hidden" | "member" | "add" | "offline";

// The available row that represents a saved Host (its own id, or an alias of a Ring row).
export function availableFor(host: HostConfig, available: AvailableHost[]): AvailableHost | undefined {
  return available.find(h => h.id === host.id || h.aliases.includes(host.id));
}

// A Host is a Ring member once its row carries its Daemon key and a Relay route.
export function isRingMember(row: AvailableHost | undefined): boolean {
  return !!row?.daemonKey && row.routes.includes("relay");
}

export function sshRingView(host: HostConfig, status: HostStatus | undefined, available: AvailableHost[], ring: RingStatus | null): SshRingView {
  if (!ring || !ring.enabled || ring.membership !== "member") return "hidden";
  if (isRingMember(availableFor(host, available))) return "member";
  if (status?.os === "windows") return "hidden";
  return isUsableStatus(status) ? "add" : "offline";
}

const ERROR_KEYS: Record<string, StringKey> = {
  "not-ready": "hosts.ring.err.notReady",
  offline: "hosts.ring.err.offline",
  unsupported: "hosts.ring.err.unsupported",
  windows: "hosts.ring.err.windows",
  "own-desktop": "hosts.ring.err.ownDesktop",
  "other-ring": "hosts.ring.err.otherRing",
  "rate-limited": "hosts.ring.err.rateLimited",
  unfinished: "hosts.ring.err.unfinished",
  expired: "hosts.ring.err.expired",
  service: "hosts.ring.err.service",
  busy: "hosts.ring.err.busy",
  failed: "hosts.ring.err.failed",
};

// The code of a `code: detail` error (`failed` for anything else).
export function addToRingErrorCode(error: unknown): string {
  const text = typeof error === "string" ? error : (error as { message?: string })?.message ?? "";
  const code = text.split(":")[0].trim();
  return code in ERROR_KEYS ? code : "failed";
}

export function addToRingErrorKey(code: string): StringKey {
  return ERROR_KEYS[code] ?? "hosts.ring.err.failed";
}
