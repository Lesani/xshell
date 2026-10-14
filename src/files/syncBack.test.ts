import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  LOCAL_CHANGED, MISSING_POLLS, POLL_MS, RETRY_MS, STABLE_POLLS, SyncManager, freshWatch, verifyOutcome, watchStep,
  type CopyMeta, type SyncDeps, type Watch,
} from "./syncBack";
import { joinPath } from "./paths";

// ── Pure parts ──

describe("watchStep", () => {
  const st = (mtime_ms: number, size = 1) => ({ mtime_ms, size });
  const run = (w: Watch, stats: ({ mtime_ms: number; size: number } | null)[]) => {
    const evs: string[] = [];
    for (const s of stats) { const r = watchStep(w, s); w = r.w; evs.push(r.ev); }
    return { w, evs };
  };

  it("an unchanged copy reports nothing", () => {
    expect(run(freshWatch(st(1)), [st(1), st(1)]).evs).toEqual(["none", "none"]);
  });

  it("a change settles after STABLE_POLLS equal polls", () => {
    const { evs } = run(freshWatch(st(1)), [st(2), st(2), st(2)]);
    expect(STABLE_POLLS).toBe(2);
    expect(evs).toEqual(["changed", "none", "settled"]);
  });

  it("a change that keeps changing does not settle", () => {
    expect(run(freshWatch(st(1)), [st(2), st(3), st(4, 2)]).evs).toEqual(["changed", "changed", "changed"]);
  });

  it("a one-poll gap (rename save) is not missing", () => {
    const { evs } = run(freshWatch(st(1)), [null, st(2), st(2), st(2)]);
    expect(evs).toEqual(["absent", "changed", "none", "settled"]);
  });

  it("a long absence reports missing from MISSING_POLLS on", () => {
    const { evs } = run(freshWatch(st(1)), Array(MISSING_POLLS + 1).fill(null));
    expect(evs.slice(0, MISSING_POLLS - 1).every(e => e === "absent")).toBe(true);
    expect(evs.slice(MISSING_POLLS - 1)).toEqual(["missing", "missing"]);
  });

  it("an unknown baseline makes the first stat a change", () => {
    expect(run(freshWatch(null), [st(1)]).evs).toEqual(["changed"]);
  });
});

describe("verifyOutcome", () => {
  it("the Host already has the content", () => {
    expect(verifyOutcome("s", "s", "b", [])).toEqual({ kind: "done" });
  });
  it("the Host still has the base", () => {
    expect(verifyOutcome("b", "s", "b", ["a"])).toEqual({ kind: "upload", ifVersion: "b" });
  });
  it("an earlier attempt landed and the copy changed again", () => {
    expect(verifyOutcome("a", "s", "b", ["a"])).toEqual({ kind: "upload", ifVersion: "a" });
    expect(verifyOutcome("a1", "s", "b", ["a1", "a2"])).toEqual({ kind: "upload", ifVersion: "a1" });
  });
  it("anything else is a conflict", () => {
    expect(verifyOutcome("x", "s", "b", ["a"])).toEqual({ kind: "conflict" });
    expect(verifyOutcome("x", "s", null, [])).toEqual({ kind: "conflict" });
  });
});

// ── The manager, against a fake machine and Host ──

class Offline extends Error { constructor() { super("offline"); this.name = "HostUnavailableError"; } }

interface Gate { promise: Promise<void>; open: () => void }
function gate(): Gate {
  let open!: () => void;
  const promise = new Promise<void>(r => { open = r; });
  return { promise, open };
}

const hash = (c: string) => `sha256:${c}`;
const HOST = "h_aaaaaaaa";
const PATH = "/srv/a.txt";

class World {
  local = new Map<string, { content: string; mtime: number }>();
  remote = new Map<string, string>();
  metas = new Map<string, CopyMeta>();
  opened: string[] = [];
  downloads = 0;
  uploads: { path: string; content: string; ifVersion: string | null }[] = [];
  uploadAttempts = 0;
  uploadDirs: string[] = [];
  inFlight = 0;
  maxInFlight = 0;
  online = true;
  clock = 1000;
  uploadGate: Gate | null = null;
  downloadGate: Gate | null = null;
  // The next commit lands (or not) and then gets no answer.
  outcomeUnknown: "landed" | "lost" | "late" | null = null;
  remoteError: string | null = null;
  usable: ((h: string) => void)[] = [];
  snaps = 0;
  // Runs right after an install moved the download in (an editor saving at once).
  afterInstall: (() => void) | null = null;
  // Runs in an upload right before the Host checks ifVersion (a late commit landing).
  beforeCommit: (() => void) | null = null;
  late: (() => void) | null = null;
  metaFails = false;
  metaGate: Gate | null = null;
  // Only writes this accepts wait for metaGate (all when unset).
  metaGateFor: ((meta: CopyMeta) => boolean) | null = null;

  constructor() { this.remote.set(PATH, "remote v0"); }

  pathsOf(host: string, rp: string) {
    const dir = `/cache/open/${host}${rp.replace(/\//g, "_")}`;
    const name = rp.split("/").pop()!;
    return { dir, payload: `${dir}/f/${name}`, incoming: `${dir}/dl/${name}` };
  }

