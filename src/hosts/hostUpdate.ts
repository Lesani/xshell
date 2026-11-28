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

// Healthy: connected with nothing offered. A Host that follows releases may run another
// version than this Desktop and still be current, so the versions are not compared here.
export function exactUpdateConnected(status: HostStatus | undefined): boolean {
  return !!status && status.status === "connected" && status.phase === null && !status.lastError && status.errorHint !== "update-failed" && !status.updateAction;
}

export type UpdateSource = "release" | "desktop";

/// Who supplies the offered update. An older backend offers only this Desktop's build.
export function updateSource(status: HostStatus | undefined): UpdateSource | null {
  return status?.updateSource ?? (status?.updateAction ? "desktop" : null);
}

// `sent`: what a release update may bring back instead, when the Host cannot get the release
// itself and is sent this Desktop's build: this Desktop's version and exact build. Only for
// a Host that ran another version when the update was confirmed.
type Expectation = { source: UpdateSource; expectedVersion: string; expectedBuild: HostStatus["desktopBuild"]; sent: { version: string; build: NonNullable<HostStatus["desktopBuild"]> } | null };
// What a confirmed update must bring back. A release is checked by version only: its build
// is whatever the release holds, not this Desktop's.
function expectation(status: HostStatus): Expectation {
  const source = updateSource(status) ?? "desktop";
  const sent = source === "release" && status.desktopBuild && status.daemonVersion !== status.desktopVersion ? { version: status.desktopVersion, build: { ...status.desktopBuild } } : null;
  return { source, expectedVersion: status.updateVersion ?? status.desktopVersion, expectedBuild: source === "desktop" && status.desktopBuild ? { ...status.desktopBuild } : null, sent };
}

type Confirmation = { host: Pick<HostConfig, "id" | "name">; action: UpdateAction; target: UpdateTarget } & Expectation;

// This operation succeeded: usable, settled, on the expected version over a new link. A
// further update may already be offered (a newer release learned meanwhile): still success.
// A Host that could not get the release itself was sent this Desktop's build instead, and
// is then on this Desktop's version.
function arrived(s: HostStatus, c: Confirmation): boolean {
  return ["connected", "upgrade-pending", "build-differs"].includes(s.status) && s.phase === null && !s.lastError && s.errorHint !== "update-failed"
    && (s.linkGeneration ?? 0) > c.target.linkGeneration
    && ((s.daemonVersion === c.expectedVersion && (c.source !== "desktop" || sameBuild(s.daemonBuild, c.expectedBuild)))
      || (!!c.sent && s.daemonVersion === c.sent.version && sameBuild(s.daemonBuild, c.sent.build)));
}

// The Host answered that it is already current: healthy, nothing offered, no restart.
function alreadyCurrent(s: HostStatus, c: Confirmation): boolean {
  return exactUpdateConnected(s) && s.daemonVersion !== c.expectedVersion && (s.linkGeneration ?? 0) <= c.target.linkGeneration;
}

type Invocation = { confirmation: Confirmation; succeeded: boolean };

export type UpdateHost = Pick<HostConfig, "id" | "name">;
/// Why "Update all" leaves a machine out.
/// `desktop`: the machine has the xshell app, which is updated on that machine itself.
export type SkipReason = "current" | "offline" | "unsupported" | "desktop" | "busy";
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
// `sources`: the source of each confirmed update, by Host id, while it runs; the status
// stops offering it (and naming its source) once the Host starts.
type Snapshot = { confirmation: Confirmation | null; invoking: Record<string, boolean>; errors: Record<string, string>; all: AllRun | null; sources: Record<string, UpdateSource> };

/// Why `status` cannot be updated now; `null` when it can.
export function skipReason(status: HostStatus | undefined): SkipReason | null {
  if (!status) return "offline";
  if (status.updateUnavailable === "desktop-owned") return "desktop";
  if (status.routeUpdateUnavailable === "ring-update-unsupported" || status.updateUnavailable) return "unsupported";
  if (status.phase === "upgrading" || status.phase === "preparing" || status.phase === "installing") return "busy";
  if (!["connected", "upgrade-pending", "build-differs", "incompatible"].includes(status.status)) return "offline";
  return updateAction(status) ? null : "current";
}

/// An update of this machine runs: this Desktop is asking for it, or the backend reports a
/// phase of it.
export function updateRunning(status: HostStatus | undefined, invoking: boolean | undefined): boolean {
  return !exactUpdateConnected(status) && (!!invoking || status?.phase === "upgrading" || status?.phase === "preparing" || status?.phase === "installing");
}

