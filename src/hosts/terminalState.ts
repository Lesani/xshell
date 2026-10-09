import { fmt } from "./strings";
import type { HostStatus, TerminalInfo } from "./types";

// What a Daemon Tab shows while its Host is not usable. Pure: TerminalTab renders it.

export type TerminalHostState = null | "waiting" | "reconnecting" | "offline" | "incompatible";

// null when the Host is usable. `live` is the Host's last list; `started` whether this Tab's
// Terminal ever started in this mount.
export function terminalHostState(status: HostStatus | undefined, live: TerminalInfo[] | null | undefined, started: boolean): TerminalHostState {
  const st = status?.status;
  if (st === "connected" || st === "upgrade-pending") return null;
  if (st === "incompatible") return "incompatible";
  if (st === "offline") return "offline";
  return live == null || !started ? "waiting" : "reconnecting";
}

// The banner text. A Local Daemon Tab (`local`) is worded without a Host name; a Local Host
// that cannot be used (it never is incompatible: one build) reports its last error.
export function terminalHostStateText(state: TerminalHostState, local: boolean, hostName: string, lastError: string | null | undefined): string | null {
  if (!state) return null;
  if (!local) return fmt(`terminal.remote.${state}` as const, { host: hostName });
  if (state === "waiting") return fmt("terminal.local.waiting");
  if (state === "reconnecting") return fmt("terminal.local.reconnecting");
  return fmt("terminal.local.offline", { error: (lastError || "").trim() || fmt("hosts.status.offline").toLowerCase() });
}

// The error line when a Daemon Tab's Terminal cannot start.
export function terminalOpenFailedText(local: boolean, hostName: string, error: string): string {
  return local ? fmt("terminal.local.openFailed", { error }) : fmt("terminal.remote.openFailed", { host: hostName, error });
}