  save(path: string, content: string) { this.local.set(path, { content, mtime: ++this.clock }); }
  touch(path: string) { const f = this.local.get(path)!; this.local.set(path, { content: f.content, mtime: ++this.clock }); }
  goOnline() { this.online = true; for (const cb of this.usable) cb(HOST); }
  filesUnder(dir: string) { return [...this.local.keys()].filter(k => k.startsWith(dir + "/")); }

  deps(): SyncDeps {
    const need = () => { if (!this.online) throw new Offline(); };
    return {
      copyPaths: async (host, rp) => this.pathsOf(host, rp),
      statCopy: async p => { const f = this.local.get(p); return f ? { mtime_ms: f.mtime, size: f.content.length } : null; },
      hashCopy: async p => { const f = this.local.get(p); if (!f) throw new Error("ENOENT"); return hash(f.content); },
      snapshot: async payload => {
        const f = this.local.get(payload);
        if (!f) throw new Error("ENOENT");
        const path = payload.replace(/\/f\/[^/]+$/, `/snap/${++this.snaps}`);
        this.local.set(path, { ...f });
        return { path, version: hash(f.content), size: f.content.length, mtime_ms: f.mtime };
      },
      dropFile: async p => { this.local.delete(p); },
      install: async (incoming, payload, expect) => {
        const cur = this.local.get(payload);
        if (cur && (expect === "absent" || hash(cur.content) !== expect)) {
          this.local.delete(incoming);
          throw `${LOCAL_CHANGED}: ${payload}`;
        }
        const f = this.local.get(incoming)!;
        this.local.set(payload, f);
        this.local.delete(incoming);
        this.afterInstall?.();
        return { mtime_ms: f.mtime, size: f.content.length };
      },
      download: async (_h, rp, lp, onProgress) => {
        need();
        this.downloads++;
        if (this.downloadGate) await this.downloadGate.promise;
        const content = this.remote.get(rp);
        if (content === undefined) throw "Failed to stat: No such file or directory (os error 2)";
        onProgress(content.length, content.length);
        this.local.set(lp, { content, mtime: ++this.clock });
        return { version: hash(content) };
      },
      upload: async (_h, dir, lp, name, ifVersion) => {
        need();
        this.uploadAttempts++;
        this.uploadDirs.push(dir);
        this.inFlight++;
        this.maxInFlight = Math.max(this.maxInFlight, this.inFlight);
        try {
          if (this.uploadGate) await this.uploadGate.promise;
          this.beforeCommit?.();
          if (this.remoteError) { const e = this.remoteError; this.remoteError = null; throw e; }
          const rp = joinPath(dir, name);
          const cur = this.remote.get(rp);
          const curV = cur === undefined ? "deleted" : hash(cur);
          if (ifVersion !== null && ifVersion !== curV) throw `file changed on the host: ${curV}`;
          const content = this.local.get(lp)!.content;
          if (this.outcomeUnknown) {
            const how = this.outcomeUnknown;
            this.outcomeUnknown = null;
            if (how === "landed") this.remote.set(rp, content);
            if (how === "late") this.late = () => this.remote.set(rp, content);
            throw "upload outcome unknown: no answer";
          }
          this.remote.set(rp, content);
          this.uploads.push({ path: rp, content, ifVersion });
          return { version: hash(content) };
        } finally {
          this.inFlight--;
        }
      },
      remoteStat: async (_h, p) => { need(); const c = this.remote.get(p); return c === undefined ? "deleted" : hash(c); },
      openLocal: async p => { this.opened.push(p); },
      readMeta: async dir => this.metas.get(dir) ?? null,
      writeMeta: async (dir, meta) => {
        if (this.metaGate && (!this.metaGateFor || this.metaGateFor(meta))) await this.metaGate.promise;
        if (this.metaFails) throw "No space left on device (os error 28)";
        this.metas.set(dir, { ...meta });
      },
      release: async (dir, guard) => {
        if (guard) {
          const cur = this.local.get(guard.payload);
          const ok = guard.expect === "absent" ? !cur : !!cur && hash(cur.content) === guard.expect;
          if (!ok) throw `${LOCAL_CHANGED}: ${guard.payload}`;
        }
        for (const k of this.filesUnder(dir)) this.local.delete(k);
        this.metas.delete(dir);
      },
      isHostUnavailable: e => e instanceof Offline,
      isFileChanged: e => String(e).startsWith("file changed on the host"),
      isOutcomeUnknown: e => String(e).startsWith("upload outcome unknown"),
      hostUsable: () => this.online,
      onHostUsable: cb => { this.usable.push(cb); return () => { this.usable = this.usable.filter(c => c !== cb); }; },
    };
  }
}

async function flush() { for (let i = 0; i < 30; i++) await Promise.resolve(); }
async function poll(n = 1) {
  for (let i = 0; i < n; i++) { await vi.advanceTimersByTimeAsync(POLL_MS); await flush(); }
}
const SETTLE = STABLE_POLLS + 1;

