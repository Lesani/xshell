import type { HostId } from "../hosts/types";
import { baseName, parentOf, type PathStyle } from "./paths";

// Open with sync-back (Lesani/xshell#46, design D1/D2; plan-x46 amendments A5–A10, A12).
//
// A file of the Local Host opens as itself in its default app. A file of a Remote Host is
// downloaded to a private copy on this machine (src-tauri/src/open_copies.rs) and opened; one
// poller watches every copy and uploads it after each save, but only over the remote version
// it was made from (`ifVersion`). Every version is `sha256:<hex>` of the content, on both
// sides, so a local hash compares directly with a remote version.
//
// States of a copy ("session"):
//   downloading     first download, refresh, Keep remote, Download again
//   clean           the copy equals the remote version it came from
//   settling        a save was seen; waiting until size and mtime hold still
//   hashing         snapshot of the copy taken and hashed (A6)
//   uploading       the snapshot is being uploaded
//   waitingHost     the Host was unavailable; retried when it is usable and every 15 s
//   unknownOutcome  a commit got no answer (A7); the Host is checked before anything else
//   conflict        the remote file changed meanwhile; the user picks Overwrite or Keep remote
//   failed          the Host refused the upload; the user picks Retry
//   missing         the copy has been gone for 30 polls (A12)
//   released        closed and deleted
// `pending` (recovery metadata in meta.json, A5) is set from the first save seen until an
// upload is acknowledged whose content equals the copy's current content.

export type SyncState =
  | "downloading" | "clean" | "settling" | "hashing" | "uploading"
  | "waitingHost" | "unknownOutcome" | "conflict" | "failed" | "missing" | "released";

export interface CopyPaths { dir: string; payload: string; incoming: string }
export interface CopyStat { mtime_ms: number; size: number }
export interface Snapshot { path: string; version: string; size: number; mtime_ms: number }

// What meta.json holds; `dir` is added by open_copies_pending.
export interface CopyMeta {
  v: 1;
  host: HostId;
  remotePath: string;
  remoteVersion: string | null;
  localVersion: string | null;
  pending: boolean;
  // Snapshot hashes of uploads that may have reached the Host (A7), oldest first, at most
  // MAX_ATTEMPTS. Cleared by an acknowledged upload.
  attempts: string[];
  // The copy had gone missing (A12); restored as missing.
  missing?: boolean;
  dir?: string;
}

export const LOCAL_CHANGED = "local copy changed";

export interface SyncDeps {
  copyPaths(host: HostId, remotePath: string): Promise<CopyPaths>;
  statCopy(payload: string): Promise<CopyStat | null>;
  hashCopy(payload: string): Promise<string>;
  snapshot(payload: string): Promise<Snapshot>;
  // Remove a snapshot (or a copy folder: `release`). Never rejects.
  dropFile(path: string): Promise<void>;
  // Move a download over the payload if the payload still has `expect` (a version; or
  // "absent": no payload). Rejects with LOCAL_CHANGED otherwise. Resolves to the installed
  // file's size and mtime (those of the downloaded content).
  install(incoming: string, payload: string, expect: string): Promise<CopyStat>;
  download(host: HostId, remotePath: string, localPath: string, onProgress: (done: number, total: number) => void): Promise<{ version: string }>;
  upload(host: HostId, dir: string, localPath: string, name: string, ifVersion: string | null): Promise<{ version: string }>;
  // The remote file's version; "deleted" when it does not exist.
  remoteStat(host: HostId, path: string): Promise<string>;
  openLocal(path: string): Promise<void>;
  readMeta(dir: string): Promise<CopyMeta | null>;
  writeMeta(dir: string, meta: CopyMeta): Promise<void>;
  // Delete a copy folder. With a guard, only while its payload is `expect` (a version, or
  // "absent"); rejects with LOCAL_CHANGED otherwise.
  release(dir: string, guard?: { payload: string; expect: string }): Promise<void>;
  isHostUnavailable(e: unknown): boolean;
  isFileChanged(e: unknown): boolean;
  isOutcomeUnknown(e: unknown): boolean;
  hostUsable(host: HostId): boolean;
  // The path rules of the Host's OS, when known; else they are read from the path's form.
  hostPathStyle?(host: HostId): PathStyle | undefined;
  onHostUsable(cb: (host: HostId) => void): () => void;
}

