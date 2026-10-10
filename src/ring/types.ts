// What the `ring_*` commands answer and the `ring:status` event carries (src-tauri/src/ring.rs,
// xshell_hostlink::ring::RingView).

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
  // Settings → Mobile offers to remove it: not this app, not this computer, and no configured
  // Host's Daemon.
  removable: boolean;
  presence: { kind: PresenceKind; reason?: string; at?: number };
}

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
  // A Relay move still owed to the old Relay.
  move: { state: "moving" | "failed"; error?: string } | null;
  problem: "recovered" | "unreadable" | null;
  problemDetail: string | null;
  members: MemberView[];
  // How this computer's terminals run: a Daemon that can join, inside the app, or a Daemon too
  // old to join.
  local: "daemon" | "in-process" | "too-old";
  // The Remote Hosts not in the Ring, and why (empty when every connected Host joined).
  hosts: HostRingState[];
}

// `other-ring`: paired with another Desktop's devices; `ring_claim_host` pairs it here.
export type HostRingKind = "too-old" | "other-ring" | "full" | "failed";

export interface HostRingState {
  host: string;
  name: string;
  state: HostRingKind;
  error?: string;
}

// Pairing (#9): what `ring_pair_phone_start` answers and the `ring:pairing` event carries.

// `rows[y][x]` is "1" for a dark module. No quiet zone; the UI adds the margin.
export interface Qr {
  size: number;
  rows: string[];
}

export interface PhoneStart {
  // The `xsp1.…` pairing text the QR code encodes.
  payload: string;
  qr: Qr;
  // Unix seconds; shown only, the Desktop enforces the expiry.
  expiresAt: number;
}

export type PairingFlow = "phone" | "computer";

export type PairingEvent =
  | { flow: PairingFlow; state: "waiting" }
  | { flow: PairingFlow; state: "paired"; name: string; role: MemberRole }
  | { flow: PairingFlow; state: "expired" }
  | { flow: PairingFlow; state: "failed"; code: string };
