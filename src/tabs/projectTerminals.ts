import type { AgentId } from "../agents";
import type { Group, Tab } from "../types";
import { daemonHost } from "../hosts/localHost";
import { isUsableStatus, type RegistrySnapshot } from "../hosts/registry";
import { runtimeTabProjectKey } from "../hosts/projectRuntime";
import { sessionKeyOfTab } from "../hosts/projectKey";
import { tabName } from "./tabName";
import { asAgentStatus } from "./agentStatus";
import type { AgentStatus } from "../hosts/types";

export interface ProjectTerminal {
  tabId: string;
  terminalKey: string;
  projectKey: string;
  title: string;
  detail: string;
  host?: string;
  agent: AgentId;
  kind: "agent" | "shell";
  state: "live" | "last-known" | "unconfirmed" | "status-unavailable" | "ended" | "closing";
  agentStatus: AgentStatus | null;
  sessionKey: string | null;
  groupName?: string;
}

// Presentation only: Tabs remain the inventory; snapshots qualify their currentness.
export function projectTerminals(tabs: Tab[], groups: Group[], hosts: RegistrySnapshot, closing: ReadonlySet<string>, local: ReadonlyMap<string, AgentStatus> = new Map()): ProjectTerminal[] {
  return tabs.flatMap(tab => {
    const projectKey = runtimeTabProjectKey(tab);
    if (tab.type !== "terminal" || !projectKey) return [];
    const wireHost = daemonHost(tab);
    const info = wireHost ? hosts.live[wireHost]?.find(t => t.terminal === tab.terminal) : undefined;
    const current = !!wireHost && isUsableStatus(hosts.status[wireHost]) && !!info;
    // A legacy in-process Tab alone cannot establish whether its process still runs.
    const state = closing.has(tab.id) ? "closing"
      : (info && (info.exitCode !== null || info.agentStatus === "ended")) || (!wireHost && local.get(tab.id) === "ended") ? "ended"
      : !wireHost && !tab.host ? "status-unavailable"
      : current ? "live" : wireHost && isUsableStatus(hosts.status[wireHost]) ? "unconfirmed" : "last-known";
    const name = tabName(tab);
    return [{ tabId: tab.id, terminalKey: wireHost ? JSON.stringify([wireHost, tab.terminal]) : JSON.stringify(["in-process", tab.id]), projectKey,
      title: name.primary, detail: name.secondary, host: tab.host, agent: tab.agent ?? "claude", kind: tab.shellMode === "raw" ? "shell" : "agent", state,
      agentStatus: state === "live" ? asAgentStatus(info?.agentStatus) : null,
      sessionKey: sessionKeyOfTab(tab), groupName: groups.find(g => g.id === tab.groupId)?.name }];
  });
}