export const POLL_MS = 1000;
export const STABLE_POLLS = 2;
export const MISSING_POLLS = 30;
export const RETRY_MS = 15_000;
export const MAX_UPLOADS = 2;
export const MAX_ATTEMPTS = 8;
const UPLOADED_NOTICE_MS = 4000;

// ── Pure parts ────────────────────────────────────────────────────────

export interface Watch {
  // Size and mtime of the content last hashed (null: unknown, the next stat is a change).
  known: CopyStat | null;
  // A change being watched until it holds still.
  candidate: CopyStat | null;
  stable: number;
  absent: number;
}

export type WatchEvent = "none" | "changed" | "settled" | "absent" | "missing";

export const freshWatch = (known: CopyStat | null): Watch => ({ known, candidate: null, stable: 0, absent: 0 });

const sameStat = (a: CopyStat | null, b: CopyStat | null) => !!a && !!b && a.mtime_ms === b.mtime_ms && a.size === b.size;

// One poll of a copy. A missing file is not a deletion: an editor saving by rename leaves a
// gap of a poll or so; only MISSING_POLLS in a row (and more) report "missing". A change
// "settles" once the same size and mtime were seen on STABLE_POLLS polls after it.
export function watchStep(w: Watch, s: CopyStat | null): { w: Watch; ev: WatchEvent } {
  if (!s) {
    const absent = w.absent + 1;
    return { w: { ...w, absent }, ev: absent >= MISSING_POLLS ? "missing" : "absent" };
  }
  const base = { ...w, absent: 0 };
  if (!base.candidate) {
    if (sameStat(s, base.known)) return { w: base, ev: "none" };
    return { w: { ...base, candidate: s, stable: 0 }, ev: "changed" };
  }
  if (!sameStat(s, base.candidate)) return { w: { ...base, candidate: s, stable: 0 }, ev: "changed" };
  const stable = base.stable + 1;
  return { w: { ...base, stable }, ev: stable >= STABLE_POLLS ? "settled" : "none" };
}

export type VerifyOutcome = { kind: "done" } | { kind: "upload"; ifVersion: string } | { kind: "conflict" };

// After a restart, a lost connection or an unanswered commit (A7), compare the remote version
// with what this copy knows: the content now (`snap`), the version it came from (`base`) and
// uploads that may have landed (`attempts`).
export function verifyOutcome(remote: string, snap: string, base: string | null, attempts: readonly string[]): VerifyOutcome {
  if (remote === snap) return { kind: "done" };
  if (base && remote === base) return { kind: "upload", ifVersion: base };
  if (attempts.includes(remote)) return { kind: "upload", ifVersion: remote };
  return { kind: "conflict" };
}

// ── The manager ───────────────────────────────────────────────────────

interface Session {
  key: string;
  host: HostId;
  remotePath: string;
  name: string;
  paths: CopyPaths;
  state: SyncState;
  remoteVersion: string | null;
  localVersion: string | null;
  pending: boolean;
  attempts: string[];
  // Deleting the copy is under way (an explorer went away); an open cancels it until the
  // point of no return, `deleting`.
  releasing: boolean;
  deleting: Promise<void> | null;
  // The next upload checks the Host first (verifyOutcome).
  verifyFirst: boolean;
  dirtyAgain: boolean;
  watch: Watch;
  owners: Set<string>;
  busy: boolean;
  checking: boolean;
  progress: number | null;
  error: string | null;
  // The latest meta.json write; rejects when it failed.
  metaWrite: Promise<void>;
}

export interface SyncView {
  key: string;
  host: HostId;
  remotePath: string;
  name: string;
  state: SyncState;
  checking: boolean;
  progress: number | null;
  error: string | null;
}

export type SyncNotice =
  | { id: number; host: HostId | undefined; kind: "uploaded"; name: string }
  | { id: number; host: HostId | undefined; kind: "openFailed"; name: string; error: string }
  | { id: number; host: HostId | undefined; kind: "restored"; n: number };

export interface SyncSnapshot { sessions: SyncView[]; notices: SyncNotice[] }

