import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { AGENT_IDS, AGENTS, type AgentId } from "../agents";
import { hostInvoke } from "./hostInvoke";
import { cache } from "./cache";
import { LOCAL_HOST } from "./localHost";
import type { AvailableHost, AvailableSnapshot, HostConfig, HostId, HostSnapshot, HostStatus, HostTerminalsEvent, TerminalInfo } from "./types";

// The Desktop's view of its Remote Hosts: configs, Host Status, the live `terminals` list per
// Host and the agent CLIs detected on each. An external store read with useSyncExternalStore.
//
// Discovery starts even without saved SSH Hosts or a Local Daemon. Configs remain saved-only.

export interface RegistrySnapshot {
  configs: HostConfig[];
  available: AvailableHost[];
  status: Record<HostId, HostStatus>;
  live: Record<HostId, TerminalInfo[] | null>; // null = never connected this run
  agents: Record<HostId, Record<AgentId, boolean>>;
  // Bumped each time a Host becomes usable or its connection is replaced: views showing data
  // read from that Host refetch when it changes.
  epochs: Record<HostId, number>;
}

export function resolveHostAlias(snap: RegistrySnapshot, id: HostId): HostId {
  return snap.available.find(h => h.id === id || h.aliases.includes(id))?.id ?? id;
}

export const isUsableStatus = (s: HostStatus | undefined): boolean =>
  !!s && (s.status === "connected" || s.status === "upgrade-pending" || s.status === "build-differs");

export class HostUnknownError extends Error {
  readonly code = "unknown-host";
  constructor(host: HostId) { super(`unknown host ${host}`); this.name = "HostUnknownError"; }
}

type Waiter = { host: HostId; resolve: () => void; reject: (e: unknown) => void; signal?: AbortSignal; onAbort?: () => void; after?: number };

const WAKE_POLL_MS = 5000;
const WAKE_GAP_MS = 30000;

export class HostRegistry {
  private snap: RegistrySnapshot = { configs: [], available: [], status: {}, live: {}, agents: {}, epochs: {} };
  private listeners = new Set<() => void>();
  // Amendment 21: per-host sequence numbers, one per field. A status event bumps only the
  // status number and a terminals event only the terminals number; a `hosts_status` snapshot
  // applies each field only where no newer event of that kind arrived.
  private statusSeq: Record<HostId, number> = {};
  private sequence = 0;
  private aliasSeq: Record<HostId, number> = {};
  private liveSeq: Record<HostId, number> = {};
  // Agent inventory probes are fenced by a per-host generation, bumped when the connection
  // is replaced (another machine may now answer).
  private probeGen: Record<HostId, number> = {};
  private initPromise: Promise<void> | null = null;
  private unlisten: UnlistenFn[] = [];
  private waiters: Waiter[] = [];
  private usableListeners = new Set<(host: HostId) => void>();
  private reattachListeners = new Set<(host: HostId) => void>();
  private agentsProbed = new Set<HostId>();
  private teardown: (() => void) | null = null;
  private local = false;
  private retargeting = new Set<HostId>();
  private retargetWaiters = new Set<() => void>();
  beginRetarget(host: HostId) { this.retargeting.add(host); }
  endRetarget(host: HostId) {
    this.retargeting.delete(host);
    for (const wake of [...this.retargetWaiters]) wake();
  }
  isRetargeting(host: HostId): boolean { return this.retargeting.has(host) || this.retargeting.has(this.resolveAlias(host)); }
  waitRetarget(host: HostId, signal?: AbortSignal): Promise<void> {
    if (signal?.aborted) return Promise.reject(abortError());
    const guarded = this.retargeting.has(host) ? host : this.resolveAlias(host);
    if (!this.retargeting.has(guarded)) return Promise.resolve();
    return new Promise<void>((resolve, reject) => {
      const cleanup = () => { this.retargetWaiters.delete(wake); signal?.removeEventListener("abort", onAbort); };
      const wake = () => {
        if (this.retargeting.has(guarded) || this.isRetargeting(host)) return;
        cleanup(); resolve();
      };
      const onAbort = () => { cleanup(); reject(abortError()); };
      this.retargetWaiters.add(wake);
      signal?.addEventListener("abort", onAbort, { once: true });
    });
  }
  private availableSequence = -1;
  private pendingStatus = new Map<HostId, HostStatus>();
  private pendingLive = new Map<HostId, TerminalInfo[]>();
  private minimumGeneration: Record<HostId, number> = {};
  private awaitingRetarget = new Set<HostId>();
  private withdrawn = new Set<HostId>();

