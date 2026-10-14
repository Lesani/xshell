import { describe, expect, it, vi } from "vitest";
import { downloadTo, reserveUniqueDir, type DownloadToDeps } from "./downloadTo";

// A local disk of reserved folders: reserveDir is exclusive, like transfer_reserve_dir.
function disk(existing: string[] = []) {
  const have = new Set(existing);
  return { have, reserve: vi.fn(async (p: string) => { if (have.has(p)) return false; have.add(p); return true; }) };
}

function deps(over: Partial<DownloadToDeps> = {}, existing: string[] = []): DownloadToDeps & { have: Set<string> } {
  const d = disk(existing);
  return {
    have: d.have,
    saveDialog: vi.fn(async (name: string) => `/home/u/Downloads/${name}`),
    folderDialog: vi.fn(async () => "/home/u/Downloads"),
    reserveDir: d.reserve,
    downloadFile: vi.fn(async (_r: string, _l: string, onProgress: (d: number, t: number) => void) => { onProgress(5, 10); return {}; }),
    downloadTree: vi.fn(async () => ({ files: 3, skipped: [] })),
    ...over,
  };
}

describe("reserveUniqueDir", () => {
  it("takes the name, else the first (n) suffix it can create", async () => {
    expect(await reserveUniqueDir("/d", "src", disk().reserve)).toBe("/d/src");
    expect(await reserveUniqueDir("/d", "src", disk(["/d/src"]).reserve)).toBe("/d/src (2)");
    expect(await reserveUniqueDir("/d", "src", disk(["/d/src", "/d/src (2)"]).reserve)).toBe("/d/src (3)");
    expect(await reserveUniqueDir("C:\\Users\\u", "src", disk(["C:\\Users\\u\\src"]).reserve)).toBe("C:\\Users\\u\\src (2)");
  });

  it("two concurrent downloads of `src` get two folders, never one merged", async () => {
    const d = disk();
    const [a, b] = await Promise.all([reserveUniqueDir("/d", "src", d.reserve), reserveUniqueDir("/d", "src", d.reserve)]);
    expect(new Set([a, b])).toEqual(new Set(["/d/src", "/d/src (2)"]));
  });
});

describe("downloadTo", () => {
  it("a file goes where the Save dialog says, with progress", async () => {
    const d = deps();
    const progress: unknown[] = [];
    const r = await downloadTo({ path: "/srv/a.txt", name: "a.txt", isDir: false }, d, { progress: (...a) => progress.push(a) });
    expect(d.saveDialog).toHaveBeenCalledWith("a.txt");
    expect(d.downloadFile).toHaveBeenCalledWith("/srv/a.txt", "/home/u/Downloads/a.txt", expect.any(Function), undefined);
    expect(r).toEqual({ path: "/home/u/Downloads/a.txt", files: 1, skipped: [] });
    expect(progress).toEqual([[1, 1, "a.txt", 0], [1, 1, "a.txt", 50]]);
  });

  it("a cancelled dialog downloads nothing", async () => {
    const d = deps({ saveDialog: vi.fn(async () => null), folderDialog: vi.fn(async () => null) });
    expect(await downloadTo({ path: "/srv/a.txt", name: "a.txt", isDir: false }, d)).toBeNull();
    expect(await downloadTo({ path: "/srv/src", name: "src", isDir: true }, d)).toBeNull();
    expect(d.downloadFile).not.toHaveBeenCalled();
    expect(d.downloadTree).not.toHaveBeenCalled();
  });

  it("a folder recurses into a reserved <picked>/<name>, or <name> (2) when taken", async () => {
    const d = deps({}, ["/home/u/Downloads/src"]);
    const ac = new AbortController();
    const listing: unknown[] = [];
    const r = await downloadTo({ path: "/srv/src", name: "src", isDir: true }, d, { listing: (...a) => listing.push(a) }, ac.signal);
    expect(d.downloadTree).toHaveBeenCalledWith("/srv/src", "/home/u/Downloads/src (2)", expect.any(Function), ac.signal, expect.any(Function));
    expect(r).toEqual({ path: "/home/u/Downloads/src (2)", files: 3, skipped: [] });
    // The listing state shows before the walk.
    expect(listing[0]).toEqual(["src", 0]);
  });

  it("a folder name this machine cannot hold creates nothing", async () => {
    const d = deps({ folderDialog: vi.fn(async () => "C:\\Downloads") });
    for (const name of ["..\\notes", "a:b", "CON", "..", "x."]) {
      await expect(downloadTo({ path: `/srv/${name}`, name, isDir: true }, d)).rejects.toThrow("not a valid folder name");
    }
    const posix = deps();
    await expect(downloadTo({ path: "/srv/..", name: "..", isDir: true }, posix)).rejects.toThrow();
    expect(d.reserveDir).not.toHaveBeenCalled();
    expect(posix.reserveDir).not.toHaveBeenCalled();
    // A backslash is an ordinary character on a POSIX destination.
    await expect(downloadTo({ path: "/srv/a\\b", name: "a\\b", isDir: true }, posix)).resolves.toMatchObject({ path: "/home/u/Downloads/a\\b" });
  });

  it("folder progress names the current file and counts from 1", async () => {
    const d = deps({
      downloadTree: vi.fn(async (_r, _l, onProgress) => {
        onProgress(0, 2, "/srv/src/a.txt", 0.5);
        onProgress(1, 2, "/srv/src/sub/b.txt", 0);
        onProgress(2, 2, "", 1);
        return { files: 2, skipped: [] };
      }),
    });
    const progress: unknown[] = [];
    await downloadTo({ path: "/srv/src", name: "src", isDir: true }, d, { progress: (...a) => progress.push(a) });
    expect(progress).toEqual([[1, 2, "a.txt", 50], [2, 2, "b.txt", 0]]);
  });

  it("errors from the download reject", async () => {
    const d = deps({ downloadFile: vi.fn(async () => { throw new Error("boom"); }) });
    await expect(downloadTo({ path: "/srv/a", name: "a", isDir: false }, d)).rejects.toThrow("boom");
  });
});