type NoticeInput =
  | { host: HostId | undefined; kind: "uploaded"; name: string }
  | { host: HostId | undefined; kind: "openFailed"; name: string; error: string }
  | { host: HostId | undefined; kind: "restored"; n: number };

export function messageOf(e: unknown): string {
  if (typeof e === "string") return e;
  if (e instanceof Error) return e.message;
  return String(e);
}

const keyOf = (host: HostId, path: string) => `${host}\u0000${path}`;

export class SyncManager {
  private sessions = new Map<string, Session>();
  private listeners = new Set<() => void>();
  private snap: SyncSnapshot = { sessions: [], notices: [] };
  private notices: SyncNotice[] = [];
  private nextNotice = 1;
  private poller: ReturnType<typeof setInterval> | null = null;
  private retrier: ReturnType<typeof setInterval> | null = null;
  private unsubUsable: (() => void) | null = null;
  private ticking = false;
  private uploads = 0;
  private uploadQueue: (() => void)[] = [];
  private opening = new Map<string, Promise<void>>();

  constructor(private deps: SyncDeps) {}

  // A Remote Host's paths follow its OS: `/` only on POSIX ("\\" is a filename character
  // there), `\` and `/` on Windows.
  private remoteName(host: HostId, p: string): string {
    return baseName(p, this.deps.hostPathStyle?.(host));
  }

  private remoteParent(host: HostId, p: string): string {
    // A file always has a parent ("/a" -> "/", "C:\\a" -> "C:\\").
    return parentOf(p, this.deps.hostPathStyle?.(host)) ?? "/";
  }

  subscribe = (l: () => void) => { this.listeners.add(l); return () => { this.listeners.delete(l); }; };
  getSnapshot = () => this.snap;

  private emit() {
    this.snap = {
      sessions: [...this.sessions.values()].map(s => ({
        key: s.key, host: s.host, remotePath: s.remotePath, name: s.name, state: s.state,
        checking: s.checking, progress: s.progress, error: s.error,
      })),
      notices: [...this.notices],
    };
    for (const l of this.listeners) l();
  }

  private notify(n: NoticeInput, ttl?: number) {
    const notice = { ...n, id: this.nextNotice++ } as SyncNotice;
    this.notices.push(notice);
    if (ttl) setTimeout(() => this.dismiss(notice.id), ttl);
  }

  dismiss(id: number) {
    const before = this.notices.length;
    this.notices = this.notices.filter(n => n.id !== id);
    if (this.notices.length !== before) this.emit();
  }

  // Tests and the explorer's badges.
  state(host: HostId, path: string): SyncState | null {
    return this.sessions.get(keyOf(host, path))?.state ?? null;
  }

  // Open `path` of `host` (undefined: the Local Host) in its default app, for the explorer
  // `owner`.
  async open(host: HostId | undefined, path: string, owner: string): Promise<void> {
    if (!host) {
      try {
        await this.deps.openLocal(path);
      } catch (e) {
        this.notify({ host, kind: "openFailed", name: baseName(path), error: messageOf(e) });
        this.emit();
      }
      return;
    }
    const key = keyOf(host, path);
    const existing = this.sessions.get(key);
    if (existing?.deleting) {
      // Past the point of no return: let the delete finish, then open afresh.
      await existing.deleting;
      return this.open(host, path, owner);
    }
    if (existing) {
      // An owner again: a release still waiting for its meta write gives up.
      existing.owners.add(owner);
      return this.reopen(existing);
    }
    // A second click while the first open is still setting up: that one opens the file.
    const inFlight = this.opening.get(key);
    if (inFlight) {
      await inFlight;
      this.sessions.get(key)?.owners.add(owner);
      return;
    }
    const p = this.openNew(host, path, owner);
    this.opening.set(key, p);
    try {
      await p;
    } finally {
      this.opening.delete(key);
    }
  }

