import { fmt, type StringKey } from "./strings";
import type { MemberRole, MemberView, RingStatus } from "./types";

// Settings → Mobile, the pure part: status lines, labels and the Relay URL form.

const STATUS_KEY: Record<MemberView["presence"]["kind"], StringKey> = {
  online: "mobile.status.online",
  closed: "mobile.status.closed",
  unreachable: "mobile.status.unreachable",
  never: "mobile.status.never",
  unknown: "mobile.status.unknown",
};

// A member's status chip. Without a Relay connection of its own the Desktop cannot know how
// the others stand.
export function presenceKey(m: MemberView, s: Pick<RingStatus, "connection">): StringKey {
  if (s.connection !== "connected" && !m.thisApp) return "mobile.status.unknown";
  return STATUS_KEY[m.presence.kind];
}

// The chip's colour class, reusing the Host chips.
export function presenceChip(key: StringKey): string {
  switch (key) {
    case "mobile.status.online": return "connected";
    case "mobile.status.unreachable": return "offline";
    case "mobile.status.closed": return "reconnecting";
    default: return "unknown";
  }
}

export function roleKey(role: MemberRole): StringKey {
  return role === "desktop" ? "mobile.role.desktop" : role === "daemon" ? "mobile.role.daemon" : "mobile.role.mobile";
}

// The connection line under the Relay settings; null while Mobile access is off.
export function connectionLine(s: RingStatus): string | null {
  switch (s.connection) {
    case "connected": return fmt(s.limited ? "mobile.conn.limited" : "mobile.conn.connected");
    case "connecting": return fmt("mobile.conn.connecting");
    case "waiting": return fmt("mobile.conn.retrying", { s: Math.max(1, s.retryIn ?? 1) });
    case "stopped": return fmt("mobile.conn.stopped");
    case "other-window": return fmt("mobile.conn.otherWindow");
    default: return null;
  }
}

// A Relay move still owed to the old Relay.
export function moveLine(s: RingStatus): string | null {
  if (!s.move) return null;
  return fmt(s.move.state === "failed" ? "mobile.conn.moveFailed" : "mobile.conn.moving");
}

export function localLine(s: Pick<RingStatus, "local">): string | null {
  if (s.local === "in-process") return fmt("mobile.local.inProcess");
  if (s.local === "too-old") return fmt("mobile.local.tooOld");
  return null;
}

// Whether Enable is the deliberate start-over after unreadable settings were set aside (the
// recovery-required state, whatever else the status says).
export function startsOver(s: Pick<RingStatus, "enabled" | "problem">): boolean {
  return s.problem === "recovered";
}

// Whether the Enable button is offered: off or recovery required, and not blocked by refused
// settings.
export function canEnable(s: Pick<RingStatus, "enabled" | "problem">): boolean {
  if (s.problem === "unreadable") return false;
  return s.problem === "recovered" || !s.enabled;
}

export function problemLine(s: RingStatus): string | null {
  if (s.problem === "recovered") return fmt("mobile.problem.recovered", { path: s.problemDetail ?? "" });
  if (s.problem === "unreadable") return fmt("mobile.problem.unreadable", { error: s.problemDetail ?? "" });
  return null;
}

// ── The Relay form ────────────────────────────────────────────────────────

export type RelayChoice = "hosted" | "custom";

export interface RelayForm {
  choice: RelayChoice;
  // The self-hosted URL as typed.
  url: string;
}

export function relayChoice(s: Pick<RingStatus, "relayUrl" | "hostedRelayUrl">): RelayChoice {
  return !s.relayUrl || s.relayUrl === s.hostedRelayUrl ? "hosted" : "custom";
}

// The form as the Ring stands: Hosted preselected unless the Ring is on another Relay.
export function initialForm(s: Pick<RingStatus, "relayUrl" | "hostedRelayUrl">): RelayForm {
  const choice = relayChoice(s);
  return { choice, url: choice === "custom" ? (s.relayUrl ?? "") : "" };
}

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

// The URL saving would set.
export function targetUrl(f: RelayForm, s: Pick<RingStatus, "hostedRelayUrl">): string {
  return f.choice === "hosted" ? s.hostedRelayUrl : f.url.trim();
}

// The inline error under the URL field, if any (nothing for an empty field).
export function urlError(f: RelayForm): string | null {
  if (f.choice !== "custom" || f.url.trim() === "" || validRelayUrl(f.url)) return null;
  return fmt("mobile.relay.err.invalid");
}

// Whether the form differs from the Ring's Relay.
export function isDirty(f: RelayForm, s: Pick<RingStatus, "relayUrl" | "hostedRelayUrl">): boolean {
  return targetUrl(f, s) !== (s.relayUrl ?? "");
}

// Saving waits while a previous relay change is still reaching the paired devices.
export function canSave(f: RelayForm, s: Pick<RingStatus, "relayUrl" | "hostedRelayUrl" | "move">, busy: boolean): boolean {
  if (busy || s.move || !isDirty(f, s)) return false;
  return f.choice === "hosted" || validRelayUrl(f.url);
}

// Command errors arrive as strings (or objects with a message).
export function errorText(e: unknown): string {
  if (typeof e === "string") return e;
  const m = (e as { message?: unknown })?.message;
  return typeof m === "string" ? m : String(e);
}