let w: World;
let m: SyncManager;
const P = () => w.pathsOf(HOST, PATH);
const meta = () => w.metas.get(P().dir);
const key = `${HOST}\u0000${PATH}`;
const view = () => m.getSnapshot().sessions.find(s => s.key === key);

beforeEach(() => {
  vi.useFakeTimers();
  w = new World();
  m = new SyncManager(w.deps());
});
afterEach(() => { vi.useRealTimers(); });

async function openClean() {
  await m.open(HOST, PATH, "o1");
  await flush();
  expect(m.state(HOST, PATH)).toBe("clean");
}

describe("SyncManager: open", () => {
  it("opens Remote: downloads, opens, state clean", async () => {
    await openClean();
    expect(w.downloads).toBe(1);
    expect(w.local.get(P().payload)?.content).toBe("remote v0");
    expect(w.opened).toEqual([P().payload]);
    expect(meta()).toMatchObject({ host: HOST, remotePath: PATH, remoteVersion: hash("remote v0"), localVersion: hash("remote v0"), pending: false });
  });

  it("opens Local: opens real path, no session", async () => {
    await m.open(undefined, "/home/u/notes.md", "o1");
    expect(w.opened).toEqual(["/home/u/notes.md"]);
    expect(w.downloads).toBe(0);
    expect(m.getSnapshot().sessions).toEqual([]);
  });

  it("a failed open leaves no session and says why", async () => {
    w.online = false;
    await m.open(HOST, PATH, "o1");
    expect(m.getSnapshot().sessions).toEqual([]);
    expect(m.getSnapshot().notices).toMatchObject([{ kind: "openFailed", host: HOST, name: "a.txt" }]);
    expect(w.filesUnder(P().dir)).toEqual([]);
  });

  it("two opens of same (host, path) share one session", async () => {
    await openClean();
    await m.open(HOST, PATH, "o2");
    await flush();
    expect(w.downloads).toBe(1);
    expect(w.opened).toEqual([P().payload, P().payload]);
    expect(m.getSnapshot().sessions).toHaveLength(1);
  });

  it("a double click while the first open downloads opens once", async () => {
    w.downloadGate = gate();
    const a = m.open(HOST, PATH, "o1");
    const b = m.open(HOST, PATH, "o2");
    await flush();
    w.downloadGate.open();
    await Promise.all([a, b]);
    expect(w.downloads).toBe(1);
    expect(w.opened).toEqual([P().payload]);
    m.releaseOwner("o1");
    await flush();
    expect(m.state(HOST, PATH)).toBe("clean"); // o2 still owns it
  });

  it("re-opening a clean copy whose remote file changed refreshes it", async () => {
    await openClean();
    w.remote.set(PATH, "remote v1");
    await m.open(HOST, PATH, "o1");
    await flush();
    expect(w.local.get(P().payload)?.content).toBe("remote v1");
    expect(m.state(HOST, PATH)).toBe("clean");
    expect(w.opened).toHaveLength(2);
  });

  it("A9: a refresh never replaces unseen local edits", async () => {
    await openClean();
    w.remote.set(PATH, "remote v1");
    w.save(P().payload, "local edit"); // saved, not polled yet
    await m.open(HOST, PATH, "o1");
    await flush();
    expect(w.local.get(P().payload)?.content).toBe("local edit");
    expect(m.state(HOST, PATH)).toBe("conflict");
    expect(meta()?.pending).toBe(true);
  });

  it("A9: a save while the refresh downloads keeps the save", async () => {
    await openClean();
    w.remote.set(PATH, "remote v1");
    w.downloadGate = gate();
    const reopening = m.open(HOST, PATH, "o1");
    await flush();
    expect(m.state(HOST, PATH)).toBe("downloading");
    w.save(P().payload, "local edit");
    w.downloadGate.open();
    await reopening;
    await flush();
    expect(w.local.get(P().payload)?.content).toBe("local edit");
    expect(m.state(HOST, PATH)).toBe("conflict");
    expect(w.filesUnder(`${P().dir}/dl`)).toEqual([]);
  });
});

