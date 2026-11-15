// What the `ring_*` commands answer and the `ring:status` event carries (src-tauri/src/ring.rs,
// src-tauri/src/ring/join.rs, xshell_hostlink::ring::RingView).

export type MemberRole = "desktop" | "daemon" | "mobile";
export type PresenceKind = "online" | "closed" | "unreachable" | "never" | "unknown";
// `other-window`: another xshell window runs the connection; this one shows the state on disk.
export type Connection = "off" | "connecting" | "connected" | "waiting" | "stopped" | "other-window";

export interface MemberView {
  name: string;
  role: MemberRole;
  signKey: string;
  thisApp: boolean;
  thisComputer: boolean;
  // The configured Remote Host (still in Settings → Hosts) whose Daemon this member is.
  hostId: string | null;
  // Every row of a format-2 Ring, this machine's own included.
  removable: boolean;
  presence: { kind: PresenceKind; reason?: string; at?: number };
  // The member's enrollment unit. A Computer is one row: its Desktop's name and signKey, its
  // Daemon in daemonKey (equal to its `AvailableHost.daemonKey`); removing it removes both.
  enrollmentId: string | null;
  kind: EnrollmentKind | null;
  daemonKey: string | null;
  // A grouped Computer: its Daemon's own presence (presence is online when either key is).
  daemonPresence: { kind: PresenceKind; reason?: string; at?: number } | null;
}

export type EnrollmentKind = "computer" | "daemon" | "mobile" | "legacyDesktop";

export type Membership = "none" | "member" | "finishing" | "removed";

export interface RingStatus {
  enabled: boolean;
  ringId: string | null;
  version: number | null;
  relayUrl: string | null;
  hostedRelayUrl: string;
  connection: Connection;
  // While waiting: seconds until the next attempt.
  retryIn: number | null;
  connectionError: string | null;
  limited: boolean;
  // `recovered`: an unreadable file was set aside (Create clears it); `unreadable`: refused,
  // left in place until it can be read.
  problem: "recovered" | "unreadable" | null;
  problemDetail: string | null;
  members: MemberView[];
  // While the Ring is over its Relay's daily message quota: when the quota resets (Unix
  // seconds). Only in the window that runs the connection; null otherwise.
  quotaResetAt: number | null;
  // The Ring's Relay is the Hosted one, not the user's own (the Desktop compares origins).
  relayHosted: boolean;
  // This computer's Daemon can join, or is too old to.
  local: "daemon" | "too-old";
  // The Remote Hosts not in the Ring, and why (empty when every connected Host joined).
  hosts: HostRingState[];
  // `finishing`: earlier Join/Create/approval work is finished automatically (Create and
  // Join wait); `removed`: another device removed this machine.
  membership: Membership;
  // Set while `membership` is `removed` (Unix seconds).
  removed: { ringId: string; at: number } | null;
  // A stored format-1 Ring was set aside at start, until Create or Join (Unix seconds).
  oldRing: { at: number } | null;
  // `membership` is `finishing` and the Relay cannot be reached right now.
  finishingOffline: boolean;
  // An adoption, approval or registration runs now: nothing new can start.
  busy: boolean;
}

export type HostRingKind = "too-old" | "other-ring" | "full" | "failed";

export interface HostRingState {
  host: string;
  name: string;
  state: HostRingKind;
  error?: string;
}

// Phone pairing (#9): what `ring_pair_phone_start` answers and the `ring:pairing` event carries.

// `rows[y][x]` is "1" for a dark module. No quiet zone; the UI adds the margin.
export interface Qr {
  size: number;
  rows: string[];
}

export interface PhoneStart {
  operation: string;
  // The `xsp1.…` pairing text the QR code encodes.
  payload: string;
  qr: Qr;
  // Unix seconds; shown only, the Desktop enforces the expiry.
  expiresAt: number;
}

export type PairingFlow = "phone" | "computer";

export type PairingEvent = { operation: string } & (
  | { flow: PairingFlow; state: "waiting" }
  | { flow: PairingFlow; state: "paired"; name: string; role: MemberRole }
  | { flow: PairingFlow; state: "expired" }
  | { flow: PairingFlow; state: "failed"; code: string });

// Create, Join and Add a machine: `ring_join_start`'s reply (the only place the words appear),
// the `ring:join` event and `ring_join_overview`.

export interface JoinStarted {
  operation: string;
  // Eight words joined by `-`.
  words: string;
  expiresAt: number;
}

export type JoinOrigin = "gui" | "cli";
export type JoinStateName = "waiting" | "completing" | "installed" | "finishing" | "expired" | "cancelled" | "failed";

export interface JoinEvent {
  operation: string;
  origin: JoinOrigin;
  state: JoinStateName;
  // `code: detail`.
  error?: string;
  expiresAt: number;
}

export interface JoinOverview {
  unavailable?: "platform" | "not-registered" | null;
  // This window owns pairing (otherwise another xshell window does).
  owner: boolean;
  joins: JoinEvent[];
}

export interface ApprovedComputer {
  names: string[];
  version: number;
}