  private async openNew(host: HostId, path: string, owner: string): Promise<void> {
    const key = keyOf(host, path);
    let paths: CopyPaths;
    let meta: CopyMeta | null = null;
    let present = false;
    try {
      paths = await this.deps.copyPaths(host, path);
      meta = await this.deps.readMeta(paths.dir);
      present = (await this.deps.statCopy(paths.payload)) !== null;
    } catch (e) {
      this.notify({ host, kind: "openFailed", name: this.remoteName(host, path), error: messageOf(e) });
      this.emit();
      return;
    }
    if (this.sessions.has(key)) {
      // Another open got here first.
      const s = this.sessions.get(key)!;
      s.owners.add(owner);
      return this.reopen(s);
    }
    // A copy from an earlier run is never deleted on its meta's word alone: unless its
    // payload is what the meta says was synced, it is resumed like pending changes.
    let resume = !!meta?.pending || (!meta && present);
    if (meta && !resume) {
      try {
        await this.deps.release(paths.dir, { payload: paths.payload, expect: present ? (meta.localVersion ?? "absent") : "absent" });
      } catch (e) {
        if (!messageOf(e).startsWith(LOCAL_CHANGED)) {
          this.notify({ host, kind: "openFailed", name: this.remoteName(host, path), error: messageOf(e) });
          this.emit();
          return;
        }
        resume = true;
      }
    }
    if (resume) {
      const base: CopyMeta = meta ?? { v: 1, host, remotePath: path, remoteVersion: null, localVersion: null, pending: true, attempts: [] };
      const s = this.restoreOne({ ...base, pending: true, missing: false, dir: paths.dir }, paths);
      s.owners.add(owner);
      this.emit();
      this.kick(s);
      await this.deps.openLocal(paths.payload).catch(() => {});
      return;
    }
    if (meta) {
      try {
        paths = await this.deps.copyPaths(host, path);
      } catch (e) {
        this.notify({ host, kind: "openFailed", name: this.remoteName(host, path), error: messageOf(e) });
        this.emit();
        return;
      }
    }
    const s = this.newSession(host, path, paths, "downloading");
    s.owners.add(owner);
    this.sessions.set(key, s);
    this.ensureTimers();
    this.emit();
    await this.replacePayload(s, null, "open");
  }

  private newSession(host: HostId, remotePath: string, paths: CopyPaths, state: SyncState): Session {
    return {
      key: keyOf(host, remotePath), host, remotePath, name: this.remoteName(host, remotePath), paths, state,
      remoteVersion: null, localVersion: null, pending: false, attempts: [], releasing: false, deleting: null, verifyFirst: false,
      dirtyAgain: false, watch: freshWatch(null), owners: new Set(), busy: false, checking: false,
      progress: null, error: null, metaWrite: Promise.resolve(),
    };
  }

  // Open an existing copy again. A clean copy whose remote file changed is refreshed first.
  private async reopen(s: Session): Promise<void> {
    if (s.state === "missing") return this.downloadAgain(s.key);
    if (s.state === "downloading") return;
    if (s.state === "clean" && !s.busy) {
      s.busy = true;
      let remote: string | null = null;
      try {
        remote = await this.deps.remoteStat(s.host, s.remotePath);
      } catch {
        // Offline: open what is here.
      } finally {
        s.busy = false;
      }
      if (s.state === "clean" && remote !== null && remote !== s.remoteVersion) {
        await this.replacePayload(s, s.localVersion, "refresh");
        return;
      }
    }
    await this.deps.openLocal(s.paths.payload).catch(e => {
      this.notify({ host: s.host, kind: "openFailed", name: s.name, error: messageOf(e) });
      this.emit();
    });
  }

