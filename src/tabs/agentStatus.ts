import type { Tab } from "../types";
import { fmt, type StringKey } from "../hosts/strings";
import { isUsableStatus } from "../hosts/registry";
import { daemonHost } from "../hosts/localHost";
import type { AgentStatus, HostStatus, TerminalInfo } from "../hosts/types";

// A Tab's Agent Status and how the tab bar words it. Pure: TabBar renders the result, tests
// cover every branch.

// The hello capability of Daemons that report Agent Status.
export const AGENT_STATUS_CAPABILITY = "agent.status";

export const AGENT_STATUSES: readonly AgentStatus[] = ["working", "needs-you", "finished", "ended"];

// The agents whose hooks report a status; other agents and shells never show one.
const HOOK_AGENTS: readonly string[] = ["claude", "codex"];

// A value from the Daemon or the local store, or null when this Desktop does not know it.
export function asAgentStatus(v: unknown): AgentStatus | null {
  return typeof v === "string" && (AGENT_STATUSES as readonly string[]).includes(v) ? (v as AgentStatus) : null;
}

export interface AgentStatusContext {
  // Daemon Tabs (Remote, and Local ones on "local"; see `daemonHost`): the Host's last known
  // `terminals` list and its status. Ignored for in-process Tabs.
  live: TerminalInfo[] | null | undefined;
  status: HostStatus | undefined;
  // In-process Local Tabs: the statuses by Tab id.
  local: ReadonlyMap<string, AgentStatus>;
}

export interface TabAgentStatus {
  status: AgentStatus;
  // The Host is not connected: this is the last status it reported.
  stale: boolean;
}

export function agentStatusOf(tab: Tab, c: AgentStatusContext): TabAgentStatus | null {
  if ((tab.shellMode || "claude") === "raw") return null;
  if (!HOOK_AGENTS.includes(tab.agent || "claude")) return null;
  if (!daemonHost(tab)) {
    const s = asAgentStatus(c.local.get(tab.id));
    return s ? { status: s, stale: false } : null;
  }
  const s = asAgentStatus(c.live?.find(i => i.terminal === tab.terminal)?.agentStatus);
  if (!s) return null;
  if (isUsableStatus(c.status)) {
    // A Daemon without hooks never sends a status; checked anyway, as for Relaunch.
    if (!c.status?.daemonCapabilities?.includes(AGENT_STATUS_CAPABILITY)) return null;
    return { status: s, stale: false };
  }
  // Capabilities clear while disconnected; a status in the last list came from a Daemon
  // that had the capability.
  return { status: s, stale: true };
}

const KEY: Record<AgentStatus, { long: StringKey; short: StringKey }> = {
  "working": { long: "tab.agentStatus.working", short: "tab.agentStatus.short.working" },
  "needs-you": { long: "tab.agentStatus.needsYou", short: "tab.agentStatus.short.needsYou" },
  "finished": { long: "tab.agentStatus.finished", short: "tab.agentStatus.short.finished" },
  "ended": { long: "tab.agentStatus.ended", short: "tab.agentStatus.short.ended" },
};

export const agentStatusLabel = (s: AgentStatus): string => fmt(KEY[s].long);
export const agentStatusShortLabel = (s: AgentStatus): string => fmt(KEY[s].short);

export const TOOLTIP_LINE_MAX = 60;

// Break each line at spaces so none is longer than `max` (a single longer word stays whole).
export function wrapLines(text: string, max = TOOLTIP_LINE_MAX): string {
  return text.split("\n").map(line => {
    const out: string[] = [];
    let cur = "";
    for (const word of line.split(" ")) {
      if (cur && cur.length + 1 + word.length > max) { out.push(cur); cur = word; }
      else cur = cur ? `${cur} ${word}` : word;
    }
    out.push(cur);
    return out.join("\n");
  }).join("\n");
}

function staleText(s: TabAgentStatus, hostName: string): string {
  return fmt("tab.agentStatus.stale", { status: agentStatusShortLabel(s.status), host: hostName });
}

// The lines appended below the Tab's tooltip.
export function agentStatusTooltip(s: TabAgentStatus, hostName: string): string {
  return wrapLines(s.stale ? staleText(s, hostName) : agentStatusLabel(s.status));
}

// The badge's accessible name; the stale text's line break becomes a pause.
export function agentStatusAria(s: TabAgentStatus, hostName: string): string {
  const status = s.stale ? staleText(s, hostName).replace("\n", ", ") : agentStatusShortLabel(s.status);
  return fmt("tab.agentStatus.aria", { status });
}