/// How long "Update all" waits, after the request answered, for one machine to come back
/// with the expected version.
export const SETTLE_MS = 150_000;

// Only confirmation/invocation state is shared here. Completion and progress remain the
// existing Host Status authority. This store outlives navigation between its two surfaces.
export class HostUpdates {
  private state: Snapshot = { confirmation: null, invoking: {}, errors: {}, all: null, sources: {} };
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
    this.set({ confirmation: { host: { ...host }, action, target: updateTarget(status), ...expectation(status) } });
  }
  reconcile(status: HostStatus | undefined) {
    if (!status) return;
    const invocation = this.inFlight.get(status.host);
    if (invocation && (status.configGeneration ?? 0) === invocation.confirmation.target.configGeneration && arrived(status, invocation.confirmation)) {
      // Keep this evidence until the invocation settles, even across offline/navigation.
      invocation.succeeded = true;
    }
    if (!exactUpdateConnected(status)) return;
    if (this.state.errors[status.host]) this.set({ errors: { ...this.state.errors, [status.host]: "" } });
    if (this.state.sources[status.host] && !this.state.invoking[status.host]) {
      const sources = { ...this.state.sources };
      delete sources[status.host];
      this.set({ sources });
    }
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
      queue.push({ host: { ...host }, action, target: updateTarget(status), ...expectation(status) });
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

  /// One machine of "Update all": the confirmed update, then wait until it is back on the
  /// expected version, says it is already current, its update failed, or `settleMs` passed.
  private async updateOne(c: Confirmation): Promise<AllResult> {
    const id = c.host.id;
    const current = this.statusOf(id);
    if (!current || skipReason(current) || (current.linkGeneration ?? 0) !== c.target.linkGeneration) {
      return { host: c.host, outcome: "skipped", reason: skipReason(current) ?? "offline" };
    }
    const settled = this.waitSettled(c);
    this.set({ invoking: { ...this.state.invoking, [id]: true }, sources: { ...this.state.sources, [id]: c.source } });
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
    return typeof r === "object" ? { host: c.host, outcome: "ok", version: r.version }
      : r === false ? { host: c.host, outcome: "skipped", reason: "current" }
      : { host: c.host, outcome: "failed", error: r };
  }

  // Settles with the version it arrived on, `false` (the Host said it is already current) or
  // an error.
  private waitSettled(c: Confirmation): { promise: Promise<{ version: string } | false | string>; cancel: () => void; started: () => void } {
    let finish!: (r: { version: string } | false | string) => void;
    const promise = new Promise<{ version: string } | false | string>(resolve => { finish = resolve; });
    let over = false;
    // An update error counts only once this update began: an older one may still show.
    let began = false;
    // Already current is believed only after `host_upgrade` resolved: before, the Host may
    // not have looked yet.
    let answered = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const end = (r: { version: string } | false | string) => { if (over) return; over = true; stop(); clearTimeout(timer); finish(r); };
    const check = () => {
      const s = this.statusOf(c.host.id);
      if (!s) return;
      if (s.phase !== null) began = true;
      if (arrived(s, c)) end({ version: s.daemonVersion ?? c.expectedVersion });
      else if (answered && alreadyCurrent(s, c)) end(false);
      else if (began && s.errorHint === "update-failed" && s.phase === null && s.lastError) end(s.lastError);
    };
    const stop = this.watch(check);
    return {
      promise,
      cancel: () => { if (!over) { over = true; stop(); clearTimeout(timer); } },
      // The deadline runs from the answer: until then the request itself is bounded by the
      // backend (a Host fetching a release may take most of it before it restarts).
      started: () => {
        began = true; answered = true; check();
        // The backend's error (when it set one) wins.
        if (!over) timer = setTimeout(() => end(this.statusOf(c.host.id)?.lastError ?? fmt("update.notBack", { name: c.host.name })), this.settleMs);
      },
    };
  }
  confirm = async () => {
    const pending = this.state.confirmation;
    if (!pending || this.state.invoking[pending.host.id]) return;
    const id = pending.host.id;
    const invocation: Invocation = { confirmation: pending, succeeded: false };
    this.inFlight.set(id, invocation);
    this.set({ confirmation: null, invoking: { ...this.state.invoking, [id]: true }, errors: { ...this.state.errors, [id]: "" }, sources: { ...this.state.sources, [id]: pending.source } });
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