  // Download the remote file over the payload. `expect`: the payload's content the caller
  // knows (null: no payload); a payload with other content is kept and the copy goes to
  // conflict (A9).
  private async replacePayload(s: Session, expect: string | null, why: "open" | "refresh" | "keepRemote" | "again"): Promise<void> {
    const before = s.state;
    s.busy = true;
    s.state = "downloading";
    s.progress = 0;
    s.error = null;
    this.emit();
    try {
      if (expect !== null) {
        const cur = await this.deps.hashCopy(s.paths.payload).catch(() => null);
        if (cur !== null && cur !== expect) throw new Error(LOCAL_CHANGED);
      }
      const res = await this.deps.download(s.host, s.remotePath, s.paths.incoming, (done, total) => {
        s.progress = total > 0 ? done / total : 1;
        this.emit();
      });
      const installed = await this.deps.install(s.paths.incoming, s.paths.payload, expect ?? "absent");
      // The baseline is the installed content: its version and its size and mtime, from the
      // same file. A save right after the install shows as a different hash.
      s.remoteVersion = res.version;
      s.localVersion = res.version;
      s.attempts = [];
      s.verifyFirst = false;
      s.dirtyAgain = false;
      s.watch = freshWatch(installed);
      s.progress = null;
      const now = await this.deps.hashCopy(s.paths.payload).catch(() => null);
      s.busy = false;
      if (now === res.version) {
        s.pending = false;
        s.state = "clean";
        this.persist(s);
      } else {
        this.toSettling(s);
      }
      this.emit();
      await this.deps.openLocal(s.paths.payload).catch(e => {
        this.notify({ host: s.host, kind: "openFailed", name: s.name, error: messageOf(e) });
        this.emit();
      });
      this.releaseIfOrphan(s);
    } catch (e) {
      s.progress = null;
      s.busy = false;
      if (messageOf(e).startsWith(LOCAL_CHANGED)) {
        // Local edits: never replaced. They meet the remote change as a conflict.
        s.localVersion = await this.deps.hashCopy(s.paths.payload).catch(() => s.localVersion);
        s.watch = freshWatch(await this.deps.statCopy(s.paths.payload).catch(() => null));
        if (why === "again") {
          // The copy came back while it downloaded: treat it as a save.
          this.toSettling(s);
        } else {
          s.state = "conflict";
          s.pending = true;
          this.persist(s);
        }
        this.emit();
        return;
      }
      this.notify({ host: s.host, kind: "openFailed", name: s.name, error: messageOf(e) });
      if (why === "open") {
        // Nothing of the user's is in it: the payload was never installed.
        this.sessions.delete(s.key);
        s.state = "released";
        await this.deps.release(s.paths.dir, { payload: s.paths.payload, expect: "absent" }).catch(() => {});
        this.stopTimersIfIdle();
      } else {
        s.state = before;
      }
      this.emit();
      if (why === "refresh") await this.deps.openLocal(s.paths.payload).catch(() => {});
    }
  }

  private toSettling(s: Session) {
    s.state = "settling";
    s.pending = true;
    // Unknown baseline: the next poll starts the settle.
    s.watch = { ...s.watch, known: null, candidate: null, stable: 0 };
    this.persist(s);
  }

  // Write meta.json (in order). Steps that rely on it (an upload, a release) await the
  // returned promise; it rejects when the write failed.
  private persist(s: Session): Promise<void> {
    const meta: CopyMeta = {
      v: 1, host: s.host, remotePath: s.remotePath, remoteVersion: s.remoteVersion,
      localVersion: s.localVersion, pending: s.pending, attempts: s.attempts, missing: s.state === "missing",
    };
    const write = () => this.deps.writeMeta(s.paths.dir, meta);
    const p = s.metaWrite.then(write, write);
    s.metaWrite = p;
    p.catch(() => {});
    return p;
  }

  // ── Watching ──

  private ensureTimers() {
    if (!this.poller) this.poller = setInterval(() => { void this.tick(); }, POLL_MS);
    if (!this.retrier) this.retrier = setInterval(() => this.retryWaiting(), RETRY_MS);
    if (!this.unsubUsable) this.unsubUsable = this.deps.onHostUsable(host => this.retryWaiting(host));
  }

  private stopTimersIfIdle() {
    if (this.sessions.size > 0) return;
    if (this.poller) clearInterval(this.poller);
    if (this.retrier) clearInterval(this.retrier);
    this.unsubUsable?.();
    this.poller = null;
    this.retrier = null;
    this.unsubUsable = null;
  }

  // One poll of every copy (exposed for tests).
  async tick(): Promise<void> {
    if (this.ticking) return;
    this.ticking = true;
    try {
      await Promise.all([...this.sessions.values()].map(s => this.pollOne(s)));
    } finally {
      this.ticking = false;
    }
  }