  resolveAlias(id: HostId): HostId {
    return resolveHostAlias(this.snap, id);
  }
  dataHost(id: HostId | undefined): HostId | undefined {
    const resolved = id ? this.resolveAlias(id) : undefined;
    return resolved === LOCAL_HOST ? undefined : resolved;
  }
  availableHost(id: HostId): AvailableHost | undefined {
    return this.snap.available.find(h => h.id === id || h.aliases.includes(id));
  }
  private bindCache() {
    cache.bindHosts(this.snap.available, this.snap.configs, this.snap.status);
  }
  applyAvailable(snapshot: AvailableSnapshot) {
    if (snapshot.sequence <= this.availableSequence) return;
    this.availableSequence = snapshot.sequence;
    const previous = this.snap.available;
    for (const row of snapshot.hosts) for (const alias of row.aliases) {
      const old = previous.find(h => h.id === alias || h.aliases.includes(alias));
      if (!old || old.id !== row.id || old.daemonKey !== row.daemonKey || old.activeRoute !== row.activeRoute) this.aliasSeq[alias] = ++this.sequence;
    }
    const status = { ...this.snap.status };
    const live = { ...this.snap.live };
    for (const old of previous) {
      const next = snapshot.hosts.find(h => h.id === old.id);
      if (!next || (old.daemonKey !== null && next.daemonKey !== old.daemonKey) || next.activeRoute !== old.activeRoute) {
        if (next && status[old.id]) status[old.id] = { ...status[old.id], status: "reconnecting" };
        else delete status[old.id];
        delete live[old.id];
        this.invalidateAgents(old.id);
        if (!next && !snapshot.hosts.some(h => h.aliases.includes(old.id))) this.withdrawn.add(old.id);
      }
    }
    this.emit({ available: snapshot.hosts, status, live });
    for (const old of previous) if (!this.isAvailable(old.id)) this.rejectWaiters(old.id);
    for (const h of snapshot.hosts) {
      this.withdrawn.delete(h.id);
      const pending = this.pendingStatus.get(h.id);
      const list = this.pendingLive.get(h.id);
      this.pendingStatus.delete(h.id); this.pendingLive.delete(h.id);
      if (pending) this.applyStatus(pending);
      if (list) this.applyTerminals(h.id, list);
      if (this.isUsable(h.id)) this.flushWaiters(h.id);
    }
    this.pendingStatus.clear(); this.pendingLive.clear();
  }
  private async refreshStatus() {
    const before = { status: { ...this.statusSeq }, live: { ...this.liveSeq }, available: this.availableSequence };
    try { this.applySnapshot(await invoke<HostSnapshot[]>("hosts_status"), before); } catch (_) {}
  }

  // ── store ──
  subscribe = (l: () => void) => { this.listeners.add(l); return () => { this.listeners.delete(l); }; };
  getSnapshot = () => this.snap;
  private emit(next: Partial<RegistrySnapshot>) {
    this.snap = { ...this.snap, ...next };
    this.bindCache(); // Subscribers must observe provenance for this same snapshot.
    for (const l of this.listeners) l();
  }

  get initialized(): boolean { return this.initPromise !== null; }

  // Single-flight (StrictMode-safe), including zero-SSH startup.
  init(hosts: HostConfig[]): Promise<void> {
    if (this.initPromise) return this.initPromise;
    this.emit({ configs: hosts });

    this.initPromise = this.start();
    return this.initPromise;
  }

