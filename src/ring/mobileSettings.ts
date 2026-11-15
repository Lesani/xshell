import { fmt, type StringKey } from "./strings";
import type { RingStatus } from "./types";

// Settings → Ring, the pure part of the lines kept from the earlier Ring section: the Relay,
// the connection, the daily quota, and why pairing a phone waits.

// A Relay URL as the copy names it: the host, with the port if any, never the full URL.
export function relayHost(url: string | null | undefined): string {
  if (!url) return "";
  const m = /^[a-z][a-z0-9+.-]*:\/\/([^/?#]*)/i.exec(url.trim());
  return m ? m[1].replace(/^[^@]*@/, "") : url;
}

// The connection line under the Relay; null while there is no connection to report.
export function connectionLine(s: RingStatus): string | null {
  const relay = relayHost(s.relayUrl);
  switch (s.connection) {
    case "connected": return s.limited ? fmt("mobile.conn.limited") : fmt("mobile.conn.connected", { relay });
    case "connecting": return fmt("mobile.conn.connecting", { relay });
    case "waiting": return fmt("mobile.conn.retrying", { relay, s: Math.max(1, s.retryIn ?? 1) });
    case "stopped": return fmt("mobile.conn.stopped");
    case "other-window": return fmt("mobile.conn.otherWindow");
    default: return null;
  }
}

// The reset time of the daily quota, in local time (HH:MM), as the phone app shows it.
export function formatResetTime(unixS: number): string {
  return new Date(unixS * 1000).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

// The notice while the Ring is over its Relay's daily message quota (#42); null when it is not,
// once the reset time has passed, or in a window that does not run the connection.
export function quotaLine(
  s: Pick<RingStatus, "quotaResetAt" | "relayHosted" | "connection">,
  nowMs: number,
  fmtTime: (unixS: number) => string = formatResetTime,
): string | null {
  if (s.quotaResetAt == null || s.quotaResetAt * 1000 <= nowMs || s.connection === "other-window") return null;
  return fmt(s.relayHosted ? "mobile.quota.hosted" : "mobile.quota.ownRelay", { time: fmtTime(s.quotaResetAt) });
}

// COPY-GAP: `mobile.local.tooOld` predates the new copy (it says "mobile access"); kept as the
// nearest approved line for a local Daemon too old to join.
export function localLine(s: Pick<RingStatus, "local">): string | null {
  return s.local === "too-old" ? fmt("mobile.local.tooOld") : null;
}

// Refused Ring settings (left in place). COPY-GAP: kept from the earlier copy. A `recovered`
// file needs no line: Create starts over and clears it.
export function problemLine(s: Pick<RingStatus, "problem" | "problemDetail">): string | null {
  if (s.problem === "unreadable") return fmt("mobile.problem.unreadable", { error: s.problemDetail ?? "" });
  return null;
}

// ── Pairing a phone and the connection (xshell#38) ──────────────────────────

// Why pairing cannot succeed now, as the note shown with its buttons disabled; null when it
// can. While connecting it is offered: the Desktop waits for the connection to add a device.
export function pairingBlocked(s: Pick<RingStatus, "connection">): StringKey | null {
  switch (s.connection) {
    case "waiting": return "mobile.pair.needsRelay";
    case "stopped": return "mobile.pair.needsMember";
    default: return null;
  }
}

// The note under a pairing panel's title: that pairing waits for the connection (while
// connecting, or while an offer already shown stays valid during a retry), or why it is
// disabled; null when connected.
export function pairingNote(s: Pick<RingStatus, "connection">, offerShown = false): StringKey | null {
  if (s.connection === "connecting" || (offerShown && s.connection === "waiting")) return "mobile.pair.connecting";
  return pairingBlocked(s);
}

// ── The Relay chosen before the Ring exists ──────────────────────────────────

const LOOPBACK = /^(localhost|127(\.\d{1,3}){3}|\[::1\])(:\d{1,5})?$/i;

// The client-side check: wss:// anywhere, ws:// only to loopback; no userinfo, query,
// fragment or escapes. The Desktop checks the full rules again.
export function validRelayUrl(raw: string): boolean {
  const url = raw.trim();
  const m = /^(wss?):\/\/([^/?#@%\\\s]+)(\/[^?#%\\\s]*)?$/i.exec(url);
  if (!m) return false;
  const [, scheme, authority] = m;
  if (scheme.toLowerCase() === "ws") return LOOPBACK.test(authority);
  return authority.length > 0 && !authority.startsWith(":");
}

// The inline error under the URL field, if any (nothing for an empty field: the hosted Relay).
export function relayUrlError(raw: string): string | null {
  if (raw.trim() === "" || validRelayUrl(raw)) return null;
  return fmt("mobile.relay.err.invalid");
}