  private async pollOne(s: Session): Promise<void> {
    if (s.state === "downloading" || s.state === "released") return;
    let st: CopyStat | null;
    try {
      st = await this.deps.statCopy(s.paths.payload);
    } catch {
      return;
    }
    // It may have changed while the stat ran.
    const now = s.state as SyncState;
    if (now === "downloading" || now === "released") return;
    if (s.state === "missing") {
      if (st) {
        s.watch = freshWatch(null);
        this.toSettling(s);
        s.watch = watchStep(s.watch, st).w;
        this.emit();
      }
      return;
    }
    const { w, ev } = watchStep(s.watch, st);
    s.watch = w;
    if (ev === "missing") {
      if (!s.busy) {
        s.state = "missing";
        this.persist(s);
        this.emit();
      }
      return;
    }
    if (s.busy) {
      if (ev === "changed" && (s.state === "hashing" || s.state === "uploading")) s.dirtyAgain = true;
      return;
    }
    if (s.state === "clean" && w.candidate) {
      // A save: pending from now on (A5).
      s.state = "settling";
      s.pending = true;
      this.persist(s);
      this.emit();
    }
    if (ev !== "settled") return;
    if (s.state === "settling") void this.sync(s);
    else if (s.state === "conflict") void this.rehashInConflict(s);
  }

  // A save during a conflict: the copy's content is what Keep remote may discard.
  private async rehashInConflict(s: Session) {
    s.busy = true;
    const cand = s.watch.candidate;
    try {
      s.localVersion = await this.deps.hashCopy(s.paths.payload);
      s.watch = freshWatch(cand);
      this.persist(s);
    } catch {
      // Gone or unreadable: the next poll tells.
    } finally {
      s.busy = false;
    }
  }

  private retryWaiting(host?: HostId) {
    for (const s of this.sessions.values()) {
      if (host && s.host !== host) continue;
      if (s.busy) continue;
      if (s.state === "waitingHost" || s.state === "unknownOutcome") void this.sync(s);
    }
  }

  private kick(s: Session) {
    if (this.deps.hostUsable(s.host)) void this.sync(s);
  }

  // ── Uploading ──

  private async acquireUpload(): Promise<void> {
    if (this.uploads < MAX_UPLOADS) { this.uploads++; return; }
    await new Promise<void>(resolve => this.uploadQueue.push(resolve));
  }

  private releaseUpload() {
    const next = this.uploadQueue.shift();
    if (next) next();
    else this.uploads--;
  }

  // Snapshot the copy and upload it over the version it came from (or, after a doubt,
  // verify first). `forcedIfVersion`: Overwrite's fresh remote version.
  private async sync(s: Session, forcedIfVersion?: string): Promise<void> {
    if (s.busy) return;
    s.busy = true;
    const from = s.state;
    let snap: Snapshot | null = null;
    let slot = false;
    try {
      if (from !== "waitingHost" && from !== "unknownOutcome") s.state = "hashing";
      s.error = null;
      this.emit();
      // The recovery record must say "pending" before anything is uploaded (A5).
      s.pending = true;
      await this.persist(s);
      try {
        snap = await this.deps.snapshot(s.paths.payload);
      } catch (e) {
        const st = await this.deps.statCopy(s.paths.payload).catch(() => null);
        if (!st) {
          // Saved by rename and not back yet, or gone: the poller decides.
          if (s.state === "hashing") this.toSettling(s);
          return;
        }
        this.fail(s, e);
        return;
      }
      s.watch = freshWatch({ mtime_ms: snap.mtime_ms, size: snap.size });
      s.dirtyAgain = false;
      if (forcedIfVersion === undefined && !s.verifyFirst && snap.version === s.remoteVersion) {
        // Saved without changes (or changed back).
        s.localVersion = snap.version;
        s.pending = false;
        s.state = "clean";
        this.persist(s);
        return;
      }
      await this.acquireUpload();
      slot = true;
      let ifVersion: string | null = forcedIfVersion ?? s.remoteVersion;
      if (forcedIfVersion === undefined && s.verifyFirst) {
        s.checking = true;
        this.emit();
        const remote = await this.deps.remoteStat(s.host, s.remotePath);
        s.checking = false;
        const out = verifyOutcome(remote, snap.version, s.remoteVersion, s.attempts);
        if (out.kind === "conflict") {
          this.toConflict(s, snap.version);
          return;
        }
        if (out.kind === "done") {
          await this.succeeded(s, snap.version, snap.version);
          return;
        }
        ifVersion = out.ifVersion;
      }
      s.state = "uploading";
      s.attempts = [...s.attempts.filter(a => a !== snap!.version), snap.version].slice(-MAX_ATTEMPTS);
      s.pending = true;
      this.emit();
      await this.persist(s);
      const res = await this.deps.upload(s.host, this.remoteParent(s.host, s.remotePath), snap.path, s.name, ifVersion);
      await this.succeeded(s, res.version, snap.version);
    } catch (e) {
      s.checking = false;
      if (this.deps.isFileChanged(e)) {
        await this.afterChanged(s, snap);
      } else if (this.deps.isOutcomeUnknown(e)) {
        s.state = "unknownOutcome";
        s.verifyFirst = true;
        this.persist(s);
      } else if (this.deps.isHostUnavailable(e)) {
        s.state = from === "unknownOutcome" ? "unknownOutcome" : "waitingHost";
        s.verifyFirst = true;
        this.persist(s);
      } else {
        this.fail(s, e);
      }
    } finally {
      if (slot) this.releaseUpload();
      if (snap) await this.deps.dropFile(snap.path).catch(() => {});
      s.busy = false;
      this.emit();
      this.releaseIfOrphan(s);
    }
  }

