// Add to Ring on a saved SSH Host's row (#115): what the row shows, and the sentence for each
// error code of `ring_add_ssh_host` (src-tauri/src/ring/ssh_host.rs, errors `code: detail`).

import type { RingStatus } from "../ring/types";
import type { StringKey } from "./strings";
import type { AvailableHost, HostConfig, HostId, HostStatus } from "./types";
import { isUsableStatus, registry } from "./registry";

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

// ── Moving a saved Host to the Ring list (#119) ──────────────────────
// After Add to Ring the saved SSH entry is removed and the machine continues under its Ring
// id, reached through the Relay. Only while that is expected to work: see `ringMoveTarget`.

// How long the Relay has to report the new member online before the move is given up.
// (An object so that a test can shorten it.)
export const ringMove = { waitMs: 20_000 };
const RING_ID = /^r_[0-9a-f]{64}$/;

// The Ring id a saved Host continues under, or null while it must stay an SSH Host: its row
// must be its own (not folded into another machine's), carry the Daemon key and the Ring id,
// and the Relay must report the machine online. Launch prefixes exist only in the saved SSH
// entry (a Ring Host has none), so a Host with one stays where they keep working.
export function ringMoveTarget(host: HostConfig | undefined, row: AvailableHost | undefined): HostId | null {
  if (!host || !row || row.id !== host.id || !isRingMember(row) || row.reachability.kind !== "online") return null;
  if (Object.values(host.launchPrefixes ?? {}).some(Boolean)) return null;
  return row.aliases.find(alias => RING_ID.test(alias)) ?? null;
}

// Resolves with the Ring id once the saved Host can move, or null after `timeoutMs` (or when
// the Host is no longer saved).
export function waitRingMoveTarget(id: HostId, timeoutMs = ringMove.waitMs): Promise<HostId | null> {
  return new Promise(resolve => {
    let done = false;
    const finish = (value: HostId | null) => {
      if (done) return;
      done = true;
      clearTimeout(timer); unsubscribe();
      resolve(value);
    };
    const check = () => {
      const host = registry.config(id);
      if (!host) return finish(null);
      const target = ringMoveTarget(host, registry.availableHost(id));
      if (target) finish(target);
    };
    const timer = setTimeout(() => finish(null), timeoutMs);
    const unsubscribe = registry.subscribe(check);
    check();
  });
}

// `key` with the Host part replaced, when it is a Project of `from`.
export function moveProjectKey(key: string, from: HostId, to: HostId): string {
  const prefix = `host:${from}:`;
  return key.startsWith(prefix) ? `host:${to}:${key.slice(prefix.length)}` : key;
}

// A Pi session's folder entry names its Host (`scopedSessionKey`): `["<host>","pi",path,id]`.
export function moveSessionFolderId(entry: string, from: HostId, to: HostId): string {
  if (!entry.startsWith(`["${from}",`)) return entry;
  try {
    const parts: unknown = JSON.parse(entry);
    if (Array.isArray(parts) && parts[0] === from) return JSON.stringify([to, ...parts.slice(1)]);
  } catch { /* not a scoped key */ }
  return entry;
}

// `["daemon", host, terminal]` keys (right panels, completion-seen): the entries of `from`
// continue under `to`; an entry `to` already has wins.
export function moveTerminalKeys<T>(values: Readonly<Record<string, T>>, from: HostId, to: HostId): Record<string, T> {
  const next: Record<string, T> = {};
  const moved: [string, T][] = [];
  for (const [key, value] of Object.entries(values)) {
    let parts: unknown = null;
    try { parts = JSON.parse(key); } catch { /* kept as it is */ }
    if (Array.isArray(parts) && parts.length === 3 && parts[0] === "daemon" && parts[1] === from) moved.push([JSON.stringify(["daemon", to, parts[2]]), value]);
    else next[key] = value;
  }
  for (const [key, value] of moved) if (!(key in next)) next[key] = value;
  return moved.length ? next : (values as Record<string, T>);
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
