import type { Tab } from "../types";
import { AGENTS } from "../agents";
import { fmt } from "../hosts/strings";
import { isUsableStatus } from "../hosts/registry";
import { daemonHost } from "../hosts/localHost";
import type { HostStatus, TerminalInfo } from "../hosts/types";

// Whether a Tab offers "skip permissions" and in what state. Pure: TerminalTab renders the
// result, tests cover every branch.

// The hello capability of Daemons that can restart a Terminal in place.
export const RELAUNCH_CAPABILITY = "term.relaunch";

export type SkipPermsState =
  | { kind: "hidden" }
  | { kind: "disabled"; reason: string }
  | { kind: "available"; on: boolean };

export interface SkipPermsContext {
  // Daemon Tabs (Remote, and Local ones on "local"): the Host's live `terminals` list and
  // status. Ignored for in-process Tabs.
  live: TerminalInfo[] | null | undefined;
  status: HostStatus | undefined;
  hostName: string;
  // A Local Tab's process has ended (remote Tabs read it from the list).
  localEnded: boolean;
  // A relaunch of this Tab is in flight.
  busy: boolean;
}

// Whether the flag is on. A remote Tab reads it from the Daemon's spec (ADR-0001), never
// from its own copy.
export function skipPermsOn(tab: Tab, live: TerminalInfo[] | null | undefined): boolean {
  if (!daemonHost(tab)) return !!tab.skipPermissions;
  return !!live?.find(i => i.terminal === tab.terminal)?.spec.skipPermissions;
}

export function skipPermsState(tab: Tab, c: SkipPermsContext): SkipPermsState {
  if ((tab.shellMode || "claude") === "raw") return { kind: "hidden" };
  if (!AGENTS[tab.agent || "claude"].bypassFlag) return { kind: "hidden" };
  let sessionId = tab.sessionId;
  let ended = c.localEnded;
  if (daemonHost(tab)) {
    if (!isUsableStatus(c.status)) return { kind: "disabled", reason: fmt("tab.skipPerms.disabled.hostUnavailable", { host: c.hostName }) };
    // A Daemon that cannot restart Terminals never gets the request.
    if (!c.status?.daemonCapabilities?.includes(RELAUNCH_CAPABILITY)) return { kind: "hidden" };
    const entry = c.live?.find(i => i.terminal === tab.terminal);
    if (!entry) return { kind: "disabled", reason: fmt("tab.skipPerms.disabled.hostUnavailable", { host: c.hostName }) };
    sessionId = entry.spec.sessionId ?? undefined;
    ended = entry.exitCode != null;
  }
  if (c.busy) return { kind: "disabled", reason: fmt("tab.skipPerms.button.busy") };
  if (ended) return { kind: "disabled", reason: fmt("tab.skipPerms.disabled.ended") };
  if (!sessionId) return { kind: "disabled", reason: fmt("tab.skipPerms.disabled.noSession") };
  return { kind: "available", on: skipPermsOn(tab, c.live) };
}
