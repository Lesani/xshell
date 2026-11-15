import { sameBuild } from "../buildId";
import { fmt } from "./strings";
import { invoke } from "@tauri-apps/api/core";
import { registry } from "./registry";
import { collectLeafIds } from "../layout";
import { daemonHost, LOCAL_HOST } from "./localHost";
import type { LayoutNode, ProjectInfo } from "../types";
import type { HostConfig, HostStatus } from "./types";

export type UpdateAction = "update" | "replace";

// Semver/build eligibility comes from the backend, including incompatible Daemons. A
// protocol-range direction is not a version ordering. Reconnect retains owner diagnosis.
export function updateAction(status: HostStatus | undefined): UpdateAction | null {
  if (!status || status.updateUnavailable || status.routeUpdateUnavailable || !["connected", "upgrade-pending", "build-differs", "incompatible"].includes(status.status)) return null;
  return status.updateAction ?? null;
}

export function activeUpdateHost(active: string, tabs: readonly { id: string; host?: string; terminal?: string }[], groups: readonly { id: string; layout: LayoutNode }[], focused: Record<string, string>, selectedProject: Pick<ProjectInfo, "host"> | null): string | undefined {
  if (active === "home") return selectedProject ? selectedProject.host ?? LOCAL_HOST : undefined;
  if (active === "settings") return undefined;
  const group = groups.find(g => g.id === active);
  const leaves = group ? collectLeafIds(group.layout) : [];
  const leaf = group ? (leaves.includes(focused[active]) ? focused[active] : leaves[0]) : active;
  const tab = tabs.find(t => t.id === leaf);
  return tab ? tab.host ?? daemonHost(tab) ?? undefined : undefined;
}

export type UpdateTarget = { configGeneration: number; linkGeneration: number; daemonVersion: string | null; daemonBuild: HostStatus["daemonBuild"] };

export function updateTarget(status: HostStatus): UpdateTarget {
  return { configGeneration: status.configGeneration ?? 0, linkGeneration: status.linkGeneration ?? 0, daemonVersion: status.daemonVersion, daemonBuild: status.daemonBuild ? { ...status.daemonBuild } : null };
}

export function exactUpdateConnected(status: HostStatus | undefined): boolean {
  return !!status && status.status === "connected" && status.phase === null && !status.lastError && status.errorHint !== "update-failed"
    && status.daemonVersion === status.desktopVersion && sameBuild(status.daemonBuild, status.desktopBuild);
}

type Confirmation = { host: Pick<HostConfig, "id" | "name">; action: UpdateAction; target: UpdateTarget; expectedVersion: string; expectedBuild: HostStatus["desktopBuild"] };
type Invocation = { confirmation: Confirmation; succeeded: boolean };

export type UpdateHost = Pick<HostConfig, "id" | "name">;
/// Why "Update all" leaves a machine out.
export type SkipReason = "current" | "offline" | "unsupported" | "busy";
export type AllResult =
  | { host: UpdateHost; outcome: "ok"; version: string }
  | { host: UpdateHost; outcome: "failed"; error: string }
  | { host: UpdateHost; outcome: "skipped"; reason: SkipReason };
/// "Update all": the machines it will update (each with the target it was confirmed for)
/// and the ones it leaves out, then one result per machine as they finish.
export type AllRun = {
  queue: Confirmation[];
  skipped: AllResult[];
  confirmed: boolean;
  current: UpdateHost | null;
  results: AllResult[];
  done: boolean;
};
type Snapshot = { confirmation: Confirmation | null; invoking: Record<string, boolean>; errors: Record<string, string>; all: AllRun | null };

/// Why `status` cannot be updated now; `null` when it can.
export function skipReason(status: HostStatus | undefined): SkipReason | null {
  if (!status) return "offline";
  if (status.routeUpdateUnavailable === "ring-update-unsupported" || status.updateUnavailable) return "unsupported";
  if (status.phase === "upgrading" || status.phase === "preparing" || status.phase === "installing") return "busy";
  if (!["connected", "upgrade-pending", "build-differs", "incompatible"].includes(status.status)) return "offline";
  return updateAction(status) ? null : "current";
}

/// How long "Update all" waits for one machine to come back with this Desktop's build.
export const SETTLE_MS = 150_000;