describe("SyncManager: saves", () => {
  it("change settles after 2 stable polls then uploads", async () => {
    await openClean();
    w.save(P().payload, "v1");
    await poll(1);
    expect(m.state(HOST, PATH)).toBe("settling");
    expect(meta()?.pending).toBe(true); // A5: pending from the first sight of a change
    await poll(SETTLE - 1 - 1);
    expect(w.uploads).toHaveLength(0);
    await poll(1);
    expect(w.uploads).toEqual([{ path: PATH, content: "v1", ifVersion: hash("remote v0") }]);
    expect(m.state(HOST, PATH)).toBe("clean");
    expect(meta()).toMatchObject({ pending: false, remoteVersion: hash("v1"), localVersion: hash("v1"), attempts: [] });
    expect(w.filesUnder(`${P().dir}/snap`)).toEqual([]); // A6: the snapshot is dropped
    expect(m.getSnapshot().notices).toMatchObject([{ kind: "uploaded", name: "a.txt" }]);
  });

  it("missing file during rename-replace is not a delete", async () => {
    await openClean();
    w.local.delete(P().payload);
    await poll(1);
    expect(m.state(HOST, PATH)).toBe("clean");
    w.save(P().payload, "renamed in");
    await poll(SETTLE);
    expect(w.uploads.map(u => u.content)).toEqual(["renamed in"]);
    expect(w.remote.get(PATH)).toBe("renamed in");
  });

  it("unchanged hash after touch does not upload", async () => {
    await openClean();
    w.touch(P().payload);
    await poll(SETTLE + 2);
    expect(w.uploadAttempts).toBe(0);
    expect(m.state(HOST, PATH)).toBe("clean");
    expect(meta()?.pending).toBe(false);
  });

  it("upload sends ifVersion = last remote version", async () => {
    await openClean();
    w.save(P().payload, "v1");
    await poll(SETTLE);
    w.save(P().payload, "v2");
    await poll(SETTLE);
    expect(w.uploads.map(u => u.ifVersion)).toEqual([hash("remote v0"), hash("v1")]);
  });

  it("save during upload re-uploads after (A6: the snapshot is what goes up)", async () => {
    await openClean();
    w.uploadGate = gate();
    w.save(P().payload, "v1");
    await poll(SETTLE);
    expect(m.state(HOST, PATH)).toBe("uploading");
    w.save(P().payload, "v2");
    await poll(1);
    const g = w.uploadGate;
    w.uploadGate = null;
    g.open();
    await flush();
    expect(w.uploads.map(u => u.content)).toEqual(["v1"]);
    expect(m.state(HOST, PATH)).toBe("settling");
    expect(meta()?.pending).toBe(true);
    await poll(SETTLE + 1);
    expect(w.uploads.map(u => u.content)).toEqual(["v1", "v2"]);
    expect(w.uploads[1].ifVersion).toBe(hash("v1"));
    expect(m.state(HOST, PATH)).toBe("clean");
    expect(meta()?.pending).toBe(false);
  });

  it("a same-size, same-mtime change during the upload is caught by the hash", async () => {
    await openClean();
    w.uploadGate = gate();
    w.save(P().payload, "v1");
    await poll(SETTLE);
    const f = w.local.get(P().payload)!;
    w.local.set(P().payload, { content: "v9", mtime: f.mtime }); // same size, mtime kept
    const g = w.uploadGate;
    w.uploadGate = null;
    g.open();
    await flush();
    expect(m.state(HOST, PATH)).toBe("settling");
    await poll(SETTLE + 1);
    expect(w.remote.get(PATH)).toBe("v9");
  });

  it("at most 2 concurrent uploads", async () => {
    const paths = ["/srv/a.txt", "/srv/b.txt", "/srv/c.txt"];
    for (const p of paths) w.remote.set(p, "r");
    for (const p of paths) await m.open(HOST, p, "o1");
    await flush();
    w.uploadGate = gate();
    for (const p of paths) w.save(w.pathsOf(HOST, p).payload, `new ${p}`);
    await poll(SETTLE);
    expect(w.inFlight).toBe(2);
    const g = w.uploadGate;
    w.uploadGate = null;
    g.open();
    await poll(1);
    expect(w.maxInFlight).toBe(2);
    expect(w.uploads).toHaveLength(3);
    for (const p of paths) expect(m.state(HOST, p)).toBe("clean");
  });
});

describe("SyncManager: conflicts", () => {
  async function intoConflict() {
    await openClean();
    w.remote.set(PATH, "theirs");
    w.save(P().payload, "mine");
    await poll(SETTLE);
    expect(m.state(HOST, PATH)).toBe("conflict");
  }

  it("changed error → conflict, later saves do not upload", async () => {
    await intoConflict();
    expect(meta()?.pending).toBe(true);
    const attempts = w.uploadAttempts;
    w.save(P().payload, "mine 2");
    await poll(SETTLE + 3);
    expect(w.uploadAttempts).toBe(attempts);
    expect(w.remote.get(PATH)).toBe("theirs");
    expect(m.state(HOST, PATH)).toBe("conflict");
  });

  it("Overwrite re-stats and uploads with fresh version", async () => {
    await intoConflict();
    await m.resolveConflict(key, "overwrite");
    await flush();
    expect(w.uploads).toEqual([{ path: PATH, content: "mine", ifVersion: hash("theirs") }]);
    expect(m.state(HOST, PATH)).toBe("clean");
    expect(meta()?.pending).toBe(false);
  });

  it("Overwrite loses to a third writer instead of overwriting it", async () => {
    await intoConflict();
    w.uploadGate = gate();
    const p = m.resolveConflict(key, "overwrite");
    await flush();
    w.remote.set(PATH, "third");
    const g = w.uploadGate;
    w.uploadGate = null;
    g.open();
    await p;
    expect(w.remote.get(PATH)).toBe("third");
    expect(m.state(HOST, PATH)).toBe("conflict");
  });

  it("Keep remote re-downloads and re-opens", async () => {
    await intoConflict();
    await m.resolveConflict(key, "keepRemote");
    await flush();
    expect(w.local.get(P().payload)?.content).toBe("theirs");
    expect(w.opened).toHaveLength(2);
    expect(m.state(HOST, PATH)).toBe("clean");
    expect(meta()).toMatchObject({ pending: false, remoteVersion: hash("theirs") });
  });

  it("Keep remote takes a save seen during the conflict into account", async () => {
    await intoConflict();
    w.save(P().payload, "mine 2");
    await poll(SETTLE);
    await m.resolveConflict(key, "keepRemote");
    await flush();
    expect(w.local.get(P().payload)?.content).toBe("theirs");
  });

  it("A9: Keep remote with a save during the download keeps the save", async () => {
    await intoConflict();
    w.downloadGate = gate();
    const p = m.resolveConflict(key, "keepRemote");
    await flush();
    w.save(P().payload, "saved meanwhile");
    w.downloadGate.open();
    await p;
    expect(w.local.get(P().payload)?.content).toBe("saved meanwhile");
    expect(m.state(HOST, PATH)).toBe("conflict");
  });
});

