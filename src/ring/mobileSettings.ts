import { fmt, type StringKey } from "./strings";
import type { HostRingState, MemberRole, MemberView, RingStatus } from "./types";
import { timeAgo } from "../utils";

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

const HOST_KEY: Record<HostRingState["state"], StringKey> = {
  "too-old": "mobile.host.tooOld",
  "other-ring": "mobile.host.otherRing",
  full: "mobile.host.full",
  failed: "mobile.host.failed",
};

// The note for a Remote Host that is not in the Ring.
export function hostLine(h: HostRingState): string {
  return fmt(HOST_KEY[h.state], { name: h.name, error: h.error ?? "" });
}

// Whether the note offers "Pair with this desktop".
export function canClaim(h: HostRingState): boolean {
  return h.state === "other-ring";
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

// ── Pairing and the connection (xshell#38) ──────────────────────────────────

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

// ── Removing a device (#22) ─────────────────────────────────────────────────

// Whether a member's row offers Remove: the Desktop says it may go, and this window runs the
// connection (another window's removal would be refused).
export function canRemove(m: MemberView, s: Pick<RingStatus, "connection">): boolean {
  return m.removable && !m.thisApp && !m.thisComputer && s.connection !== "other-window";
}

// Why a Host's row (this computer's, or a configured Remote Host's) has no Remove button.
export function removeHint(m: MemberView): string | null {
  if (m.removable || m.thisApp) return null;
  return m.thisComputer || m.hostId ? fmt("mobile.remove.hostHint") : null;
}

// "Last seen …" under a device that is closed or unreachable, while this Desktop is connected
// and so knows. Uses Date.now().
export function lastSeenLine(m: MemberView, s: Pick<RingStatus, "connection">): string | null {
  if (s.connection !== "connected" || m.thisApp) return null;
  const { kind, at } = m.presence;
  if ((kind !== "closed" && kind !== "unreachable") || at == null) return null;
  const ago = timeAgo(new Date(at * 1000).toISOString());
  return ago ? fmt("mobile.member.lastSeen", { ago }) : null;
}

const CONFIRM_BODY: Record<MemberRole, StringKey> = {
  mobile: "mobile.remove.confirmBody.mobile",
  daemon: "mobile.remove.confirmBody.daemon",
  desktop: "mobile.remove.confirmBody.desktop",
};

export function removeConfirm(m: Pick<MemberView, "name" | "role">): { title: string; body: string; confirm: string } {
  return {
    title: fmt("mobile.remove.confirmTitle", { name: m.name }),
    body: fmt(CONFIRM_BODY[m.role], { name: m.name }),
    confirm: fmt("mobile.remove.confirm"),
  };
}

const REMOVE_REFUSAL: Record<string, StringKey> = {
  in_use: "mobile.remove.err.inUse",
  self: "mobile.remove.err.self",
  other_window: "mobile.remove.err.otherWindow",
};

// A failed removal: the Desktop's refusals start with a code (`in_use: …`).
export function removeErrorLine(name: string, e: unknown): string {
  const text = errorText(e);
  const m = /^([a-z_]+): (.*)$/s.exec(text);
  const key = m ? REMOVE_REFUSAL[m[1]] : undefined;
  if (key) return fmt(key);
  return fmt("mobile.remove.failed", { name, error: m ? m[2] : text });
}

// After a removal while this Desktop is not connected to the relay: removed here only, so far.
export function removePendingLine(s: Pick<RingStatus, "connection">, name: string): string | null {
  return s.connection === "connected" ? null : fmt("mobile.remove.pending", { name });
}

// A removal as Settings → Mobile follows it, kept apart from the member list (the row goes
// away when it succeeds; the notes stay).
export type Removal =
  | { state: "idle" }
  | { state: "confirming"; member: MemberView }
  | { state: "removing"; signKey: string; name: string }
  | { state: "failed"; signKey: string; line: string }
  | { state: "removed"; pending: string | null };

export type RemovalAction =
  | { type: "ask"; member: MemberView }
  | { type: "cancel" }
  | { type: "start" }
  | { type: "done"; status: Pick<RingStatus, "connection"> }
  | { type: "failed"; error: unknown };

export const REMOVAL_IDLE: Removal = { state: "idle" };

export function removalReducer(r: Removal, a: RemovalAction): Removal {
  switch (a.type) {
    case "ask": return r.state === "removing" ? r : { state: "confirming", member: a.member };
    case "cancel": return r.state === "confirming" ? REMOVAL_IDLE : r;
    case "start": return r.state === "confirming" ? { state: "removing", signKey: r.member.signKey, name: r.member.name } : r;
    case "done": return r.state === "removing" ? { state: "removed", pending: removePendingLine(a.status, r.name) } : r;
    case "failed": return r.state === "removing" ? { state: "failed", signKey: r.signKey, line: removeErrorLine(r.name, a.error) } : r;
  }
}

// The note under the device list for a removal, if any.
export function removalNote(r: Removal): { text: string; error: boolean } | null {
  if (r.state === "failed") return { text: r.line, error: true };
  if (r.state === "removed" && r.pending) return { text: r.pending, error: false };
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
