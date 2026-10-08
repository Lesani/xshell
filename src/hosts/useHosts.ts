import { useSyncExternalStore } from "react";
import { AGENT_IDS, type AgentId } from "../agents";
import { registry, isUsableStatus, type RegistrySnapshot } from "./registry";
import { fmt } from "./strings";
import type { HostConfig, HostId, HostStatus, TerminalInfo } from "./types";

export function useHostsSnapshot(): RegistrySnapshot {
  return useSyncExternalStore(registry.subscribe, registry.getSnapshot);
}

export function useHostConfigs(): HostConfig[] {
  return useHostsSnapshot().configs;
}

export function useHostStatus(id: HostId | undefined): HostStatus | undefined {
  const snap = useHostsSnapshot();
  return id ? snap.status[id] : undefined;
}

export function useHostLive(id: HostId | undefined): TerminalInfo[] | null | undefined {
  const snap = useHostsSnapshot();
  return id ? snap.live[id] : undefined;
}

const NO_AGENTS = Object.fromEntries(AGENT_IDS.map(a => [a, false])) as Record<AgentId, boolean>;

export function useHostAgents(id: HostId | undefined): Record<AgentId, boolean> {
  const snap = useHostsSnapshot();
  return (id && snap.agents[id]) || NO_AGENTS;
}

export function usableHosts(snap: RegistrySnapshot = registry.getSnapshot()): HostId[] {
  return snap.configs.filter(c => isUsableStatus(snap.status[c.id])).map(c => c.id);
}

// "Connected", "Reconnecting · Checking…" … — the chip text used everywhere.
export function statusLabel(s: HostStatus | undefined): string {
  if (!s) return fmt("hosts.status.reconnecting");
  const base = {
    "connected": fmt("hosts.status.connected"),
    "reconnecting": fmt("hosts.status.reconnecting"),
    "offline": fmt("hosts.status.offline"),
    "upgrade-pending": fmt("hosts.status.upgradePending"),
    "incompatible": fmt("hosts.status.incompatible"),
  }[s.status];
  return base;
}

export function phaseLabel(s: HostStatus | undefined): string | null {
  if (!s?.phase) return null;
  return { probing: fmt("hosts.phase.probing"), installing: fmt("hosts.phase.installing"), upgrading: fmt("hosts.phase.upgrading") }[s.phase];
}