describe("SyncManager: Host trouble", () => {
  it("offline → waitingHost, retries on host usable", async () => {
    await openClean();
    w.online = false;
    w.save(P().payload, "v1");
    await poll(SETTLE);
    expect(m.state(HOST, PATH)).toBe("waitingHost");
    expect(meta()?.pending).toBe(true);
    w.goOnline();
    await flush();
    expect(w.remote.get(PATH)).toBe("v1");
    expect(m.state(HOST, PATH)).toBe("clean");
  });

  it("offline → waitingHost, retries on the 15 s timer", async () => {
    await openClean();
    w.online = false;
    w.save(P().payload, "v1");
    await poll(SETTLE);
    w.online = true; // no usable event
    await vi.advanceTimersByTimeAsync(RETRY_MS);
    await flush();
    expect(w.remote.get(PATH)).toBe("v1");
    expect(m.state(HOST, PATH)).toBe("clean");
  });

  it("remote error → failed, Retry", async () => {
    await openClean();
    w.remoteError = "Permission denied (os error 13)";
    w.save(P().payload, "v1");
    await poll(SETTLE);
    expect(m.state(HOST, PATH)).toBe("failed");
    expect(view()?.error).toContain("Permission denied");
    await m.retry(key);
    await flush();
    expect(w.remote.get(PATH)).toBe("v1");
    expect(m.state(HOST, PATH)).toBe("clean");
  });

  it("A7: a commit that lands after the timeout counts as uploaded", async () => {
    await openClean();
    w.outcomeUnknown = "landed";
    w.save(P().payload, "v1");
    await poll(SETTLE);
    expect(m.state(HOST, PATH)).toBe("unknownOutcome");
    expect(meta()).toMatchObject({ pending: true, attempts: [hash("v1")] });
    const attempts = w.uploadAttempts;
    w.goOnline();
    await flush();
    expect(w.uploadAttempts).toBe(attempts); // checked, not uploaded again
    expect(m.state(HOST, PATH)).toBe("clean");
    expect(meta()).toMatchObject({ pending: false, remoteVersion: hash("v1"), attempts: [] });
  });

  it("A7: a commit that was lost is uploaded again over the old version", async () => {
    await openClean();
    w.outcomeUnknown = "lost";
    w.save(P().payload, "v1");
    await poll(SETTLE);
    w.goOnline();
    await flush();
    expect(w.uploads).toEqual([{ path: PATH, content: "v1", ifVersion: hash("remote v0") }]);
    expect(m.state(HOST, PATH)).toBe("clean");
  });

  it("A7: someone else's change meanwhile is a conflict", async () => {
    await openClean();
    w.outcomeUnknown = "lost";
    w.save(P().payload, "v1");
    await poll(SETTLE);
    w.remote.set(PATH, "theirs");
    w.goOnline();
    await flush();
    expect(m.state(HOST, PATH)).toBe("conflict");
    expect(w.remote.get(PATH)).toBe("theirs");
  });
});

describe("SyncManager: owners and cleanup", () => {
  it("last owner release deletes clean copy", async () => {
    await openClean();
    await m.open(HOST, PATH, "o2");
    await flush();
    m.releaseOwner("o1");
    await flush();
    expect(m.state(HOST, PATH)).toBe("clean");
    m.releaseOwner("o2");
    await flush();
    expect(m.state(HOST, PATH)).toBeNull();
    expect(w.filesUnder(P().dir)).toEqual([]);
    expect(meta()).toBeUndefined();
  });

  it("last owner release keeps pending; released after successful sync", async () => {
    await openClean();
    w.online = false;
    w.save(P().payload, "v1");
    await poll(SETTLE);
    m.releaseOwner("o1");
    await flush();
    expect(m.state(HOST, PATH)).toBe("waitingHost");
    expect(w.local.get(P().payload)?.content).toBe("v1");
    w.goOnline();
    await flush();
    expect(w.remote.get(PATH)).toBe("v1");
    expect(m.state(HOST, PATH)).toBeNull();
    expect(w.filesUnder(P().dir)).toEqual([]);
  });
});

