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
  // Settings → Hosts → Ring offers to remove it: not this app, not this computer, and no configured
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
  // Old Relays that still owe a farewell: devices there are told about the current Relay.
  farewells: FarewellView[];
  problem: "recovered" | "unreadable" | null;
  problemDetail: string | null;
  members: MemberView[];
  // While the Ring is over its Relay's daily message quota: when the quota resets (Unix
  // seconds). Only in the window that runs the connection; null otherwise.
  quotaResetAt: number | null;
  // The Ring's Relay is the Hosted one, not the user's own (the Desktop compares origins).
  relayHosted: boolean;
  // How this computer's terminals run: a Daemon that can join, inside the app, or a Daemon too
  // old to join.
  local: "daemon" | "in-process" | "too-old";
  // The Remote Hosts not in the Ring, and why (empty when every connected Host joined).
  hosts: HostRingState[];
  // Whether this Desktop can add computers (#87): `unavailable` means an upgrade is needed but
  // cannot run here; `unresolved` means earlier adoption work needs reconciliation.
  approval: Approval;
  // The retained Ring's Roster format: a v2 Ring's Relay is fixed (shown read-only).
  format: 1 | 2 | null;
  // An adoption, approval or registration runs now: nothing new can start (not uncertainty).
  busy: boolean;
}

export type Approval = "ready" | "needs-upgrade" | "needs-completion" | "unavailable" | "unresolved" | "none";

// `other-ring`: paired with another Desktop's devices; `ring_claim_host` pairs it here.
export type HostRingKind = "too-old" | "other-ring" | "full" | "failed";

export interface FarewellView {
  relayUrl: string;
  state: "pending" | "sending" | "waiting";
  error?: string;
  retryIn?: number;
  failures: number;
}

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

// Computer adoption (#87): `ring_join_start`'s reply (the only place the words appear), the
// `ring:join` event and `ring_join_overview`.

export interface JoinStarted {
  operation: string;
  // Eight words joined by `-`.
  words: string;
  expiresAt: number;
}

export type JoinOrigin = "gui" | "cli";
export type JoinStateName = "waiting" | "awaitingConfirmation" | "completing" | "installed" | "declined" | "expired" | "failed" | "reconciliationRequired" | "cancelled";

export interface JoinPreview {
  ringId: string;
  ringShort: string;
  // The approving device's name.
  approver: string | null;
  // This computer's name.
  computer: string;
  expiresAt: number;
}

export interface JoinEvent {
  operation: string;
  origin: JoinOrigin;
  state: JoinStateName;
  preview?: JoinPreview;
  error?: string;
  expiresAt: number;
}

export interface JoinOverview {
  unavailable?: "platform" | "in-process" | "not-registered";
  // This window owns pairing (otherwise another xshell window does).
  owner: boolean;
  joins: JoinEvent[];
}

export interface ApprovedComputer {
  names: string[];
  version: number;
}