  // Local Daemon mode: track the "local" Host too, starting the listeners even with no Remote
  // Hosts. Call after `init`. When the registry already runs, its "local" events were ignored
  // until now, so a fresh `hosts_status` fills them in.
  setLocal(enabled: boolean): Promise<void> {
    this.local = enabled;
    if (!enabled) return this.initPromise ?? Promise.resolve();
    if (!this.initPromise) {
      this.initPromise = this.start();
      return this.initPromise;
    }
    return this.initPromise.then(async () => {
      const seqBefore = { status: { ...this.statusSeq }, live: { ...this.liveSeq }, available: this.availableSequence };
      let snaps: HostSnapshot[] = [];
      try { snaps = await invoke<HostSnapshot[]>("hosts_status"); } catch (_) {}
      this.applySnapshot(snaps.filter(x => x.status.host === LOCAL_HOST), seqBefore);
    });
  }

  get localEnabled(): boolean { return this.local; }

  private async start() {
    // Listen before fetching either snapshot. Status can precede discovery; buffer it.
    this.unlisten.push(await listen<AvailableSnapshot>("hosts:available", e => {
      this.applyAvailable(e.payload);
      void this.refreshStatus();
    }));
    this.unlisten.push(await listen<HostStatus>("hosts:status", e => this.applyStatus(e.payload)));
    this.unlisten.push(await listen<HostTerminalsEvent>("hosts:terminals", e => this.applyTerminals(e.payload.host, e.payload.list)));
    const seqBefore = { status: { ...this.statusSeq }, live: { ...this.liveSeq }, available: this.availableSequence };
    try { await invoke("hosts_configure", { hosts: this.snap.configs }); } catch (_) {}
    try {
      const available = await invoke<AvailableSnapshot>("hosts_available");
      if (available) this.applyAvailable(available);
    } catch (_) {}
    seqBefore.available = this.availableSequence;
    let snaps: HostSnapshot[] = [];
    try { snaps = await invoke<HostSnapshot[]>("hosts_status"); } catch (_) {}
    this.applySnapshot(snaps, seqBefore);
    this.installWakeHooks();
  }

  applySnapshot(snaps: HostSnapshot[], seqBefore: { status: Record<HostId, number>; live: Record<HostId, number>; available?: number }) {
    if (seqBefore.available !== undefined && seqBefore.available !== this.availableSequence) return;
    const status = { ...this.snap.status };
    const live = { ...this.snap.live };
    const prevStatus = this.snap.status;
    let changed = false;
    for (const s of snaps) {
      const h = s.status.host;
      if (!this.isAvailable(h) || this.resolveAlias(h) !== h || (s.status.configGeneration ?? 0) < (this.minimumGeneration[h] ?? 0)) continue;
      // Each field independently: skip it only if a newer event of that kind was applied.
      if ((this.statusSeq[h] ?? 0) === (seqBefore.status[h] ?? 0)) { status[h] = s.status; this.awaitingRetarget.delete(h); changed = true; }
      if ((this.liveSeq[h] ?? 0) === (seqBefore.live[h] ?? 0) && (s.terminals !== null || !(h in live))) { live[h] = s.terminals; changed = true; }
    }
    if (!changed) return;
    this.emit({ status, live });
    for (const s of snaps) this.afterStatus(prevStatus[s.status.host], status[s.status.host]);
  }

  applyStatus(s: HostStatus) {
    const h = s.host;
    if ((s.configGeneration ?? 0) < (this.minimumGeneration[h] ?? 0)) return;
    if (!this.isAvailable(h)) {
      if (this.withdrawn.has(h)) return;
      if (this.pendingStatus.size < 256) this.pendingStatus.set(h, s);
      return;
    }
    if (this.resolveAlias(h) !== h) return; // A stopped saved alias is not the selected Link.
    const previous = this.snap.status[h];
    if ((s.configGeneration ?? 0) < (previous?.configGeneration ?? 0)) return;
    this.awaitingRetarget.delete(h);
    this.statusSeq[h] = ++this.sequence;
    const prev = this.snap.status[h];
    this.emit({ status: { ...this.snap.status, [h]: s } });
    this.afterStatus(prev, s);
  }

  applyTerminals(host: HostId, list: TerminalInfo[]) {
    if (!this.isAvailable(host)) {
      if (this.withdrawn.has(host)) return;
      if (this.pendingLive.size < 256) this.pendingLive.set(host, list);
      return;
    }
    if (this.resolveAlias(host) !== host || this.awaitingRetarget.has(host)) return;
    this.liveSeq[host] = (this.liveSeq[host] ?? 0) + 1;
    this.emit({ live: { ...this.snap.live, [host]: list } });
  }

