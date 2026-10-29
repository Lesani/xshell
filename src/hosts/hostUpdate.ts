import { sameBuild } from "../buildId";
import { invoke } from "@tauri-apps/api/core";
import { registry } from "./registry";
import { collectLeafIds } from "../layout";
import type { LayoutNode } from "../types";
import type { HostConfig, HostStatus } from "./types";

export type UpdateAction = "update" | "replace";

// Semver/build eligibility comes from the backend, including incompatible Daemons. A
// protocol-range direction is not a version ordering. Reconnect retains owner diagnosis.
export function updateAction(status: HostStatus | undefined): UpdateAction | null {
  if (!status || status.updateUnavailable || !["connected", "upgrade-pending", "build-differs", "incompatible"].includes(status.status)) return null;
  return status.updateAction ?? null;
}

export function activeUpdateHost(active: string, tabs: readonly { id: string; host?: string }[], groups: readonly { id: string; layout: LayoutNode }[], focused: Record<string, string>): string | undefined {
  if (active === "home" || active === "settings") return undefined;
  const group = groups.find(g => g.id === active);
  const leaves = group ? collectLeafIds(group.layout) : [];
  const leaf = group ? (leaves.includes(focused[active]) ? focused[active] : leaves[0]) : active;
  return tabs.find(t => t.id === leaf)?.host;
}

export type UpdateTarget = { configGeneration: number; linkGeneration: number; daemonVersion: string | null; daemonBuild: HostStatus["daemonBuild"] };

export function updateTarget(status: HostStatus): UpdateTarget {
  return { configGeneration: status.configGeneration ?? 0, linkGeneration: status.linkGeneration ?? 0, daemonVersion: status.daemonVersion, daemonBuild: status.daemonBuild ? { ...status.daemonBuild } : null };
}

export function exactUpdateConnected(status: HostStatus | undefined): boolean {
  return !!status && status.status === "connected" && status.phase === null && !status.lastError && status.errorHint !== "update-failed"
    && status.daemonVersion === status.desktopVersion && sameBuild(status.daemonBuild, status.desktopBuild);
}

type Confirmation = { host: HostConfig; action: UpdateAction; target: UpdateTarget; expectedVersion: string; expectedBuild: HostStatus["desktopBuild"] };
type Invocation = { confirmation: Confirmation; succeeded: boolean };
type Snapshot = { confirmation: Confirmation | null; invoking: Record<string, boolean>; errors: Record<string, string> };

// Only confirmation/invocation state is shared here. Completion and progress remain the
// existing Host Status authority. This store outlives navigation between its two surfaces.
export class HostUpdates {
  private state: Snapshot = { confirmation: null, invoking: {}, errors: {} };
  private listeners = new Set<() => void>();
  private inFlight = new Map<string, Invocation>();
  constructor(private call: typeof invoke = invoke) {}
  getSnapshot = () => this.state;
  subscribe = (listener: () => void) => { this.listeners.add(listener); return () => { this.listeners.delete(listener); }; };
  private set(patch: Partial<Snapshot>) { this.state = { ...this.state, ...patch }; for (const listener of this.listeners) listener(); }
  ask(host: HostConfig, status: HostStatus) {
    const action = updateAction(status);
    if (!action || (status.phase === "upgrading" || status.phase === "preparing") || this.state.confirmation || this.state.invoking[host.id]) return;
    this.set({ confirmation: { host: { ...host }, action, target: updateTarget(status), expectedVersion: status.desktopVersion, expectedBuild: status.desktopBuild ? { ...status.desktopBuild } : null } });
  }
  reconcile(status: HostStatus | undefined) {
    if (!status || !exactUpdateConnected(status)) return;
    const invocation = this.inFlight.get(status.host);
    if (invocation) {
      const { target, expectedVersion, expectedBuild } = invocation.confirmation;
      if ((status.configGeneration ?? 0) === target.configGeneration && (status.linkGeneration ?? 0) > target.linkGeneration
        && status.daemonVersion === expectedVersion && sameBuild(status.daemonBuild, expectedBuild)) {
        // Keep this evidence until the invocation settles, even across offline/navigation.
        invocation.succeeded = true;
      }
    }
    if (this.state.errors[status.host]) this.set({ errors: { ...this.state.errors, [status.host]: "" } });
  }
  cancel = () => this.set({ confirmation: null });
  confirm = async () => {
    const pending = this.state.confirmation;
    if (!pending || this.state.invoking[pending.host.id]) return;
    const id = pending.host.id;
    const invocation: Invocation = { confirmation: pending, succeeded: false };
    this.inFlight.set(id, invocation);
    this.set({ confirmation: null, invoking: { ...this.state.invoking, [id]: true }, errors: { ...this.state.errors, [id]: "" } });
    try { await this.call("host_upgrade", { host: id, target: pending.target }); }
    catch (e) {
      if (!invocation.succeeded) this.set({ errors: { ...this.state.errors, [id]: typeof e === "string" ? e : (e as { message?: string })?.message ?? String(e) } });
    }
    finally {
      this.inFlight.delete(id);
      this.set({ invoking: { ...this.state.invoking, [id]: false } });
    }
  };
}

export const hostUpdates = new HostUpdates();

// Window-lifetime observation, independent of whether any Host action is mounted. This
// subscribes to the existing registry; it does not start listeners, polling or a transaction.
registry.subscribe(() => {
  for (const status of Object.values(registry.getSnapshot().status)) hostUpdates.reconcile(status);
});