  // The Host refused the upload as "changed". It may hold our own content already (a commit
  // from an earlier attempt that published late): check before calling it a conflict.
  private async afterChanged(s: Session, snap: Snapshot | null) {
    if (snap) {
      // This attempt was refused, so it never publishes.
      s.attempts = s.attempts.filter(a => a !== snap.version);
      try {
        const remote = await this.deps.remoteStat(s.host, s.remotePath);
        if (remote === snap.version) {
          await this.succeeded(s, remote, snap.version);
          return;
        }
        if (s.attempts.includes(remote)) {
          // An earlier save of ours published late: upload this newer one over it.
          s.remoteVersion = remote;
          s.attempts = s.attempts.filter(a => a !== remote);
          s.verifyFirst = false;
          this.toSettling(s);
          return;
        }
      } catch (e) {
        if (this.deps.isHostUnavailable(e)) {
          s.state = "waitingHost";
          s.verifyFirst = true;
          this.persist(s);
          return;
        }
      }
    }
    this.toConflict(s, snap?.version ?? s.localVersion);
  }

  private fail(s: Session, e: unknown) {
    s.state = "failed";
    s.error = messageOf(e);
    s.verifyFirst = true;
    s.pending = true;
    this.persist(s);
  }

  private toConflict(s: Session, local: string | null) {
    s.state = "conflict";
    s.localVersion = local;
    s.pending = true;
    this.persist(s);
  }

  // The Host has `remoteVersion` = the content of snapshot `uploaded`.
  private async succeeded(s: Session, remoteVersion: string, uploaded: string) {
    s.remoteVersion = remoteVersion;
    s.attempts = [];
    s.verifyFirst = false;
    const current = await this.deps.hashCopy(s.paths.payload).catch(() => null);
    if (current === uploaded && current === remoteVersion && !s.dirtyAgain) {
      s.localVersion = current;
      s.pending = false;
      s.state = "clean";
      this.persist(s);
      this.notify({ host: s.host, kind: "uploaded", name: s.name }, UPLOADED_NOTICE_MS);
    } else {
      // Saved again while it uploaded (A6): upload the newer content next.
      s.localVersion = uploaded;
      this.toSettling(s);
    }
  }

  // ── User actions ──

  async resolveConflict(key: string, choice: "overwrite" | "keepRemote"): Promise<void> {
    const s = this.sessions.get(key);
    if (!s || s.state !== "conflict" || s.busy) return;
    if (choice === "keepRemote") return this.replacePayload(s, s.localVersion, "keepRemote");
    // Overwrite: over the version the Host has now, never blindly (a third writer may race).
    s.busy = true;
    let remote: string;
    try {
      s.checking = true;
      this.emit();
      remote = await this.deps.remoteStat(s.host, s.remotePath);
    } catch (e) {
      s.busy = false;
      s.checking = false;
      if (this.deps.isHostUnavailable(e)) { s.state = "waitingHost"; s.verifyFirst = true; this.persist(s); }
      else this.fail(s, e);
      this.emit();
      return;
    }
    s.checking = false;
    s.busy = false;
    s.verifyFirst = false;
    await this.sync(s, remote);
  }