  private afterStatus(prev: HostStatus | undefined, next: HostStatus | undefined) {
    if (!next) return;
    const h = next.host;
    const nowUsable = isUsableStatus(next);
    const replaced = !!prev && prev.configGeneration !== undefined && next.configGeneration !== undefined && prev.configGeneration !== next.configGeneration;
    // A replaced connection may reach another machine: forget its agent inventory.
    if (replaced) this.invalidateAgents(h);
    if (nowUsable) this.flushWaiters(h);
    if ((nowUsable && !isUsableStatus(prev)) || replaced) {
      this.emit({ epochs: { ...this.snap.epochs, [h]: (this.snap.epochs[h] ?? 0) + 1 } });
    }
    if (nowUsable && !isUsableStatus(prev)) {
      for (const l of this.usableListeners) l(h);
    }
    // The Local Host's agents are the Desktop's own probe (Settings), not the Daemon's.
    if (nowUsable && h !== LOCAL_HOST && !this.agentsProbed.has(h)) { this.agentsProbed.add(h); this.probeAgents(h); }
    // Amendment 20: a replaced connection (configGeneration changed) → mounted Tabs re-attach.
    if (replaced) {
      for (const l of this.reattachListeners) l(h);
    }
  }

  private invalidateAgents(host: HostId) {
    this.probeGen[host] = (this.probeGen[host] ?? 0) + 1;
    this.agentsProbed.delete(host);
    if (host in this.snap.agents) {
      const agents = { ...this.snap.agents };
      delete agents[host];
      this.emit({ agents });
    }
  }

  private probeAgents(host: HostId) {
    const gen = this.probeGen[host] ?? 0;
    const current = () => (this.probeGen[host] ?? 0) === gen && this.isAvailable(host);
    for (const id of AGENT_IDS) {
      hostInvoke<{ installed: boolean }>(host, "detect_agent_binary", { binary: AGENTS[id].binary })
        .then(p => {
          if (!current()) return; // answer from a replaced connection
          const cur = this.snap.agents[host] ?? (Object.fromEntries(AGENT_IDS.map(a => [a, false])) as Record<AgentId, boolean>);
          this.emit({ agents: { ...this.snap.agents, [host]: { ...cur, [id]: !!p.installed } } });
        })
        .catch(() => { if (current()) this.agentsProbed.delete(host); });
    }
  }

  // ── configuration ──
  async configure(list: HostConfig[]) {
    const removed = this.snap.configs.filter(c => !list.some(n => n.id === c.id)).map(c => c.id);
    const status = { ...this.snap.status };
    const live = { ...this.snap.live };
    const agents = { ...this.snap.agents };
    // A Host whose target or daemon command changed is a new connection: its agent
    // inventory belongs to the old one.
    const retargeted = list.filter(n => { const o = this.config(n.id); return !!o && (o.sshTarget !== n.sshTarget || (o.daemonCommand ?? "") !== (n.daemonCommand ?? "")); }).map(c => c.id);
    for (const id of [...removed, ...retargeted]) {
      delete agents[id];
      if (retargeted.includes(id)) {
        this.minimumGeneration[id] = (status[id]?.configGeneration ?? 0) + 1;
        this.awaitingRetarget.add(id);
        this.pendingStatus.delete(id); this.pendingLive.delete(id);
        delete status[id]; delete live[id];
      }
      this.agentsProbed.delete(id);
      this.probeGen[id] = (this.probeGen[id] ?? 0) + 1;
    }
    for (const id of removed) {
      delete status[id]; delete live[id];
      this.rejectWaiters(id);
    }
    // Withdraw old alias associations synchronously on retarget/removal. Only the backend
    // may restore a verified association; configure never clears unrelated discovery.
    const invalid = new Set([...removed, ...retargeted]);
    const available = this.snap.available.filter(h => !h.aliases.some(a => invalid.has(a)));
    this.emit({ configs: list, available, status, live, agents });
    if (!this.initPromise) {
      await this.init(list);
      return;
    }
    await this.initPromise;
    await invoke("hosts_configure", { hosts: list });
  }