// Only confirmation/invocation state is shared here. Completion and progress remain the
// existing Host Status authority. This store outlives navigation between its two surfaces.
export class HostUpdates {
  private state: Snapshot = { confirmation: null, invoking: {}, errors: {}, all: null };
  private listeners = new Set<() => void>();
  private inFlight = new Map<string, Invocation>();
  constructor(
    private call: typeof invoke = invoke,
    private statusOf: (id: string) => HostStatus | undefined = id => registry.getSnapshot().status[id],
    private watch: (listener: () => void) => () => void = l => registry.subscribe(l),
    private settleMs: number = SETTLE_MS,
  ) {}
  getSnapshot = () => this.state;
  subscribe = (listener: () => void) => { this.listeners.add(listener); return () => { this.listeners.delete(listener); }; };
  private set(patch: Partial<Snapshot>) { this.state = { ...this.state, ...patch }; for (const listener of this.listeners) listener(); }
  ask(host: Pick<HostConfig, "id" | "name">, status: HostStatus) {
    const action = updateAction(status);
    if (status.host !== host.id || !action || (status.phase === "upgrading" || status.phase === "preparing") || this.state.confirmation || this.state.invoking[host.id]) return;
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

  /// "Update all": the eligible machines among `hosts`, one after another, once confirmed.
  askAll(hosts: { host: UpdateHost; status: HostStatus | undefined }[]) {
    if (this.state.confirmation || (this.state.all && !this.state.all.done)) return;
    const queue: Confirmation[] = [];
    const skipped: AllResult[] = [];
    for (const { host, status } of hosts) {
      const reason = status?.host === host.id ? skipReason(status) : "offline";
      const action = status && updateAction(status);
      if (reason || !status || !action || this.state.invoking[host.id]) {
        skipped.push({ host: { ...host }, outcome: "skipped", reason: reason ?? "busy" });
        continue;
      }
      queue.push({ host: { ...host }, action, target: updateTarget(status), expectedVersion: status.desktopVersion, expectedBuild: status.desktopBuild ? { ...status.desktopBuild } : null });
    }
    if (!queue.length) return;
    this.set({ all: { queue, skipped, confirmed: false, current: null, results: [], done: false } });
  }

  cancelAll = () => { if (this.state.all && !this.state.all.confirmed) this.set({ all: null }); };
  dismissAll = () => { if (this.state.all?.done) this.set({ all: null }); };

  confirmAll = async () => {
    const run = this.state.all;
    if (!run || run.confirmed) return;
    this.set({ all: { ...run, confirmed: true } });
    for (const c of run.queue) {
      const all = this.state.all!;
      this.set({ all: { ...all, current: c.host } });
      const result = await this.updateOne(c);
      const after = this.state.all!;
      this.set({ all: { ...after, current: null, results: [...after.results, result] } });
    }
    const end = this.state.all!;
    this.set({ all: { ...end, done: true } });
  };

  /// One machine of "Update all": the confirmed update, then wait until it is back with
  /// this Desktop's build, its update failed, or `settleMs` passed.
  private async updateOne(c: Confirmation): Promise<AllResult> {
    const id = c.host.id;
    const current = this.statusOf(id);
    if (!current || skipReason(current) || (current.linkGeneration ?? 0) !== c.target.linkGeneration) {
      return { host: c.host, outcome: "skipped", reason: skipReason(current) ?? "offline" };
    }
    const settled = this.waitSettled(c);
    this.set({ invoking: { ...this.state.invoking, [id]: true } });
    try {
      await this.call("host_upgrade", { host: id, target: c.target });
      settled.started();
    } catch (e) {
      settled.cancel();
      const error = typeof e === "string" ? e : (e as { message?: string })?.message ?? String(e);
      return { host: c.host, outcome: "failed", error };
    } finally {
      this.set({ invoking: { ...this.state.invoking, [id]: false } });
    }
    const r = await settled.promise;
    return r === true ? { host: c.host, outcome: "ok", version: c.expectedVersion } : { host: c.host, outcome: "failed", error: r };
  }

  private waitSettled(c: Confirmation): { promise: Promise<true | string>; cancel: () => void; started: () => void } {
    let finish!: (r: true | string) => void;
    const promise = new Promise<true | string>(resolve => { finish = resolve; });
    let over = false;
    // An update error counts only once this update began: an older one may still show.
    let began = false;
    const end = (r: true | string) => { if (over) return; over = true; stop(); clearTimeout(timer); finish(r); };
    const check = () => {
      const s = this.statusOf(c.host.id);
      if (!s) return;
      if (s.phase !== null) began = true;
      if (exactUpdateConnected(s) && (s.linkGeneration ?? 0) > c.target.linkGeneration && s.daemonVersion === c.expectedVersion && sameBuild(s.daemonBuild, c.expectedBuild)) end(true);
      else if (began && s.errorHint === "update-failed" && s.phase === null && s.lastError) end(s.lastError);
    };
    const stop = this.watch(check);
    // COPY-GAP: no approved text for "did not come back in time" from the UI's own wait;
    // the backend's error (when it set one) wins, else the nearest approved string.
    const timer = setTimeout(() => end(this.statusOf(c.host.id)?.lastError ?? fmt("update.offline", { name: c.host.name })), this.settleMs);
    return {
      promise,
      cancel: () => { if (!over) { over = true; stop(); clearTimeout(timer); } },
      started: () => { began = true; check(); },
    };
  }
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
