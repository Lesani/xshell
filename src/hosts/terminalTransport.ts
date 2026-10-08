import { invoke, Channel } from "@tauri-apps/api/core";
import type { Tab } from "../types";
import { getShellById } from "../shells";
import { isHostError, type HostErrorCode, type HostId, type LaunchSpec, type RemoteExit, type TerminalMeta } from "./types";

// The one place TerminalTab talks to a PTY. Local tabs use today's spawn/write/resize/close
// commands unchanged; remote tabs use host_term_* and a lifecycle controller that makes the
// asynchronous start cancellable and idempotent (amendment 16).

export interface TerminalSinks {
  data(bytes: Uint8Array): void;
  exit(code: number): void;
}

export interface StartOptions {
  cols: number;
  rows: number;
  shellMode: string;
  shellCommand: string | null;
  shellId: string | null;
  fullscreenRendering: boolean;
  forceSyncOutput: boolean;
}

// The pre-hosts spawn arguments, computed exactly as TerminalTab did: raw shells use the
// tab's shellId; agent sessions fall back to the user's default shell.
export function localShellOptions(tab: Tab, defaultShellId: string): { shellMode: string; shellId: string | null; shellCommand: string | null } {
  const shellMode = tab.shellMode || "claude";
  const shellId = tab.shellId || (shellMode === "claude" ? defaultShellId : null);
  const shellCommand = shellId ? (getShellById(shellId)?.command || null) : null;
  return { shellMode, shellId, shellCommand };
}

// A remote Terminal this Desktop is creating. Kept until a `terminals` list confirms it, so
// reconcile never drops a Tab whose Terminal is still being opened.
export interface PendingOpen {
  host: HostId;
  spec: LaunchSpec;
  meta: TerminalMeta;
  state: "opening" | "sent" | "failed";
  error?: string;
}
export const pendingOpens = new Map<string, PendingOpen>();
export const pendingUuids = (): ReadonlySet<string> => new Set(pendingOpens.keys());

// Connection-level failures of a start: the TerminalTab waits for the Host and tries again.
// An open is retried only when the request never reached the Daemon (offline/busy); after a
// timeout the Terminal may exist, so that open is reported as failed.
const RETRY_ATTACH: ReadonlySet<HostErrorCode> = new Set<HostErrorCode>(["offline", "busy", "timeout", "incompatible"]);
const RETRY_OPEN: ReadonlySet<HostErrorCode> = new Set<HostErrorCode>(["offline", "busy", "incompatible"]);
export function isRetryableStartError(err: unknown, kind: "open" | "attach"): boolean {
  return isHostError(err) && (kind === "open" ? RETRY_OPEN : RETRY_ATTACH).has(err.code);
}

const toBytes = (buf: ArrayBuffer | number[] | Uint8Array): Uint8Array =>
  buf instanceof Uint8Array ? buf : buf instanceof ArrayBuffer ? new Uint8Array(buf) : Uint8Array.from(buf);

function errorText(e: unknown): string {
  if (typeof e === "string") return e;
  if (e && typeof e === "object" && "message" in e) return String((e as { message: unknown }).message);
  return String(e);
}

// ── Local ───────────────────────────────────────────────────────────

async function startLocal(tab: Tab, o: StartOptions, sinks: TerminalSinks) {
  // PTY output arrives as raw bytes over a Channel, pre-coalesced into whole frames on the
  // Rust side; xterm reassembles multibyte sequences across chunks.
  const onData = new Channel<ArrayBuffer>();
  onData.onmessage = (buf) => sinks.data(new Uint8Array(buf));
  const onExit = new Channel<number>();
  onExit.onmessage = (code) => sinks.exit(code);
  localChannels.set(tab.id, { onData, onExit });
  await invoke("spawn_terminal", { id: tab.id, sessionId: tab.sessionId || null, cwd: tab.projectPath || ".", cols: o.cols, rows: o.rows, shellMode: o.shellMode, shellCommand: o.shellCommand, shellId: o.shellId, agent: tab.agent || null, fullscreenRendering: o.fullscreenRendering, forceSyncOutput: o.forceSyncOutput, onData, onExit });
}
const localChannels = new Map<string, { onData: Channel<ArrayBuffer>; onExit: Channel<number> }>();

// ── Remote lifecycle controller ─────────────────────────────────────

interface Entry {
  host: HostId;
  uuid: string;
  mountGen: number;       // bumped per mount; a stale mount's unmount is ignored
  sinks: TerminalSinks | null;
  inflight: Promise<RemoteStartResult> | null; // single-flight start per uuid
  epoch: number;          // bumped per attach/open call; older channels are ignored
  attached: boolean;
  opening: Promise<unknown> | null;
  closeIntent: boolean;
  closing: Promise<void> | null;
  gone: boolean;          // no longer listed by the Daemon
}