  isConfigured(id: HostId): boolean { return this.snap.configs.some(c => c.id === id); }
  isAvailable(id: HostId): boolean {
    return !!this.availableHost(id) || (id === LOCAL_HOST ? this.local : this.isConfigured(id));
  }
  config(id: HostId): HostConfig | undefined { return this.snap.configs.find(c => c.id === id); }
  hostName(id: HostId | undefined): string { return (id && (this.availableHost(id)?.name || this.config(id)?.name)) || id || ""; }
  getStatus(id: HostId): HostStatus | undefined { return this.snap.status[this.resolveAlias(id)]; }
  isUsable(id: HostId): boolean { return this.isAvailable(id) && isUsableStatus(this.getStatus(id)); }

  // Bumped by every status event of the Host (generation for waitUsable's `after`).
  statusVersion(id: HostId): number { return Math.max(this.statusSeq[this.resolveAlias(id)] ?? 0, this.aliasSeq[id] ?? 0); }

  // Resolves once the Host is usable; rejects when the signal aborts or the Host is removed.
  // With `after`, a status from that version or older does not count: the caller saw the
  // connection fail and waits for a newer usable status.
  waitUsable(id: HostId, signal?: AbortSignal, after?: number): Promise<void> {
    if (signal?.aborted) return Promise.reject(abortError());
    if (!this.isAvailable(id)) return Promise.reject(new HostUnknownError(id));
    if (this.isUsable(id) && (after === undefined || this.statusVersion(id) > after)) return Promise.resolve();
    return new Promise<void>((resolve, reject) => {
      const w: Waiter = { host: id, resolve, reject, signal, after };
      if (signal) {
        w.onAbort = () => { this.waiters = this.waiters.filter(x => x !== w); reject(abortError()); };
        signal.addEventListener("abort", w.onAbort, { once: true });
      }
      this.waiters.push(w);
    });
  }

  private flushWaiters(host: HostId) {
    const isReady = (w: Waiter) => this.resolveAlias(w.host) === host && this.isUsable(w.host) && (w.after === undefined || this.statusVersion(w.host) > w.after);
    const ready = this.waiters.filter(isReady);
    this.waiters = this.waiters.filter(w => !isReady(w));
    for (const w of ready) { if (w.onAbort) w.signal?.removeEventListener("abort", w.onAbort); w.resolve(); }
  }

  private rejectWaiters(host: HostId) {
    const gone = this.waiters.filter(w => w.host === host);
    this.waiters = this.waiters.filter(w => w.host !== host);
    for (const w of gone) { if (w.onAbort) w.signal?.removeEventListener("abort", w.onAbort); w.reject(new HostUnknownError(host)); }
  }

  onUsable(cb: (host: HostId) => void): () => void { this.usableListeners.add(cb); return () => { this.usableListeners.delete(cb); }; }
  onReattach(cb: (host: HostId) => void): () => void { this.reattachListeners.add(cb); return () => { this.reattachListeners.delete(cb); }; }

  kick(host?: HostId) {
    if (!this.initPromise) return;
    invoke("hosts_kick", host ? { host } : {}).catch(() => {});
  }

  // Retry now on network change and after a sleep (a 5 s timer that fired >30 s late).
  private installWakeHooks() {
    if (typeof window === "undefined" || this.teardown) return;
    const onOnline = () => this.kick();
    window.addEventListener("online", onOnline);
    let last = Date.now();
    const timer = window.setInterval(() => {
      const now = Date.now();
      if (now - last > WAKE_GAP_MS) this.kick();
      last = now;
    }, WAKE_POLL_MS);
    this.teardown = () => { window.removeEventListener("online", onOnline); window.clearInterval(timer); };
  }

  // Tests only.
  _dispose() {
    for (const u of this.unlisten) u();
    this.unlisten = [];
    this.teardown?.();
    this.teardown = null;
  }
}

function abortError(): Error {
  const e = new Error("aborted");
  e.name = "AbortError";
  return e;
}

export const registry = new HostRegistry();
