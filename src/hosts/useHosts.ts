import { useSyncExternalStore } from "react";
import { AGENT_IDS, type AgentId } from "../agents";
import { registry, isUsableStatus, type RegistrySnapshot } from "./registry";
import { fmt } from "./strings";
import { LOCAL_HOST } from "./localHost";
import type { AvailableHost, HostConfig, HostId, HostStatus, TerminalInfo } from "./types";

export function useHostsSnapshot(): RegistrySnapshot {
  return useSyncExternalStore(registry.subscribe, registry.getSnapshot);
}

export function useHostConfigs(): HostConfig[] {
  return useHostsSnapshot().configs;
}

export function remoteHosts(snap: RegistrySnapshot): (AvailableHost | HostConfig)[] {
  const available = snap.available ?? [];
  const represented = new Set(available.flatMap(h => [h.id, ...h.aliases]));
  return [...available.filter(h => h.id !== LOCAL_HOST), ...snap.configs.filter(c => !represented.has(c.id))];
}

export function useHostStatus(id: HostId | undefined): HostStatus | undefined {
  const snap = useHostsSnapshot();
  return id ? snap.status[registry.resolveAlias(id)] : undefined;
}

export function useHostLive(id: HostId | undefined): TerminalInfo[] | null | undefined {
  const snap = useHostsSnapshot();
  return id ? snap.live[registry.resolveAlias(id)] : undefined;
}

const NO_AGENTS = Object.fromEntries(AGENT_IDS.map(a => [a, false])) as Record<AgentId, boolean>;

export function useHostAgents(id: HostId | undefined): Record<AgentId, boolean> {
  const snap = useHostsSnapshot();
  return (id && snap.agents[registry.resolveAlias(id)]) || NO_AGENTS;
}

export function usableHosts(snap: RegistrySnapshot = registry.getSnapshot()): HostId[] {
  return remoteHosts(snap).filter(c => isUsableStatus(snap.status[c.id])).map(c => c.id);
}

// "Connected", "Reconnecting · Checking…" … — the chip text used everywhere.
export function statusLabel(s: HostStatus | undefined): string {
  if (!s) return fmt("hosts.status.reconnecting");
  const base = {
    "connected": fmt("hosts.status.connected"),
    "reconnecting": fmt("hosts.status.reconnecting"),
    "offline": fmt("hosts.status.offline"),
    "upgrade-pending": fmt("hosts.status.upgradePending"),
    "build-differs": fmt("hosts.status.buildDiffers"),
    "incompatible": fmt("hosts.status.incompatible"),
  }[s.status];
  return base;
}

export function phaseLabel(s: HostStatus | undefined): string | null {
  if (!s?.phase) return null;
  return { preparing: fmt("hosts.update.preparingPhase"), probing: fmt("hosts.phase.probing"), installing: fmt("hosts.phase.installing"), upgrading: fmt("hosts.phase.upgrading") }[s.phase];
}