  async retry(key: string): Promise<void> {
    const s = this.sessions.get(key);
    if (!s || s.busy || (s.state !== "failed" && s.state !== "waitingHost" && s.state !== "unknownOutcome")) return;
    await this.sync(s);
  }

  // A12: fetch the remote file again for a copy that went missing.
  async downloadAgain(key: string): Promise<void> {
    const s = this.sessions.get(key);
    if (!s || s.state !== "missing" || s.busy) return;
    await this.replacePayload(s, null, "again");
  }

  // Close a missing copy: stop watching and delete what is left. Nothing is uploaded.
  async close(key: string): Promise<void> {
    const s = this.sessions.get(key);
    if (!s || s.busy || s.state !== "missing") return;
    await this.drop(s, "absent", true);
  }

  // Delete the copy, but only while its payload is `expect` (a version or "absent"). If it
  // changed (or came back), it is a save: settle and upload it instead. Unless `force` (the
  // user's Close), an open that comes while the meta write is awaited cancels the delete.
  private async drop(s: Session, expect: string, force = false) {
    s.busy = true;
    s.releasing = true;
    await s.metaWrite.catch(() => {});
    if (!force && (s.owners.size > 0 || this.sessions.get(s.key) !== s || s.state !== "clean" || s.pending)) {
      s.releasing = false;
      s.busy = false;
      this.emit();
      return;
    }
    let deleted!: () => void;
    s.deleting = new Promise<void>(r => { deleted = r; });
    try {
      await this.deps.release(s.paths.dir, { payload: s.paths.payload, expect });
    } catch (e) {
      s.releasing = false;
      s.deleting = null;
      s.busy = false;
      deleted();
      if (messageOf(e).startsWith(LOCAL_CHANGED)) this.toSettling(s);
      this.emit();
      return;
    }
    s.busy = false;
    s.state = "released";
    this.sessions.delete(s.key);
    deleted();
    this.stopTimersIfIdle();
    this.emit();
  }

  private releaseIfOrphan(s: Session) {
    if (s.owners.size === 0 && s.state === "clean" && !s.pending && !s.busy && this.sessions.get(s.key) === s) {
      void this.drop(s, s.localVersion ?? "absent");
    }
  }

  // An explorer went away: copies no other explorer opened are deleted once they are clean;
  // copies with changes stay until those are uploaded.
  releaseOwner(owner: string) {
    for (const s of [...this.sessions.values()]) {
      if (!s.owners.delete(owner)) continue;
      this.releaseIfOrphan(s);
    }
  }

  // Startup: resume copies with pending changes (A5). Each is checked against the Host
  // before anything is uploaded.
  async restorePending(metas: CopyMeta[]): Promise<void> {
    const perHost = new Map<HostId, number>();
    for (const m of metas) {
      if (!m?.pending || !m.host || !m.remotePath) continue;
      if (this.sessions.has(keyOf(m.host, m.remotePath))) continue;
      let paths: CopyPaths;
      try {
        paths = await this.deps.copyPaths(m.host, m.remotePath);
      } catch {
        continue;
      }
      if (this.sessions.has(keyOf(m.host, m.remotePath))) continue;
      this.restoreOne(m, paths);
      perHost.set(m.host, (perHost.get(m.host) ?? 0) + 1);
    }
    for (const [host, n] of perHost) this.notify({ host, kind: "restored", n });
    this.emit();
    for (const s of this.sessions.values()) if (s.state === "waitingHost" && s.verifyFirst && !s.busy) this.kick(s);
  }

  // A copy that went missing while it had no unsynced changes is not restored: there is
  // nothing to recover (its folder is swept after 7 days, and opening the file again starts
  // over). One with pending changes comes back as missing.
  private restoreOne(m: CopyMeta, paths: CopyPaths): Session {
    const s = this.newSession(m.host, m.remotePath, paths, m.missing ? "missing" : "waitingHost");
    s.remoteVersion = m.remoteVersion;
    s.localVersion = m.localVersion;
    s.attempts = Array.isArray(m.attempts) ? m.attempts.slice(-MAX_ATTEMPTS) : [];
    s.pending = true;
    s.verifyFirst = true;
    this.sessions.set(s.key, s);
    this.ensureTimers();
    return s;
  }
}