// A start that failed for a connection reason; the caller waits for the Host and retries.
export class RetryableStartError extends Error {
  readonly cause: unknown;
  constructor(cause: unknown) { super(errorText(cause)); this.name = "RetryableStartError"; this.cause = cause; }
}

export interface RemoteStartResult { kind: "opened" | "attached"; exitCode: number | null; pid: number | null }

export class RemoteTerminals {
  private entries = new Map<string, Entry>();
  private gen = 0;

  private entry(host: HostId, uuid: string): Entry {
    let e = this.entries.get(uuid);
    if (!e) {
      e = { host, uuid, mountGen: 0, sinks: null, inflight: null, epoch: 0, attached: false, opening: null, closeIntent: false, closing: null, gone: false };
      this.entries.set(uuid, e);
    }
    return e;
  }

  // Registers the xterm that receives this Terminal's output; returns the mount generation.
  mount(host: HostId, uuid: string, sinks: TerminalSinks): number {
    const e = this.entry(host, uuid);
    e.mountGen = ++this.gen;
    e.sinks = sinks;
    return e.mountGen;
  }

  isClosing(uuid: string): boolean { return !!this.entries.get(uuid)?.closeIntent; }

  // Opens (pending) or attaches the Terminal. Concurrent starts for one uuid share one call.
  // Returns null when the mount went stale, the signal aborted or the Tab is being closed.
  async start(host: HostId, uuid: string, gen: number, o: StartOptions, signal?: AbortSignal): Promise<RemoteStartResult | null> {
    const e = this.entry(host, uuid);
    if (signal?.aborted || e.closeIntent || e.mountGen !== gen) return null;
    if (!e.inflight) {
      const p = this.doStart(e, o);
      e.inflight = p;
      p.finally(() => {
        if (e.inflight === p) e.inflight = null;
        // Nobody is mounted any more (unmounted while the call was in flight): drop the
        // attachment again, unless the Tab is being closed.
        if (!e.sinks && e.attached && !e.closeIntent && !e.gone) this.detachNow(e);
      }).catch(() => {});
    }
    const r = await e.inflight;
    // Stale continuation: unmounted, remounted, aborted or closing meanwhile.
    if (signal?.aborted || e.mountGen !== gen || !e.sinks || e.closeIntent) return null;
    return r;
  }

  // Re-run the attach for a mounted Tab (amendment 20: config replacement). Single-flight.
  async reattach(host: HostId, uuid: string, gen: number, o: StartOptions, signal?: AbortSignal): Promise<RemoteStartResult | null> {
    const e = this.entry(host, uuid);
    if (e.inflight || !e.attached) return this.start(host, uuid, gen, o, signal);
    e.attached = false;
    return this.start(host, uuid, gen, o, signal);
  }

  private channels(e: Entry) {
    const epoch = ++e.epoch;
    let received = 0;
    let pendingExit: RemoteExit | null = null;
    const current = () => e.epoch === epoch;
    const fireExit = (code: number) => { if (current()) e.sinks?.exit(code); };
    const onData = new Channel<ArrayBuffer>();
    onData.onmessage = (buf) => {
      const bytes = toBytes(buf);
      received += bytes.length;
      if (current()) e.sinks?.data(bytes);
      if (pendingExit && received >= pendingExit.bytes) { const c = pendingExit.code; pendingExit = null; fireExit(c); }
    };
    // Amendment 8: exit carries the number of bytes delivered before it; apply it only once
    // that much output has arrived on the data channel.
    const onExit = new Channel<RemoteExit | number>();
    onExit.onmessage = (p) => {
      const x: RemoteExit = typeof p === "number" ? { code: p, bytes: 0 } : p;
      if (received >= x.bytes) fireExit(x.code);
      else pendingExit = x;
    };
    return { onData, onExit };
  }

  private async doStart(e: Entry, o: StartOptions): Promise<RemoteStartResult> {
    const pending = pendingOpens.get(e.uuid);
    const { onData, onExit } = this.channels(e);
    if (pending && pending.state === "opening") {
      pending.state = "sent";
      const call = invoke<{ pid: number | null }>("host_term_open", {
        host: e.host, terminal: e.uuid, spec: pending.spec, meta: pending.meta, cols: o.cols, rows: o.rows, onData, onExit,
      });
      e.opening = call;
      try {
        const r = await call;
        e.attached = true;
        return { kind: "opened", exitCode: null, pid: r?.pid ?? null };
      } catch (err) {
        // Not sent (Host went away between waitUsable and the call): open again later.
        if (isRetryableStartError(err, "open")) { pending.state = "opening"; throw new RetryableStartError(err); }
        pending.state = "failed";
        pending.error = errorText(err);
        throw err;
      } finally {
        e.opening = null;
      }
    }
    if (pending && pending.state === "failed") throw pending.error ?? "open failed";
    let r: { exitCode: number | null };
    try {
      r = await invoke<{ exitCode: number | null }>("host_term_attach", { host: e.host, terminal: e.uuid, onData, onExit });
    } catch (err) {
      throw isRetryableStartError(err, "attach") ? new RetryableStartError(err) : err;
    }
    e.attached = true;
    return { kind: "attached", exitCode: r?.exitCode ?? null, pid: null };
  }