describe("SyncManager: restart (A5)", () => {
  // A copy left by an earlier run: payload content, and the meta as it was written.
  function leftover(payload: string, metaPart: Partial<CopyMeta>) {
    w.save(P().payload, payload);
    const mt: CopyMeta = {
      v: 1, host: HOST, remotePath: PATH, remoteVersion: hash("remote v0"), localVersion: hash("remote v0"),
      pending: true, attempts: [], ...metaPart,
    };
    w.metas.set(P().dir, mt);
    return { ...mt, dir: P().dir };
  }

  it("restorePending resumes uploads (quit while settling or waiting)", async () => {
    const mt = leftover("edited", {});
    await m.restorePending([mt]);
    await flush();
    expect(m.getSnapshot().notices[0]).toMatchObject({ kind: "restored", host: HOST, n: 1 });
    expect(w.uploads).toEqual([{ path: PATH, content: "edited", ifVersion: hash("remote v0") }]);
    expect(m.state(HOST, PATH)).toBeNull(); // no explorer owns it: released once synced
  });

  it("waits for the Host before checking", async () => {
    w.online = false;
    const mt = leftover("edited", {});
    await m.restorePending([mt]);
    await flush();
    expect(m.state(HOST, PATH)).toBe("waitingHost");
    expect(w.uploadAttempts).toBe(0);
    w.goOnline();
    await flush();
    expect(w.remote.get(PATH)).toBe("edited");
  });

  it("quit while uploading, and the commit landed: nothing is uploaded again", async () => {
    w.remote.set(PATH, "edited");
    const mt = leftover("edited", { attempts: [hash("edited")] });
    await m.restorePending([mt]);
    await flush();
    expect(w.uploadAttempts).toBe(0);
    expect(m.state(HOST, PATH)).toBeNull();
  });

  it("quit while uploading, commit landed, then saved again: uploads over the attempt", async () => {
    w.remote.set(PATH, "edited");
    const mt = leftover("edited twice", { attempts: [hash("edited")] });
    await m.restorePending([mt]);
    await flush();
    expect(w.uploads).toEqual([{ path: PATH, content: "edited twice", ifVersion: hash("edited") }]);
  });

  it("quit in a conflict: still a conflict", async () => {
    w.remote.set(PATH, "theirs");
    const mt = leftover("mine", { localVersion: hash("mine") });
    await m.restorePending([mt]);
    await flush();
    expect(m.state(HOST, PATH)).toBe("conflict");
    expect(w.uploadAttempts).toBe(0);
  });

  it("quit with the copy missing: missing after 30 s, nothing uploaded", async () => {
    const mt = leftover("x", {});
    w.local.delete(P().payload);
    await m.restorePending([mt]);
    await flush();
    await poll(MISSING_POLLS);
    expect(m.state(HOST, PATH)).toBe("missing");
    expect(w.uploadAttempts).toBe(0);
    expect(meta()?.pending).toBe(true); // recovery metadata kept
  });

  it("restoring twice does not duplicate sessions", async () => {
    w.online = false;
    const mt = leftover("edited", {});
    await m.restorePending([mt]);
    await m.restorePending([mt]);
    expect(m.getSnapshot().sessions).toHaveLength(1);
  });
});

describe("SyncManager: a missing copy (A12)", () => {
  it("a long absence → missing; Download again restores it", async () => {
    await openClean();
    w.local.delete(P().payload);
    await poll(MISSING_POLLS - 1);
    expect(m.state(HOST, PATH)).toBe("clean");
    await poll(1);
    expect(m.state(HOST, PATH)).toBe("missing");
    expect(w.uploadAttempts).toBe(0);
    expect(w.remote.get(PATH)).toBe("remote v0"); // no deletion is uploaded
    await m.downloadAgain(key);
    await flush();
    expect(w.local.get(P().payload)?.content).toBe("remote v0");
    expect(m.state(HOST, PATH)).toBe("clean");
  });

  it("Close drops the copy", async () => {
    await openClean();
    w.local.delete(P().payload);
    await poll(MISSING_POLLS);
    await m.close(key);
    expect(m.state(HOST, PATH)).toBeNull();
    expect(meta()).toBeUndefined();
    expect(w.remote.get(PATH)).toBe("remote v0");
  });

  it("a copy that reappears returns to settling and uploads", async () => {
    await openClean();
    w.local.delete(P().payload);
    await poll(MISSING_POLLS);
    w.save(P().payload, "back");
    await poll(1);
    expect(m.state(HOST, PATH)).toBe("settling");
    await poll(SETTLE);
    expect(w.remote.get(PATH)).toBe("back");
  });
});

