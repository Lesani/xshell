// TS mirror of the Desktop ↔ Remote Host contract (Rust serde camelCase; enum values
// kebab-case). The Rust side lives in src-tauri/crates/hostlink and src-tauri/src/hosts.

export type HostId = string; // ^h_[a-z0-9]{8}$ ; "local" is reserved and never sent

export interface HostConfig {
  id: HostId;
  name: string;
  sshTarget: string;
  color?: string;
  daemonCommand?: string;
}

export type HostStatusKind = "reconnecting" | "connected" | "upgrade-pending" | "offline" | "incompatible";

export type HostErrorHint =
  | "host-key"
  | "permission-denied"
  | "unresolved"
  | "unreachable"
  | "ssh-missing"
  | "unsupported-platform"
  | "binary-unavailable"
  | "daemon-command-failed";

export interface HostStatus {
  host: HostId;
  status: HostStatusKind;
  phase: "probing" | "installing" | "upgrading" | null;
  lastError: string | null;
  errorHint: HostErrorHint | null;
  daemonVersion: string | null;
  desktopVersion: string;
  protocol: number | null;
  os: "linux" | "macos" | null;
  arch: string | null;
  incompatibleReason: "daemon-older" | "daemon-newer" | null;
  nextRetryAt: number | null; // ms epoch
  sinceMs: number; // when this status began
  // Amendment 20: bumped when `hosts_configure` replaces this host's connection. Mounted
  // remote Tabs re-run their attach once usable when it changes. Optional so a Rust side
  // that does not send it yet is still accepted.
  configGeneration?: number;
}

export interface LaunchSpec {
  agent?: string | null;
  sessionId?: string | null;
  cwd: string;
  shellMode?: string | null;
  shellCommand?: string | null;
  shellId?: string | null;
  fullscreenRendering?: boolean | null;
  forceSyncOutput?: boolean | null;
}

export interface TerminalInfo {
  terminal: string;
  spec: LaunchSpec;
  meta: Record<string, unknown>;
  createdAtMs: number;
  pid: number | null;
  exitCode: number | null;
}

export interface HostSnapshot {
  status: HostStatus;
  terminals: TerminalInfo[] | null; // null = never connected this run
}

export type HostErrorCode = "unknown-host" | "offline" | "incompatible" | "timeout" | "remote" | "busy" | "invalid";

export interface HostError {
  code: HostErrorCode;
  message: string;
}

export interface HostTestResult {
  ok: boolean;
  os: string | null;
  arch: string | null;
  triple: string | null;
  installedVersion: string | null;
  error: string | null;
  errorHint: HostErrorHint | null;
}

// Payload of the `hosts:terminals` event.
export interface HostTerminalsEvent {
  host: HostId;
  list: TerminalInfo[];
}

// Amendment 8: a remote Terminal's exit carries a watermark — the number of bytes the
// attachment delivered on its data channel before the exit. The local `spawn_terminal`
// exit channel still sends a bare number.
export interface RemoteExit {
  code: number;
  bytes: number;
}

// Meta keys the Desktop writes on a Terminal (opaque to the Daemon).
export interface TerminalMeta {
  title?: string;
  projectName?: string;
  createdAt?: number;
}

export function isHostError(e: unknown): e is HostError {
  return typeof e === "object" && e !== null && typeof (e as HostError).code === "string" && typeof (e as HostError).message === "string";
}