  private detachNow(e: Entry) {
    e.attached = false;
    e.epoch++; // ignore anything still arriving on the old channels
    invoke("host_term_detach", { host: e.host, terminal: e.uuid }).catch(() => {});
  }

  // Unmount of the xterm (tab removed, or React StrictMode's simulated unmount). Without a
  // close intent the Terminal keeps running: the attachment is dropped.
  unmount(uuid: string, gen: number) {
    const e = this.entries.get(uuid);
    if (!e || e.mountGen !== gen) return;
    e.sinks = null;
    if (e.closeIntent || e.gone) return;
    if (e.inflight) return; // the start's settle handler detaches
    if (e.attached) this.detachNow(e);
  }

  // Explicit close (the user closed the Tab): ends the Terminal for every Desktop.
  // Independent of unmount and idempotent. A Terminal that was never opened is just dropped.
  close(host: HostId, uuid: string): Promise<void> {
    const e = this.entry(host, uuid);
    e.closeIntent = true;
    if (e.closing) return e.closing;
    e.closing = (async () => {
      const pending = pendingOpens.get(uuid);
      if (pending && pending.state === "opening") { pendingOpens.delete(uuid); return; }
      if (pending && pending.state === "failed") { pendingOpens.delete(uuid); return; }
      if (e.opening) {
        try { await e.opening; } catch (_) { pendingOpens.delete(uuid); return; }
      }
      e.epoch++;
      try { await invoke("host_term_close", { host, terminal: uuid }); } catch (_) {}
      pendingOpens.delete(uuid);
    })();
    return e.closing;
  }

  // The Daemon no longer lists the Terminal: nothing to detach or close.
  forget(uuid: string) {
    const e = this.entries.get(uuid);
    if (e) { e.gone = true; e.sinks = null; }
    this.entries.delete(uuid);
  }

  // Host removed from settings: drop every attachment without ending anything.
  dropHost(host: HostId) {
    for (const [uuid, e] of this.entries) if (e.host === host) { e.gone = true; e.sinks = null; this.entries.delete(uuid); }
  }
}

export const remoteTerminals = new RemoteTerminals();

// ── TerminalTab-facing API ──────────────────────────────────────────

export interface MountedTerminal {
  start(o: StartOptions, signal?: AbortSignal): Promise<RemoteStartResult | null | void>;
  reattach(o: StartOptions, signal?: AbortSignal): Promise<RemoteStartResult | null | void>;
  end(): void;
}

export function mountTerminal(tab: Tab, sinks: TerminalSinks): MountedTerminal {
  if (tab.host && tab.terminal) {
    const host = tab.host, uuid = tab.terminal;
    const gen = remoteTerminals.mount(host, uuid, sinks);
    return {
      start: (o, signal) => remoteTerminals.start(host, uuid, gen, o, signal),
      reattach: (o, signal) => remoteTerminals.reattach(host, uuid, gen, o, signal),
      end: () => {
        // A close intent was already acted on by markClosing; otherwise this detaches.
        remoteTerminals.unmount(uuid, gen);
      },
    };
  }
  let started = false;
  return {
    start: async (o, signal) => {
      if (signal?.aborted) return;
      started = true;
      await startLocal(tab, o, sinks);
    },
    reattach: async () => {},
    end: () => {
      // Channels have no explicit unsubscribe — dropping the handler stops processing, and
      // close_terminal tears down the PTY (and thus the Rust side of the channel).
      const ch = localChannels.get(tab.id);
      if (ch) { ch.onData.onmessage = () => {}; ch.onExit.onmessage = () => {}; localChannels.delete(tab.id); }
      if (started) invoke("close_terminal", { id: tab.id }).catch(() => {});
    },
  };
}

// Close intent for tabs the user closed. Remote Terminals are closed right away (not on
// unmount), so the close survives whatever happens to the component; the later unmount sees
// the intent and does not detach. Local tabs keep closing their PTY on unmount, as before.
export function markClosing(tabs: Tab[]) {
  for (const t of tabs) if (t.host && t.terminal) remoteTerminals.close(t.host, t.terminal);
}

export function writeTerminal(tab: Tab, data: string) {
  if (tab.host && tab.terminal) invoke("host_term_input", { host: tab.host, terminal: tab.terminal, data }).catch(() => {});
  else invoke("write_terminal", { id: tab.id, data }).catch(() => {});
}

export function resizeTerminal(tab: Tab, cols: number, rows: number) {
  if (tab.host && tab.terminal) invoke("host_term_resize", { host: tab.host, terminal: tab.terminal, cols, rows }).catch(() => {});
  else invoke("resize_terminal", { id: tab.id, cols, rows }).catch(() => {});
}