describe("SyncManager: review fixes", () => {
  it("1: closing the last explorer keeps a save it has not seen yet, uploads it, then releases", async () => {
    await openClean();
    w.save(P().payload, "unseen");
    m.releaseOwner("o1");
    await flush();
    expect(w.local.get(P().payload)?.content).toBe("unseen");
    expect(m.state(HOST, PATH)).toBe("settling");
    await poll(SETTLE + 1);
    expect(w.remote.get(PATH)).toBe("unseen");
    expect(m.state(HOST, PATH)).toBeNull();
    expect(w.filesUnder(P().dir)).toEqual([]);
  });

  it("1: Close on a missing copy that just came back keeps it", async () => {
    await openClean();
    w.local.delete(P().payload);
    await poll(MISSING_POLLS);
    expect(m.state(HOST, PATH)).toBe("missing");
    w.save(P().payload, "back"); // not polled yet
    await m.close(key);
    expect(w.local.get(P().payload)?.content).toBe("back");
    expect(m.state(HOST, PATH)).toBe("settling");
  });

  it("2: a save right after the install (same size and mtime) is not taken for the download", async () => {
    w.afterInstall = () => {
      const f = w.local.get(P().payload)!;
      w.local.set(P().payload, { content: "remote v9", mtime: f.mtime }); // same length as "remote v0"
      w.afterInstall = null;
    };
    await m.open(HOST, PATH, "o1");
    await flush();
    expect(m.state(HOST, PATH)).toBe("settling");
    expect(meta()?.pending).toBe(true);
    await poll(SETTLE + 1);
    expect(w.remote.get(PATH)).toBe("remote v9");
  });

  it("4: no upload starts before the pending record is on disk", async () => {
    await openClean();
    w.save(P().payload, "v1");
    await poll(SETTLE - 1);
    w.metaGate = gate();
    await poll(1); // settled: the sync waits for the meta write
    expect(w.uploadAttempts).toBe(0);
    const g = w.metaGate;
    w.metaGate = null;
    g.open();
    await flush();
    expect(w.uploads.map(u => u.content)).toEqual(["v1"]);
  });

  it("4: a failed meta write fails the sync without losing anything", async () => {
    await openClean();
    w.metaFails = true;
    w.save(P().payload, "v1");
    await poll(SETTLE);
    expect(m.state(HOST, PATH)).toBe("failed");
    expect(view()?.error).toContain("No space left");
    expect(w.uploadAttempts).toBe(0);
    expect(w.local.get(P().payload)?.content).toBe("v1");
    w.metaFails = false;
    await m.retry(key);
    await flush();
    expect(w.remote.get(PATH)).toBe("v1");
  });

  it("4: a leftover copy whose payload differs from its clean meta is resumed, not deleted", async () => {
    w.save(P().payload, "edited after the last sync");
    w.metas.set(P().dir, { v: 1, host: HOST, remotePath: PATH, remoteVersion: hash("remote v0"), localVersion: hash("remote v0"), pending: false, attempts: [] });
    await m.open(HOST, PATH, "o1");
    await flush();
    expect(w.downloads).toBe(0);
    expect(w.remote.get(PATH)).toBe("edited after the last sync");
  });

  it("4: a leftover copy that matches its clean meta starts over", async () => {
    w.save(P().payload, "remote v0");
    w.remote.set(PATH, "remote v1");
    w.metas.set(P().dir, { v: 1, host: HOST, remotePath: PATH, remoteVersion: hash("remote v0"), localVersion: hash("remote v0"), pending: false, attempts: [] });
    await m.open(HOST, PATH, "o1");
    await flush();
    expect(w.downloads).toBe(1);
    expect(w.local.get(P().payload)?.content).toBe("remote v1");
    expect(m.state(HOST, PATH)).toBe("clean");
  });

  it("6: a POSIX path with a backslash in the name round-trips", async () => {
    const odd = "/srv/a\\b.txt";
    w.remote.set(odd, "r");
    await m.open(HOST, odd, "o1");
    await flush();
    expect(m.getSnapshot().sessions[0].name).toBe("a\\b.txt");
    w.save(w.pathsOf(HOST, odd).payload, "edited");
    await poll(SETTLE);
    expect(w.uploads).toEqual([{ path: odd, content: "edited", ifVersion: hash("r") }]);
  });

  it("6: a Windows Remote Host's path round-trips (inferred from the path)", async () => {
    const win = "C:\\Users\\me\\a.txt";
    w.remote.set(win, "r");
    await m.open(HOST, win, "o1");
    await flush();
    expect(m.getSnapshot().sessions[0].name).toBe("a.txt");
    w.save(w.pathsOf(HOST, win).payload, "edited");
    await poll(SETTLE);
    expect(w.uploads).toEqual([{ path: win, content: "edited", ifVersion: hash("r") }]);
    expect(w.uploadDirs).toEqual(["C:\\Users\\me"]);
  });

  it("6: the Host's known OS decides when the path's form does not", async () => {
    const deps = w.deps();
    const m2 = new SyncManager({ ...deps, hostPathStyle: () => "posix" });
    const odd = "/srv/a\\b.txt";
    w.remote.set(odd, "r");
    await m2.open(HOST, odd, "o1");
    await flush();
    expect(m2.getSnapshot().sessions[0].name).toBe("a\\b.txt");
  });

  it("7: a recovery upload refused because the original commit published late is a success", async () => {
    await openClean();
    w.outcomeUnknown = "late";
    w.save(P().payload, "v1");
    await poll(SETTLE);
    expect(m.state(HOST, PATH)).toBe("unknownOutcome");
    // The check sees the old version; the late commit lands right before the retry commits.
    w.beforeCommit = () => { w.late?.(); w.beforeCommit = null; };
    w.goOnline();
    await flush();
    expect(w.remote.get(PATH)).toBe("v1");
    expect(m.state(HOST, PATH)).toBe("clean");
    expect(meta()).toMatchObject({ pending: false, remoteVersion: hash("v1") });
  });

  it("8: a copy in conflict can go missing, and a restart restores it as missing", async () => {
    await openClean();
    w.remote.set(PATH, "theirs");
    w.save(P().payload, "mine");
    await poll(SETTLE);
    expect(m.state(HOST, PATH)).toBe("conflict");
    w.local.delete(P().payload);
    await poll(MISSING_POLLS);
    expect(m.state(HOST, PATH)).toBe("missing");
    expect(meta()).toMatchObject({ pending: true, missing: true });
    const m2 = new SyncManager(w.deps());
    await m2.restorePending([{ ...meta()!, dir: P().dir }]);
    expect(m2.state(HOST, PATH)).toBe("missing");
    const downloads = w.downloads;
    await m2.downloadAgain(key);
    await flush();
    expect(w.downloads).toBe(downloads + 1);
    // No explorer owns it after the restart: once clean again it is released.
    expect(m2.state(HOST, PATH)).toBeNull();
    expect(w.uploadAttempts).toBe(1); // only the original, refused one
    expect(w.remote.get(PATH)).toBe("theirs");
  });
});

