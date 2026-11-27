import { fmt } from "./strings";
import type { RingStatus } from "./types";
import { C, fmtc } from "./copy";

// Settings → Ring, the pure part of the lines kept from the earlier Ring section: the Relay,
// the connection, the daily quota, and why pairing a phone waits.

// A Relay URL as the copy names it: the host, with the port if any, never the full URL.
export function relayHost(url: string | null | undefined): string {
  if (!url) return "";
  const m = /^[a-z][a-z0-9+.-]*:\/\/([^/?#]*)/i.exec(url.trim());
  return m ? m[1].replace(/^[^@]*@/, "") : url;
}

// The Ring uses the default service (no address is shown) rather than a self-chosen address.
export function usesDefaultService(s: Pick<RingStatus, "relayUrl" | "hostedRelayUrl" | "relayHosted">): boolean {
  return !s.relayUrl || s.relayHosted || s.relayUrl === s.hostedRelayUrl;
}

// "{n} seconds", or "1 second" (copy-rjw-3: "second" when n is 1).
function seconds(text: string, n: number): string {
  return n === 1 ? text.replace(/\b1 seconds\b/, "1 second") : text;
}

// The connection line in Settings → Ring; null while there is nothing to report.
export function connectionLine(s: RingStatus): string | null {
  const address = relayHost(s.relayUrl);
  const def = usesDefaultService(s);
  switch (s.connection) {
    // COPY-GAP: `mobile.conn.limited` predates the new copy and still names the hosted relay.
    case "connected": return s.limited ? fmt("mobile.conn.limited") : def ? C["ring.connection.default.connected"] : fmtc("ring.connection.custom.connected", { address });
    // COPY-GAP: no text while connecting to the default service; a chosen address keeps the
    // earlier line (it names only the address).
    case "connecting": return def ? null : fmt("mobile.conn.connecting", { relay: address });
    case "waiting": {
      const n = Math.max(1, s.retryIn ?? 1);
      return seconds(def ? fmtc("ring.connection.default.retry", { n }) : fmtc("ring.connection.custom.retry", { address, n }), n);
    }
    case "stopped": return fmt("mobile.conn.stopped");
    case "other-window": return fmt("mobile.conn.otherWindow");
    default: return null;
  }
}

// Why Pair a phone / Add a machine cannot work now, or null: the service cannot be reached, or
// this machine is no longer a member. While connecting they are offered.
export function pairingLine(s: Pick<RingStatus, "connection" | "relayUrl" | "hostedRelayUrl" | "relayHosted">, offerShown = false): string | null {
  if (s.connection === "stopped") return fmt("mobile.pair.needsMember");
  if (s.connection !== "waiting" || offerShown) return null;
  return usesDefaultService(s) ? C["ring.connection.default.required"] : fmtc("ring.connection.custom.required", { address: relayHost(s.relayUrl) });
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

// This machine's Daemon is too old to join a Ring.
export function localLine(s: Pick<RingStatus, "local">): string | null {
  return s.local === "too-old" ? C["mobile.local.tooOld"] : null;
}

// Refused Ring settings (left in place), without the backend's detail. A `recovered` file needs
// no line: Create starts over and clears it.
export function problemLine(s: Pick<RingStatus, "problem">): string | null {
  return s.problem === "unreadable" ? C["mobile.problem.unreadable"] : null;
}

// ── Pairing a phone and the connection (xshell#38) ──────────────────────────

// Whether pairing cannot succeed now (its buttons are disabled; `pairingLine` says why).
export function pairingBlocked(s: Pick<RingStatus, "connection">): boolean {
  return s.connection === "waiting" || s.connection === "stopped";
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
