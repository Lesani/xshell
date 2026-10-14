import type { HostId } from "../hosts/types";

// Native drag-out of explorer rows to the OS (Lesani/xshell#46, D3), from a grip on the row.
// The drag has to start while the mouse button is still held (GTK on Wayland needs the
// press's implicit grab), so the grip takes the press, waits for a small move, and then
// starts the drag of local paths: a Local Host file or folder itself, or a Remote Host file
// first downloaded into the drag area. Remote folders and files over DRAG_OUT_MAX_BYTES are
// not dragged; their grip points to "Download to…". A copy prepared for a press released too
// early is kept for DRAG_CACHE_MS, so the next drag of the same version starts at once.

export const DRAG_OUT_MAX_BYTES = 50 * 1024 * 1024;
export const DRAG_THRESHOLD_PX = 4;
export const DRAG_CACHE_MS = 10 * 60 * 1000;
// A grip found disabled (too large) is checked again after this, and on every press.
export const DRAG_DISABLED_MS = 30 * 1000;

export interface DragOutDeps {
  stat(host: HostId, path: string, withVersion: boolean): Promise<{ size: number; is_dir: boolean; version: string | null }>;
  // The local path a Remote Host file is prepared at (in the drag area).
  preparePath(host: HostId, path: string): Promise<string>;
  download(host: HostId, path: string, local: string, onProgress: (done: number, total: number) => void): Promise<{ version: string }>;
  // Remove a prepared copy. Best effort.
  release(local: string): Promise<void>;
  // Start the native drag; resolves when it ends (true: dropped), rejects when it cannot start.
  start(paths: string[]): Promise<boolean>;
  now(): number;
  setTimeout(fn: () => void, ms: number): unknown;
  clearTimeout(handle: unknown): void;
}

export type GripState =
  | { kind: "ready" }
  | { kind: "disabled"; reason: "tooLarge" | "folderRemote" }
  | { kind: "preparing"; pct: number }
  | { kind: "dragging" };

export type DragOutError = { kind: "start"; error: unknown } | { kind: "prepare"; path: string; error: unknown };

interface Press {
  key: string;
  x: number;
  y: number;
  held: boolean;
  armed: boolean;
  started: boolean;
  local: string | null;
  onError: (e: DragOutError) => void;
}

interface Cached {
  version: string;
  local: string;
  timer: unknown;
}

const keyOf = (host: HostId | undefined, path: string) => `${host ?? ""}\0${path}`;
const READY: GripState = { kind: "ready" };

export class DragOut {
  private press: Press | null = null;
  private cache = new Map<string, Cached>();
  private states = new Map<string, GripState>();
  // When a grip was found disabled.
  private disabledAt = new Map<string, number>();
  // The one preparation per key in flight: the prepared copy, or null when not draggable.
  private preparing = new Map<string, Promise<string | null>>();
  private listeners = new Set<() => void>();
  private version = 0;
  private dragging: string | null = null;

  constructor(private deps: DragOutDeps) {}

  // A native drag from this window is running: the explorer must not take it as a drop.
  get active(): boolean {
    return this.dragging !== null;
  }

  // A grip holds the press: the row must not start its own (HTML5) drag.
  get held(): boolean {
    return !!this.press?.held;
  }

  subscribe = (cb: () => void): (() => void) => {
    this.listeners.add(cb);
    return () => this.listeners.delete(cb);
  };

  getSnapshot = (): number => this.version;

  grip(host: HostId | undefined, path: string, isDir: boolean): GripState {
    if (host && isDir) return { kind: "disabled", reason: "folderRemote" };
    return this.states.get(keyOf(host, path)) ?? READY;
  }

  private set(key: string, s: GripState | null) {
    if (s) this.states.set(key, s);
    else this.states.delete(key);
    if (s?.kind === "disabled") this.disabledAt.set(key, this.deps.now());
    else this.disabledAt.delete(key);
    this.version++;
    for (const l of this.listeners) l();
  }

  // The pointer is over a Remote file's grip: learn its size so an oversized grip shows
  // disabled before it is pressed.
  async probe(host: HostId | undefined, path: string, isDir: boolean): Promise<void> {
    if (!host || isDir) return;
    const key = keyOf(host, path);
    const at = this.disabledAt.get(key);
    if (this.states.has(key) && (at === undefined || this.deps.now() - at < DRAG_DISABLED_MS)) return;
    try {
      const st = await this.deps.stat(host, path, false);
      if (this.states.has(key) && !this.disabledAt.has(key)) return; // Pressed meanwhile.
      if (st.is_dir) this.set(key, { kind: "disabled", reason: "folderRemote" });
      else if (st.size > DRAG_OUT_MAX_BYTES) this.set(key, { kind: "disabled", reason: "tooLarge" });
      else if (this.disabledAt.has(key)) this.set(key, null);
    } catch {
      // The press will try again.
    }
  }