describe("SyncManager: re-review fixes", () => {
  it("B: an open while the release waits for the clean meta write keeps the copy", async () => {
    w.metaGate = gate();
    w.metaGateFor = mt => !mt.pending; // hold the "clean" write after the download
    await m.open(HOST, PATH, "o1");
    await flush();
    expect(m.state(HOST, PATH)).toBe("clean");
    m.releaseOwner("o1"); // the release waits for the held write
    await flush();
    await m.open(HOST, PATH, "o2");
    await flush();
    expect(w.opened).toHaveLength(2);
    const g = w.metaGate;
    w.metaGate = null;
    g.open();
    await flush();
    expect(m.state(HOST, PATH)).toBe("clean");
    expect(w.local.get(P().payload)?.content).toBe("remote v0");
    // o2 owns it now; its release deletes it.
    m.releaseOwner("o2");
    await flush();
    expect(m.state(HOST, PATH)).toBeNull();
    expect(w.filesUnder(P().dir)).toEqual([]);
  });

  it("B: an open after the point of no return waits for the delete and opens afresh", async () => {
    const releaseGate = gate();
    const deps = w.deps();
    const m2 = new SyncManager({ ...deps, release: async (dir, guard) => { await releaseGate.promise; return deps.release(dir, guard); } });
    await m2.open(HOST, PATH, "o1");
    await flush();
    m2.releaseOwner("o1");
    await flush();
    const reopening = m2.open(HOST, PATH, "o2");
    await flush();
    expect(w.opened).toHaveLength(1); // waits for the delete
    releaseGate.open();
    await reopening;
    await flush();
    expect(w.downloads).toBe(2);
    expect(w.opened).toHaveLength(2);
    expect(m2.state(HOST, PATH)).toBe("clean");
    expect(w.local.get(P().payload)?.content).toBe("remote v0");
  });

  it("C: a late earlier attempt plus a newer save ends with the newer save, clean", async () => {
    await openClean();
    w.outcomeUnknown = "late";
    w.save(P().payload, "v1");
    await poll(SETTLE);
    expect(m.state(HOST, PATH)).toBe("unknownOutcome");
    w.save(P().payload, "v2"); // a newer save while the outcome is unknown
    await poll(SETTLE);
    // The check sees the base; v1 publishes right before the v2 retry commits.
    w.beforeCommit = () => { w.late?.(); w.beforeCommit = null; };
    w.goOnline();
    await flush();
    expect(w.remote.get(PATH)).toBe("v1");
    expect(m.state(HOST, PATH)).toBe("settling");
    await poll(SETTLE + 1);
    expect(w.remote.get(PATH)).toBe("v2");
    expect(w.uploads[w.uploads.length - 1]).toEqual({ path: PATH, content: "v2", ifVersion: hash("v1") });
    expect(m.state(HOST, PATH)).toBe("clean");
    expect(meta()).toMatchObject({ pending: false, attempts: [] });
  });

  it("A: an untrusted leftover from the startup list is checked and uploaded", async () => {
    // open_copies_pending reports a copy whose payload differs from its "clean" meta.
    w.save(P().payload, "edited, pending write failed");
    const mt: CopyMeta = { v: 1, host: HOST, remotePath: PATH, remoteVersion: hash("remote v0"), localVersion: hash("remote v0"), pending: true, attempts: [], dir: P().dir };
    await m.restorePending([mt]);
    await flush();
    expect(w.remote.get(PATH)).toBe("edited, pending write failed");
  });
});
