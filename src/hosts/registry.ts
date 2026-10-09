import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { AGENT_IDS, AGENTS, type AgentId } from "../agents";
import { hostInvoke } from "./hostInvoke";
import { LOCAL_HOST } from "./localHost";
import type { HostConfig, HostId, HostSnapshot, HostStatus, HostTerminalsEvent, TerminalInfo } from "./types";

// The Desktop's view of its Remote Hosts: configs, Host Status, the live `terminals` list per
// Host and the agent CLIs detected on each. An external store read with useSyncExternalStore.
//
// With no Hosts configured the registry stays dormant: no event listeners, no Tauri calls.
// The Local Host joins (`setLocal`) when its Terminals run in a Daemon: it gets status and
// `terminals` lists under the wire id "local" but is never in `configs`.

export interface RegistrySnapshot {
  configs: HostConfig[];
  status: Record<HostId, HostStatus>;
  live: Record<HostId, TerminalInfo[] | null>; // null = never connected this run
  agents: Record<HostId, Record<AgentId, boolean>>;
  // Bumped each time a Host becomes usable or its connection is replaced: views showing data
  // read from that Host refetch when it changes.
  epochs: Record<HostId, number>;
}

export const isUsableStatus = (s: HostStatus | undefined): boolean =>
  !!s && (s.status === "connected" || s.status === "upgrade-pending");

export class HostUnknownError extends Error {
  readonly code = "unknown-host";
  constructor(host: HostId) { super(`unknown host ${host}`); this.name = "HostUnknownError"; }
}

type Waiter = { host: HostId; resolve: () => void; reject: (e: unknown) => void; signal?: AbortSignal; onAbort?: () => void; after?: number };

const WAKE_POLL_MS = 5000;
const WAKE_GAP_MS = 30000;

export class HostRegistry {
  private snap: RegistrySnapshot = { configs: [], status: {}, live: {}, agents: {}, epochs: {} };
  private listeners = new Set<() => void>();
  // Amendment 21: per-host sequence numbers, one per field. A status event bumps only the
  // status number and a terminals event only the terminals number; a `hosts_status` snapshot
  // applies each field only where no newer event of that kind arrived.
  private statusSeq: Record<HostId, number> = {};
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

  // ── store ──
  subscribe = (l: () => void) => { this.listeners.add(l); return () => { this.listeners.delete(l); }; };
  getSnapshot = () => this.snap;
  private emit(next: Partial<RegistrySnapshot>) {
    this.snap = { ...this.snap, ...next };
    for (const l of this.listeners) l();
  }

  get initialized(): boolean { return this.initPromise !== null; }

  // Single-flight (StrictMode-safe). With an empty list nothing starts; `configure` starts
  // the registry later when the first Host is added.
  init(hosts: HostConfig[]): Promise<void> {
    if (this.initPromise) return this.initPromise;
    this.emit({ configs: hosts });
    if (hosts.length === 0) return Promise.resolve();
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
      const seqBefore = { status: { ...this.statusSeq }, live: { ...this.liveSeq } };
      let snaps: HostSnapshot[] = [];
      try { snaps = await invoke<HostSnapshot[]>("hosts_status"); } catch (_) {}
      this.applySnapshot(snaps.filter(x => x.status.host === LOCAL_HOST), seqBefore);
    });
  }

  get localEnabled(): boolean { return this.local; }

  private async start() {
    // 1. listen to both events, 2. hosts_configure, 3. hosts_status for anything emitted
    //    before step 1 (applied only where no newer event arrived).
    this.unlisten.push(await listen<HostStatus>("hosts:status", e => this.applyStatus(e.payload)));
    this.unlisten.push(await listen<HostTerminalsEvent>("hosts:terminals", e => this.applyTerminals(e.payload.host, e.payload.list)));
    const seqBefore = { status: { ...this.statusSeq }, live: { ...this.liveSeq } };
    try { await invoke("hosts_configure", { hosts: this.snap.configs }); } catch (_) {}
    let snaps: HostSnapshot[] = [];
    try { snaps = await invoke<HostSnapshot[]>("hosts_status"); } catch (_) {}
    this.applySnapshot(snaps, seqBefore);
    this.installWakeHooks();
  }

  applySnapshot(snaps: HostSnapshot[], seqBefore: { status: Record<HostId, number>; live: Record<HostId, number> }) {
    const status = { ...this.snap.status };
    const live = { ...this.snap.live };
    const prevStatus = this.snap.status;
    let changed = false;
    for (const s of snaps) {
      const h = s.status.host;
      if (!this.isConfigured(h)) continue;
      // Each field independently: skip it only if a newer event of that kind was applied.
      if ((this.statusSeq[h] ?? 0) === (seqBefore.status[h] ?? 0)) { status[h] = s.status; changed = true; }
      if ((this.liveSeq[h] ?? 0) === (seqBefore.live[h] ?? 0) && (s.terminals !== null || !(h in live))) { live[h] = s.terminals; changed = true; }
    }
    if (!changed) return;
    this.emit({ status, live });
    for (const s of snaps) this.afterStatus(prevStatus[s.status.host], status[s.status.host]);
  }

  applyStatus(s: HostStatus) {
    const h = s.host;
    if (!this.isConfigured(h)) return;
    this.statusSeq[h] = (this.statusSeq[h] ?? 0) + 1;
    const prev = this.snap.status[h];
    this.emit({ status: { ...this.snap.status, [h]: s } });
    this.afterStatus(prev, s);
  }

  applyTerminals(host: HostId, list: TerminalInfo[]) {
    if (!this.isConfigured(host)) return;
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
    const current = () => (this.probeGen[host] ?? 0) === gen && this.isConfigured(host);
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
      this.agentsProbed.delete(id);
      this.probeGen[id] = (this.probeGen[id] ?? 0) + 1;
    }
    for (const id of removed) {
      delete status[id]; delete live[id];
      this.rejectWaiters(id);
    }
    this.emit({ configs: list, status, live, agents });
    if (!this.initPromise) {
      if (list.length > 0) await this.init(list);
      return;
    }
    await this.initPromise;
    await invoke("hosts_configure", { hosts: list });
  }

  isConfigured(id: HostId): boolean { return id === LOCAL_HOST ? this.local : this.snap.configs.some(c => c.id === id); }
  config(id: HostId): HostConfig | undefined { return this.snap.configs.find(c => c.id === id); }
  hostName(id: HostId | undefined): string { return (id && this.config(id)?.name) || id || ""; }
  getStatus(id: HostId): HostStatus | undefined { return this.snap.status[id]; }
  isUsable(id: HostId): boolean { return isUsableStatus(this.snap.status[id]); }

  // Bumped by every status event of the Host (generation for waitUsable's `after`).
  statusVersion(id: HostId): number { return this.statusSeq[id] ?? 0; }

  // Resolves once the Host is usable; rejects when the signal aborts or the Host is removed.
  // With `after`, a status from that version or older does not count: the caller saw the
  // connection fail and waits for a newer usable status.
  waitUsable(id: HostId, signal?: AbortSignal, after?: number): Promise<void> {
    if (signal?.aborted) return Promise.reject(abortError());
    if (!this.isConfigured(id)) return Promise.reject(new HostUnknownError(id));
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
    const v = this.statusVersion(host);
    const isReady = (w: Waiter) => w.host === host && (w.after === undefined || v > w.after);
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
