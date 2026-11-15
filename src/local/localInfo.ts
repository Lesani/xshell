// What `local_host_info` answers and the `local:daemon` event carries (src-tauri/src/hosts/
// local.rs, `LocalHostInfo`): how local Terminals run, the Daemon's state and what keeps it
// running. Declared here so the UI does not depend on `hosts/localHost.ts`.

export interface LocalDaemonState {
  state: "starting" | "ready" | "failed";
  error: string | null;
  // `older-desktop`: a window of an older xshell runs this machine's Terminals.
  errorKind: "older-desktop" | null;
  // The Daemon log, for the error view.
  log: string | null;
}

export interface LocalService {
  mechanism: string | null;
  linger: boolean | null;
  error: string | null;
}

export interface LocalStatus {
  mode: "daemon" | "in-process" | "failed";
  reason: string | null;
  // Null outside Daemon mode.
  daemon?: LocalDaemonState | null;
  // Null outside Daemon mode and on Windows: the keep-terminals setting does not apply.
  service?: LocalService | null;
}

export type LocalView = "ok" | "failed" | "ready";

// The one readiness predicate (U5): `failed` when the mode or the Daemon failed; `ready` when
// the Daemon runs and listed its Terminals; `ok` otherwise (starting, in-process, unknown).
export function localView(info: LocalStatus | null | undefined): LocalView {
  if (!info) return "ok";
  if (info.mode === "failed" || info.daemon?.state === "failed") return "failed";
  if (info.mode === "daemon" && info.daemon?.state === "ready") return "ready";
  return "ok";
}

// The log path the error view names. In mode `failed` the backend has no Daemon and so no
// path: the Daemon's default log location.
export const DEFAULT_DAEMON_LOG = "~/.xshell/log/xshelld.log";

export function logPath(info: LocalStatus | null | undefined): string {
  return info?.daemon?.log || DEFAULT_DAEMON_LOG;
}