  // The grip is pressed at (x, y). Problems of this press go to `onError`.
  down(host: HostId | undefined, path: string, isDir: boolean, x: number, y: number, onError: (e: DragOutError) => void = () => {}): void {
    if (this.press || this.dragging) return;
    const key = keyOf(host, path);
    // A remote folder is never dragged; a file found too large is checked again.
    if (host && isDir) return;
    const press: Press = { key, x, y, held: true, armed: false, started: false, local: host ? null : path, onError };
    this.press = press;
    if (host) void this.prepare(press, host, path);
  }

  move(x: number, y: number): void {
    const p = this.press;
    if (!p || !p.held || p.armed) return;
    if (Math.hypot(x - p.x, y - p.y) < DRAG_THRESHOLD_PX) return;
    p.armed = true;
    this.maybeStart(p);
  }

  // The button is up (or the press was cancelled). A preparation in flight goes on and is
  // cached.
  up(): void {
    const p = this.press;
    if (!p) return;
    p.held = false;
    if (!p.started) this.press = null;
  }

  // Wait for the key's preparation, starting it unless one is in flight already (a press
  // released early and pressed again shares it).
  private async prepare(press: Press, host: HostId, path: string): Promise<void> {
    const key = press.key;
    let job = this.preparing.get(key);
    if (!job) {
      const started: Promise<string | null> = this.prepareKey(key, host, path).finally(() => {
        if (this.preparing.get(key) === started) this.preparing.delete(key);
      });
      job = started;
      this.preparing.set(key, job);
    }
    try {
      const local = await job;
      if (local && this.press === press) {
        press.local = local;
        this.maybeStart(press);
      } else {
        this.drop(press);
      }
    } catch (error) {
      if (this.press === press && press.held) press.onError({ kind: "prepare", path, error });
      this.drop(press);
    }
  }

  // Stat, then reuse the cached copy of the Host's current version or download it again.
  private async prepareKey(key: string, host: HostId, path: string): Promise<string | null> {
    try {
      const st = await this.deps.stat(host, path, false);
      if (st.is_dir || st.size > DRAG_OUT_MAX_BYTES) {
        this.set(key, { kind: "disabled", reason: st.is_dir ? "folderRemote" : "tooLarge" });
        return null;
      }
      if (this.disabledAt.has(key)) this.set(key, null);
      const { version } = await this.deps.stat(host, path, true);
      const hit = this.cache.get(key);
      if (hit && version && hit.version === version) {
        this.keep(key);
        return hit.local;
      }
      // Never replace a copy a drag is carrying (the target may still read it).
      if (this.dragging === key) throw new Error("the previous drag of this file is still running");
      this.set(key, { kind: "preparing", pct: 0 });
      const local = await this.deps.preparePath(host, path);
      const t = await this.deps.download(host, path, local, (d, total) => {
        const pct = total > 0 ? Math.round((d / total) * 100) : 0;
        const cur = this.states.get(key);
        if (cur?.kind === "preparing" && cur.pct !== pct) this.set(key, { kind: "preparing", pct });
      });
      const old = this.cache.get(key);
      if (old && old.timer !== null) this.deps.clearTimeout(old.timer);
      this.cache.set(key, { version: t.version, local, timer: null });
      this.set(key, null);
      this.keep(key);
      return local;
    } catch (error) {
      if (this.states.get(key)?.kind === "preparing") this.set(key, null);
      throw error;
    }
  }

  private drop(press: Press) {
    if (this.press === press && !press.started) this.press = null;
  }

  // (Re)start the expiry of a cached copy.
  private keep(key: string) {
    const c = this.cache.get(key);
    if (!c) return;
    if (c.timer !== null) this.deps.clearTimeout(c.timer);
    c.timer = this.deps.setTimeout(() => this.expire(key), DRAG_CACHE_MS);
  }

  private expire(key: string) {
    const c = this.cache.get(key);
    if (!c) return;
    // Never under a running drag (the target may still be reading it) or a preparation.
    if (this.dragging === key || this.preparing.has(key)) {
      this.keep(key);
      return;
    }
    this.cache.delete(key);
    void this.deps.release(c.local).catch(() => {});
  }

  private maybeStart(p: Press) {
    if (!p.held || !p.armed || !p.local || p.started) return;
    p.started = true;
    this.dragging = p.key;
    this.set(p.key, { kind: "dragging" });
    const finish = () => {
      if (this.press === p) this.press = null;
      this.dragging = null;
      this.set(p.key, null);
      // The drop target may copy the file after the drop: keep it the full time again.
      this.keep(p.key);
    };
    this.deps.start([p.local]).then(finish, (error) => {
      finish();
      p.onError({ kind: "start", error });
    });
  }
}
