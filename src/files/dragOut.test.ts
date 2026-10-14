import { beforeEach, describe, expect, it, vi } from "vitest";
import { DRAG_CACHE_MS, DRAG_DISABLED_MS, DRAG_OUT_MAX_BYTES, DragOut, type DragOutDeps, type DragOutError } from "./dragOut";

const H = "h_ab12cd34";
const flush = async () => { for (let i = 0; i < 10; i++) await Promise.resolve(); };
const deferred = <T,>() => { let resolve!: (v: T) => void, reject!: (e: unknown) => void; const p = new Promise<T>((a, b) => { resolve = a; reject = b; }); return { p, resolve, reject }; };

interface Fx {
  d: DragOut;
  deps: { [K in keyof DragOutDeps]: ReturnType<typeof vi.fn> };
  timers: { fn: () => void; ms: number; live: boolean }[];
  starts: { paths: string[]; end: (dropped: boolean) => void; fail: (e: unknown) => void }[];
  dl: ReturnType<typeof deferred<{ version: string }>>[];
  size: number;
  version: string;
  clock: number;
}

function fx(): Fx {
  const f = { timers: [], starts: [], dl: [], size: 10, version: "sha256:v1", clock: 0 } as unknown as Fx;
  const deps = {
    stat: vi.fn(async (_h: string, _p: string, withVersion: boolean) => ({ size: f.size, is_dir: false, version: withVersion ? f.version : null })),
    preparePath: vi.fn(async (_h: string, p: string) => `/cache/drag/k/${p.split("/").pop()}`),
    download: vi.fn((_h: string, _p: string, _l: string, onProgress: (d: number, t: number) => void) => {
      const dd = deferred<{ version: string }>();
      onProgress(5, 10);
      f.dl.push(dd);
      return dd.p;
    }),
    release: vi.fn(async () => {}),
    start: vi.fn((paths: string[]) => new Promise<boolean>((end, fail) => f.starts.push({ paths, end, fail }))),
    now: vi.fn(() => f.clock),
    setTimeout: vi.fn((fn: () => void, ms: number) => { const t = { fn, ms, live: true }; f.timers.push(t); return t; }),
    clearTimeout: vi.fn((t: { live: boolean }) => { t.live = false; }),
  };
  f.deps = deps as unknown as Fx["deps"];
  f.d = new DragOut(deps as unknown as DragOutDeps);
  return f;
}

let f: Fx;
beforeEach(() => { f = fx(); });

describe("Local Host", () => {
  it("under the threshold nothing is dragged", () => {
    f.d.down(undefined, "/home/u/a.txt", false, 100, 100);
    f.d.move(102, 102);
    f.d.up();
    expect(f.deps.start).not.toHaveBeenCalled();
  });

  it("a held press moved 4 px drags the real path; drops are refused meanwhile", async () => {
    f.d.down(undefined, "/home/u/a.txt", false, 100, 100);
    expect(f.d.held).toBe(true);
    f.d.move(104, 100);
    expect(f.deps.start).toHaveBeenCalledWith(["/home/u/a.txt"]);
    expect(f.d.active).toBe(true);
    expect(f.d.grip(undefined, "/home/u/a.txt", false)).toEqual({ kind: "dragging" });
    f.starts[0].end(true);
    await flush();
    expect(f.d.active).toBe(false);
    expect(f.d.held).toBe(false);
    expect(f.d.grip(undefined, "/home/u/a.txt", false)).toEqual({ kind: "ready" });
  });

  it("folders drag their real path too", () => {
    f.d.down(undefined, "/home/u/src", true, 0, 0);
    f.d.move(10, 0);
    expect(f.deps.start).toHaveBeenCalledWith(["/home/u/src"]);
  });

  it("released before the move: no drag", () => {
    f.d.down(undefined, "/home/u/a.txt", false, 0, 0);
    f.d.up();
    f.d.move(50, 0);
    expect(f.deps.start).not.toHaveBeenCalled();
  });

  it("a drag that cannot start reports it", async () => {
    const errors: DragOutError[] = [];
    f.d.down(undefined, "/home/u/a.txt", false, 0, 0, (e) => errors.push(e));
    f.d.move(10, 0);
    f.starts[0].fail(new Error("no seat"));
    await flush();
    expect(errors).toEqual([{ kind: "start", error: new Error("no seat") }]);
    expect(f.d.active).toBe(false);
  });
});

