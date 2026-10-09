import { useSyncExternalStore } from "react";
import { listen } from "@tauri-apps/api/event";
import { asAgentStatus } from "./agentStatus";
import type { AgentStatus } from "../hosts/types";

// Local Host Tabs' Agent Status, from the `local:agent-status` event (Rust `LocalAgentStatus`).
// An external store read with useSyncExternalStore. Each event is numbered: an event older
// than the last one applied for its Tab is ignored, so a Relaunch's or close's reset is never
// overwritten by a report of the run before it.

export interface LocalAgentStatusEvent {
  id: string;
  status: string | null;
  seq: number;
}

export class LocalAgentStatusStore {
  private snap: ReadonlyMap<string, AgentStatus> = new Map();
  private seqs = new Map<string, number>();
  private listeners = new Set<() => void>();
  private started: Promise<void> | null = null;

  apply(e: LocalAgentStatusEvent) {
    const last = this.seqs.get(e.id);
    if (last !== undefined && e.seq <= last) return;
    this.seqs.set(e.id, e.seq);
    const s = asAgentStatus(e.status);
    if ((this.snap.get(e.id) ?? null) === s) return;
    const next = new Map(this.snap);
    if (s) next.set(e.id, s); else next.delete(e.id);
    this.snap = next;
    for (const l of this.listeners) l();
  }

  // Single-flight: the first subscriber starts listening.
  start(): Promise<void> {
    if (!this.started) {
      this.started = listen<LocalAgentStatusEvent>("local:agent-status", e => this.apply(e.payload)).then(() => {});
    }
    return this.started;
  }

  subscribe = (l: () => void) => {
    void this.start();
    this.listeners.add(l);
    return () => { this.listeners.delete(l); };
  };

  getSnapshot = () => this.snap;
}

export const localAgentStatus = new LocalAgentStatusStore();

export function useLocalAgentStatuses(): ReadonlyMap<string, AgentStatus> {
  return useSyncExternalStore(localAgentStatus.subscribe, localAgentStatus.getSnapshot);
}