describe("Remote Host", () => {
  it("folders and files over the cap are disabled", async () => {
    expect(f.d.grip(H, "/srv/src", true)).toEqual({ kind: "disabled", reason: "folderRemote" });
    f.size = DRAG_OUT_MAX_BYTES + 1;
    await f.d.probe(H, "/srv/big.iso", false);
    expect(f.d.grip(H, "/srv/big.iso", false)).toEqual({ kind: "disabled", reason: "tooLarge" });
    f.d.down(H, "/srv/big.iso", false, 0, 0);
    f.d.move(10, 0);
    await flush();
    expect(f.deps.download).not.toHaveBeenCalled();
    expect(f.deps.start).not.toHaveBeenCalled();
  });

  it("a file just at the cap is fine", async () => {
    f.size = DRAG_OUT_MAX_BYTES;
    await f.d.probe(H, "/srv/a.bin", false);
    expect(f.d.grip(H, "/srv/a.bin", false)).toEqual({ kind: "ready" });
  });

  it("over the cap found at the press: disabled, no download", async () => {
    f.size = DRAG_OUT_MAX_BYTES + 1;
    f.d.down(H, "/srv/big.iso", false, 0, 0);
    await flush();
    expect(f.d.grip(H, "/srv/big.iso", false)).toEqual({ kind: "disabled", reason: "tooLarge" });
    expect(f.deps.download).not.toHaveBeenCalled();
    expect(f.d.held).toBe(false);
  });

  it("prepares with progress, then starts only while still held", async () => {
    f.d.down(H, "/srv/a.txt", false, 0, 0);
    f.d.move(10, 0);
    await flush();
    expect(f.deps.preparePath).toHaveBeenCalledWith(H, "/srv/a.txt");
    expect(f.deps.download).toHaveBeenCalledWith(H, "/srv/a.txt", "/cache/drag/k/a.txt", expect.any(Function));
    expect(f.d.grip(H, "/srv/a.txt", false)).toEqual({ kind: "preparing", pct: 50 });
    expect(f.deps.start).not.toHaveBeenCalled();
    f.dl[0].resolve({ version: "sha256:v1" });
    await flush();
    expect(f.deps.start).toHaveBeenCalledWith(["/cache/drag/k/a.txt"]);
  });

  it("released early: the copy finishes and is cached; the next drag starts at once", async () => {
    f.d.down(H, "/srv/a.txt", false, 0, 0);
    f.d.move(10, 0);
    await flush();
    f.d.up();
    f.dl[0].resolve({ version: "sha256:v1" });
    await flush();
    expect(f.deps.start).not.toHaveBeenCalled();
    expect(f.d.grip(H, "/srv/a.txt", false)).toEqual({ kind: "ready" });
    // Again: the same version is on the Host, so no download.
    f.d.down(H, "/srv/a.txt", false, 0, 0);
    f.d.move(10, 0);
    await flush();
    expect(f.deps.download).toHaveBeenCalledTimes(1);
    expect(f.deps.start).toHaveBeenCalledWith(["/cache/drag/k/a.txt"]);
  });

  it("a changed version downloads again", async () => {
    f.d.down(H, "/srv/a.txt", false, 0, 0);
    await flush();
    f.d.up();
    f.dl[0].resolve({ version: "sha256:v1" });
    await flush();
    f.version = "sha256:v2";
    f.d.down(H, "/srv/a.txt", false, 0, 0);
    await flush();
    expect(f.deps.download).toHaveBeenCalledTimes(2);
  });

  it("the cached copy is released after 10 minutes, but not during its drag", async () => {
    f.d.down(H, "/srv/a.txt", false, 0, 0);
    f.d.move(10, 0);
    await flush();
    f.dl[0].resolve({ version: "sha256:v1" });
    await flush();
    const first = f.timers.filter((t) => t.live);
    expect(first.map((t) => t.ms)).toEqual([DRAG_CACHE_MS]);
    // Expires while the drag still runs: kept, timer restarted.
    first[0].fn();
    expect(f.deps.release).not.toHaveBeenCalled();
    f.starts[0].end(true);
    await flush();
    const live = f.timers.filter((t) => t.live);
    expect(live.length).toBe(1);
    live[0].fn();
    expect(f.deps.release).toHaveBeenCalledWith("/cache/drag/k/a.txt");
    // Gone from the cache: the next drag downloads again.
    f.d.down(H, "/srv/a.txt", false, 0, 0);
    await flush();
    expect(f.deps.download).toHaveBeenCalledTimes(2);
  });

  it("a failed preparation while held is reported", async () => {
    const errors: DragOutError[] = [];
    f.d.down(H, "/srv/a.txt", false, 0, 0, (e) => errors.push(e));
    await flush();
    f.dl[0].reject(new Error("offline"));
    await flush();
    expect(errors).toEqual([{ kind: "prepare", path: "/srv/a.txt", error: new Error("offline") }]);
    expect(f.d.grip(H, "/srv/a.txt", false)).toEqual({ kind: "ready" });
    expect(f.d.held).toBe(false);
  });

  it("press, release and press again during preparation share one download", async () => {
    f.d.down(H, "/srv/a.txt", false, 0, 0);
    f.d.move(10, 0);
    await flush();
    f.d.up();
    // The file changes on the Host meanwhile; the second press joins the preparation.
    f.version = "sha256:v2";
    f.d.down(H, "/srv/a.txt", false, 0, 0);
    f.d.move(10, 0);
    await flush();
    expect(f.deps.download).toHaveBeenCalledTimes(1);
    f.dl[0].resolve({ version: "sha256:v1" });
    await flush();
    expect(f.deps.start).toHaveBeenCalledTimes(1);
    expect(f.deps.start).toHaveBeenCalledWith(["/cache/drag/k/a.txt"]);
    // Under the running drag, a later version is never downloaded over the copy, and the
    // copy is never released.
    expect(f.d.active).toBe(true);
    for (const t of f.timers.filter((x) => x.live)) t.fn();
    expect(f.deps.release).not.toHaveBeenCalled();
    f.starts[0].end(true);
    await flush();
    f.d.down(H, "/srv/a.txt", false, 0, 0);
    await flush();
    expect(f.deps.download).toHaveBeenCalledTimes(2);
    f.dl[1].resolve({ version: "sha256:v2" });
    await flush();
  });

  it("an older completion never publishes over a newer one", async () => {
    // First preparation (v1) is slow; it is the only one in flight, so a second press waits
    // for it rather than racing it.
    f.d.down(H, "/srv/a.txt", false, 0, 0);
    await flush();
    f.d.up();
    f.d.down(H, "/srv/a.txt", false, 0, 0);
    await flush();
    f.d.up();
    expect(f.dl.length).toBe(1);
    f.dl[0].resolve({ version: "sha256:v1" });
    await flush();
    // A press now with v1 still current is a cache hit.
    f.d.down(H, "/srv/a.txt", false, 0, 0);
    f.d.move(10, 0);
    await flush();
    expect(f.deps.download).toHaveBeenCalledTimes(1);
    expect(f.deps.start).toHaveBeenCalledWith(["/cache/drag/k/a.txt"]);
  });

  it("a file found too large is checked again later and on a press", async () => {
    f.size = DRAG_OUT_MAX_BYTES + 1;
    await f.d.probe(H, "/srv/f.bin", false);
    expect(f.d.grip(H, "/srv/f.bin", false)).toMatchObject({ kind: "disabled", reason: "tooLarge" });
    f.size = 10;
    // A fresh result is not re-checked on hover…
    await f.d.probe(H, "/srv/f.bin", false);
    expect(f.d.grip(H, "/srv/f.bin", false)).toMatchObject({ kind: "disabled" });
    // …but after DRAG_DISABLED_MS it is.
    f.clock += DRAG_DISABLED_MS;
    await f.d.probe(H, "/srv/f.bin", false);
    expect(f.d.grip(H, "/srv/f.bin", false)).toEqual({ kind: "ready" });
    // A press always re-checks.
    f.size = DRAG_OUT_MAX_BYTES + 1;
    await f.d.probe(H, "/srv/g.bin", false);
    f.size = 10;
    f.d.down(H, "/srv/g.bin", false, 0, 0);
    f.d.move(10, 0);
    await flush();
    expect(f.deps.download).toHaveBeenCalledTimes(1);
    f.dl[0].resolve({ version: "sha256:v1" });
    await flush();
    expect(f.deps.start).toHaveBeenCalledWith(["/cache/drag/k/g.bin"]);
  });
});
