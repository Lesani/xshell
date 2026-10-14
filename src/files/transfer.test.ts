import { beforeEach, describe, expect, it, vi } from "vitest";
import transferRs from "../../src-tauri/crates/core/src/transfer.rs?raw";

// Channel stand-in: tests push messages with `.emit()`.
const { FakeChannel } = vi.hoisted(() => {
  class FakeChannel<T> {
    static all: FakeChannel<unknown>[] = [];
    onmessage: (m: T) => void = () => {};
    constructor() { FakeChannel.all.push(this as FakeChannel<unknown>); }
    emit(m: T) { this.onmessage(m); }
  }
  return { FakeChannel };
});
const invoke = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...a: unknown[]) => invoke(...a), Channel: FakeChannel }));
vi.mock("@tauri-apps/plugin-store", () => ({ load: vi.fn(async () => ({ get: async () => undefined, set: async () => {}, save: async () => {} })) }));

import {
  CHUNK, FILE_CHANGED, FILE_EXISTS, UPLOAD_OUTCOME_UNKNOWN, UPLOAD_UNKNOWN, TRANSFER_CANCELLED,
  bytesToBase64, download, isFileChanged, isFileExists, isUploadOutcomeUnknown, joinPath, upload,
  uploadEntries, commitAgain, UploadPendingError, type UploadEntry,
  entriesFromDrop, downloadTree, TreeTooLargeError, TransferAbortedError, TREE_MAX_ENTRIES,
} from "./transfer";
import { HostUnavailableError } from "../hosts/hostInvoke";

const H = "h_ab12cd34";
const calls = (cmd: string) => invoke.mock.calls.filter(c => c[0] === cmd);
// The Host method a call reached, local (`invoke(method, args)`) or remote (`host_call`).
const methodOf = (c: unknown[]) => (c[0] === "host_call" ? (c[1] as { method: string }).method : (c[0] as string));
const argsOf = (c: unknown[]) => (c[0] === "host_call" ? (c[1] as { params: Record<string, unknown> }).params : (c[1] as Record<string, unknown>));
const deferred = <T,>() => { let resolve!: (v: T) => void, reject!: (e: unknown) => void; const p = new Promise<T>((a, b) => { resolve = a; reject = b; }); return { p, resolve, reject }; };
// A slow, obviously correct encoder to check against.
const refBase64 = (b: Uint8Array) => btoa(Array.from(b, c => String.fromCharCode(c)).join(""));
const done = { path: "/d/a", size: 1, version: "sha256:00" };

/** A Host that answers the upload methods; `commit` decides the commit's answer. */
function fakeHost(commit: (args: Record<string, unknown>) => unknown = () => done) {
  invoke.mockImplementation(async (cmd: string, a: Record<string, unknown>) => {
    const m = methodOf([cmd, a]);
    const p = argsOf([cmd, a]);
    if (m === "upload_begin") return { upload_id: "u1", final_path: `${p.dir}/${p.name}` };
    if (m === "upload_chunk") return { written: 0 };
    if (m === "upload_commit") { const r = commit(p); if (r instanceof Error || typeof r === "string" || (r as { code?: string }).code) throw r; return r; }
    return null;
  });
}

beforeEach(() => {
  invoke.mockReset();
  FakeChannel.all = [];
});

describe("constants", () => {
  it("equal the Rust literals", () => {
    const str = (name: string) => new RegExp(`pub const ${name}: &str = "([^"]*)";`).exec(transferRs)?.[1];
    expect(str("FILE_CHANGED")).toBe(FILE_CHANGED);
    expect(str("FILE_EXISTS")).toBe(FILE_EXISTS);
    expect(str("UPLOAD_UNKNOWN")).toBe(UPLOAD_UNKNOWN);
    expect(str("UPLOAD_OUTCOME_UNKNOWN")).toBe(UPLOAD_OUTCOME_UNKNOWN);
    expect(str("TRANSFER_CANCELLED")).toBe(TRANSFER_CANCELLED);
    expect(/pub const MAX_CHUNK: u64 = 1024 \* 1024;/.test(transferRs)).toBe(true);
    expect(CHUNK).toBe(1024 * 1024);
  });
});

describe("upload of bytes", () => {
  it("splits 2.5 MiB into three chunks with their offsets and base64", async () => {
    fakeHost();
    const bytes = new Uint8Array(CHUNK * 2.5).map((_, i) => i % 251);
    const progress: [number, number][] = [];
    await expect(upload(H, "/d", bytes, { name: "a.bin", onClash: "fail" }, (d, t) => progress.push([d, t]))).resolves.toEqual(done);
    expect(invoke.mock.calls.map(methodOf)).toEqual(["upload_begin", "upload_chunk", "upload_chunk", "upload_chunk", "upload_commit"]);
    expect(argsOf(invoke.mock.calls[0])).toEqual({ dir: "/d", name: "a.bin", size: CHUNK * 2.5, onClash: "fail" });
    const chunks = invoke.mock.calls.filter(c => methodOf(c) === "upload_chunk").map(argsOf);
    expect(chunks.map(c => c.offset)).toEqual([0, CHUNK, 2 * CHUNK]);
    expect(chunks[2].bytesBase64).toBe(refBase64(bytes.subarray(2 * CHUNK)));
    expect(chunks[0].bytesBase64).toBe(refBase64(bytes.subarray(0, CHUNK)));
    expect(argsOf(invoke.mock.calls[4])).toEqual({ uploadId: "u1", ifVersion: null });
    expect(progress).toEqual([[0, CHUNK * 2.5], [CHUNK, CHUNK * 2.5], [2 * CHUNK, CHUNK * 2.5], [CHUNK * 2.5, CHUNK * 2.5]]);
  });

  it("takes a File's name and a Blob's bytes", async () => {
    fakeHost();
    await upload(undefined, "/d", new File([new Uint8Array([104, 105])], "hi.txt"), { onClash: "keep_both" });
    expect(invoke.mock.calls[0]).toEqual(["upload_begin", { dir: "/d", name: "hi.txt", size: 2, onClash: "keep_both" }]);
    expect(invoke.mock.calls[1]).toEqual(["upload_chunk", { uploadId: "u1", offset: 0, bytesBase64: "aGk=" }]);
  });

  it("an abort mid-way aborts the Host's session", async () => {
    const ac = new AbortController();
    fakeHost();
    const inner = invoke.getMockImplementation()!;
    invoke.mockImplementation(async (cmd: string, a: Record<string, unknown>) => {
      if (methodOf([cmd, a]) === "upload_chunk") ac.abort();
      return inner(cmd, a);
    });
    const e = await upload(H, "/d", new Uint8Array(CHUNK * 2), { name: "a", onClash: "fail" }, undefined, ac.signal).catch(x => x);
    expect(e).toMatchObject({ name: "AbortError" });
    expect(invoke.mock.calls.map(methodOf)).toEqual(["upload_begin", "upload_chunk", "upload_abort"]);
    // Aborted before the start: nothing is called.
    invoke.mockClear();
    await expect(upload(H, "/d", new Uint8Array(1), { name: "a", onClash: "fail" }, undefined, ac.signal)).rejects.toMatchObject({ name: "AbortError" });
    expect(invoke).not.toHaveBeenCalled();
  });

  it("FILE_CHANGED from local and remote both give UploadPendingError", async () => {
    const msg = `${FILE_CHANGED}: sha256:ab`;
    // Local: the command rejects with the string.
    fakeHost(() => msg);
    const local = await upload(undefined, "/d", new Uint8Array(1), { name: "a", onClash: "replace", ifVersion: "sha256:00" }).catch(x => x);
    // Remote: host_call rejects with a HostError of code "remote".
    fakeHost(() => ({ code: "remote", message: msg }));
    const remote = await upload(H, "/d", new Uint8Array(1), { name: "a", onClash: "replace", ifVersion: "sha256:00" }).catch(x => x);
    for (const e of [local, remote]) {
      expect(e).toBeInstanceOf(UploadPendingError);
      expect(isFileChanged(e)).toBe(true);
      expect(isFileExists(e)).toBe(false);
      expect((e as UploadPendingError).uploadId).toBe("u1");
      expect((e as UploadPendingError).cause).toBe(msg);
    }
    expect(calls("upload_abort").length + calls("host_call").filter(c => methodOf(c) === "upload_abort").length).toBe(0);
    // "Overwrite": commit again without the precondition.
    invoke.mockClear();
    fakeHost();
    await commitAgain(H, "u1", { ifVersion: null });
    expect(invoke.mock.calls[0]).toEqual(["host_call", { host: H, method: "upload_commit", params: { uploadId: "u1", ifVersion: null } }]);
  });

  it("a commit timeout is outcome-unknown, not pending", async () => {
    fakeHost(() => ({ code: "indeterminate", message: `${UPLOAD_OUTCOME_UNKNOWN}: call timed out` }));
    const e = await upload(H, "/d", new Uint8Array(1), { name: "a", onClash: "fail" }).catch(x => x);
    expect(e).toBeInstanceOf(HostUnavailableError);
    expect(isUploadOutcomeUnknown(e)).toBe(true);
  });

  it("an offline Host gives HostUnavailableError", async () => {
    invoke.mockRejectedValue({ code: "offline", message: "down" });
    const e = await upload(H, "/d", new Uint8Array(1), { name: "a", onClash: "fail" }).catch(x => x);
    expect(e).toBeInstanceOf(HostUnavailableError);
    expect(e.code).toBe("offline");
  });
});

describe("Rust pumps", () => {
  it("download sends transfer_download with a Channel and forwards progress", async () => {
    const d = deferred<unknown>();
    invoke.mockImplementation((cmd: string) => (cmd === "transfer_download" ? d.p : Promise.resolve()));
    const progress: [number, number][] = [];
    const p = download(H, "/r/a", "/tmp/a", (x, t) => progress.push([x, t]), undefined, { private: true });
    const [, args] = calls("transfer_download")[0];
    expect(args).toMatchObject({ host: H, remotePath: "/r/a", localPath: "/tmp/a", private: true });
    expect(typeof args.transferId).toBe("string");
    expect(args.onProgress).toBe(FakeChannel.all[0]);
    FakeChannel.all[0].emit({ done: 5, total: 10 });
    d.resolve(done);
    await expect(p).resolves.toEqual(done);
    expect(progress).toEqual([[5, 10]]);
    // The Local Host: host null.
    invoke.mockResolvedValue(done);
    await download(undefined, "/r/a", "/tmp/a");
    expect(calls("transfer_download")[1][1]).toMatchObject({ host: null, private: false });
  });

  it("an abort sends transfer_cancel and rejects with AbortError", async () => {
    const d = deferred<unknown>();
    invoke.mockImplementation((cmd: string) => (cmd === "transfer_download" ? d.p : Promise.resolve()));
    const ac = new AbortController();
    const p = download(H, "/r/a", "/tmp/a", undefined, ac.signal);
    const id = calls("transfer_download")[0][1].transferId;
    ac.abort();
    expect(calls("transfer_cancel")).toEqual([["transfer_cancel", { transferId: id }]]);
    d.reject({ code: "remote", message: TRANSFER_CANCELLED });
    await expect(p).rejects.toMatchObject({ name: "AbortError" });
  });

  it("pump errors reject like Host calls", async () => {
    invoke.mockRejectedValueOnce({ code: "remote", message: `${FILE_CHANGED}: during download` });
    const e = await download(H, "/r/a", "/tmp/a").catch(x => x);
    expect(isFileChanged(e)).toBe(true);
    invoke.mockRejectedValueOnce({ code: "offline", message: "down" });
    await expect(download(H, "/r/a", "/tmp/a")).rejects.toBeInstanceOf(HostUnavailableError);
  });

  it("a { localPath } upload runs transfer_upload_path", async () => {
    invoke.mockResolvedValue(done);
    await upload(H, "/d", { localPath: "/home/u/notes.md" }, { onClash: "replace", ifVersion: "sha256:aa" });
    expect(invoke.mock.calls.map(c => c[0])).toEqual(["transfer_upload_path"]);
    expect(invoke.mock.calls[0][1]).toMatchObject({ host: H, localPath: "/home/u/notes.md", dir: "/d", name: "notes.md", onClash: "replace", ifVersion: "sha256:aa" });
    expect(invoke.mock.calls[0][1].onProgress).toBeInstanceOf(FakeChannel);
  });
});

describe("uploadEntries", () => {
  const file = (name: string, n = 1) => new File([new Uint8Array(n)], name);
  const entries: UploadEntry[] = [
    { rel: ["pics"] },
    { rel: ["pics", "a.png"], file: file("a.png", 3) },
    { rel: ["pics", "deep"] },
    { rel: ["pics", "deep", "b.png"], file: file("b.png", 2) },
    { rel: ["c.txt"], file: file("c.txt") },
  ];

  it("creates directories before their files", async () => {
    fakeHost();
    const progress: number[] = [];
    const r = await uploadEntries(H, "/d", entries, { resolveClash: async () => "skip" }, d => progress.push(d));
    const seq = invoke.mock.calls.map(c => [methodOf(c), argsOf(c).path ?? argsOf(c).dir]).filter(([m]) => m === "make_dir" || m === "upload_begin");
    expect(seq).toEqual([["make_dir", "/d/pics"], ["upload_begin", "/d/pics"], ["make_dir", "/d/pics/deep"], ["upload_begin", "/d/pics/deep"], ["upload_begin", "/d"]]);
    expect(r.uploaded.length).toBe(3);
    expect(progress[progress.length - 1]).toBe(6);
  });

  it("asks on a clash: replace, keep_both, skip", async () => {
    const asked: string[] = [];
    const answers = ["replace", "keep_both", "skip"] as const;
    invoke.mockImplementation(async (cmd: string, a: Record<string, unknown>) => {
      const m = methodOf([cmd, a]);
      const p = argsOf([cmd, a]);
      if (m === "upload_begin") {
        if (p.onClash === "fail") throw { code: "remote", message: `${FILE_EXISTS}: ${p.dir}/${p.name}` };
        return { upload_id: `u-${p.name}`, final_path: `${p.dir}/${p.name}` };
      }
      if (m === "upload_commit") return { ...done, path: p.uploadId };
      return null;
    });
    let i = 0;
    const flat: UploadEntry[] = [{ rel: ["x"], file: file("x") }, { rel: ["y"], file: file("y") }, { rel: ["z"], file: file("z") }];
    const r = await uploadEntries(H, "/d", flat, { resolveClash: async p => { asked.push(p); return answers[i++]; } });
    expect(asked).toEqual(["/d/x", "/d/y", "/d/z"]);
    const begins = invoke.mock.calls.filter(c => methodOf(c) === "upload_begin").map(c => [argsOf(c).name, argsOf(c).onClash]);
    expect(begins).toEqual([["x", "fail"], ["x", "replace"], ["y", "fail"], ["y", "keep_both"], ["z", "fail"]]);
    expect(r.uploaded.map(t => t.path)).toEqual(["u-x", "u-y"]);
    expect(r.skipped).toEqual(["/d/z"]);
  });

  it("a clash at commit commits again with the choice, or aborts on skip", async () => {
    const answers = ["keep_both", "skip"] as const;
    let i = 0;
    invoke.mockImplementation(async (cmd: string, a: Record<string, unknown>) => {
      const m = methodOf([cmd, a]);
      const p = argsOf([cmd, a]);
      if (m === "upload_begin") return { upload_id: `u-${p.name}`, final_path: "" };
      if (m === "upload_commit") {
        if (!p.onClash) throw { code: "remote", message: `${FILE_EXISTS}: /d/${p.uploadId}` };
        return { ...done, path: `${p.uploadId}:${p.onClash}` };
      }
      return null;
    });
    const r = await uploadEntries(H, "/d", [{ rel: ["x"], file: file("x") }, { rel: ["y"], file: file("y") }], { resolveClash: async () => answers[i++] });
    expect(r.uploaded.map(t => t.path)).toEqual(["u-x:keep_both"]);
    expect(r.skipped).toEqual(["/d/y"]);
    expect(invoke.mock.calls.filter(c => methodOf(c) === "upload_abort").map(c => argsOf(c).uploadId)).toEqual(["u-y"]);
  });
});

describe("uploadEntries cancel", () => {
  it("an abort while the clash is asked drops the pending upload instead of committing", async () => {
    invoke.mockImplementation(async (cmd: string, a: Record<string, unknown>) => {
      const m = methodOf([cmd, a]);
      const p = argsOf([cmd, a]);
      if (m === "upload_begin") return { upload_id: "u-x", final_path: "" };
      if (m === "upload_commit") {
        if (!p.onClash) throw { code: "remote", message: `${FILE_EXISTS}: /d/x` };
        return done;
      }
      return null;
    });
    const asked = deferred<"replace">();
    const ac = new AbortController();
    const p = uploadEntries(H, "/d", [{ rel: ["x"], file: new File([new Uint8Array(1)], "x") }], { resolveClash: () => asked.p }, undefined, ac.signal);
    await new Promise(r => setTimeout(r, 0));
    ac.abort();
    asked.resolve("replace");
    await expect(p).rejects.toMatchObject({ name: "AbortError" });
    const commits = invoke.mock.calls.filter(c => methodOf(c) === "upload_commit").map(argsOf);
    expect(commits).toEqual([{ uploadId: "u-x", ifVersion: null }]);
    expect(invoke.mock.calls.filter(c => methodOf(c) === "upload_abort").map(c => argsOf(c).uploadId)).toEqual(["u-x"]);
  });
});

describe("helpers", () => {
  it("bytesToBase64 handles large and empty arrays", () => {
    const big = new Uint8Array(100_000).map((_, i) => i % 256);
    expect(bytesToBase64(big)).toBe(refBase64(big));
    expect(bytesToBase64(new Uint8Array())).toBe("");
  });

  it("joinPath keeps the base's separator", () => {
    expect(joinPath("/d", "a", "b")).toBe("/d/a/b");
    expect(joinPath("/d/", "a")).toBe("/d/a");
    expect(joinPath("C:\\Users\\u", "a")).toBe("C:\\Users\\u\\a");
  });
});

// ── Drop-in (S3b) ────────────────────────────────────────────────────

// FileSystemEntry fakes: a directory's reader answers in the given batches, then empty.
function fileEntry(name: string, size = 1): FileSystemFileEntry {
  return { name, isFile: true, isDirectory: false, file: (ok: (f: File) => void) => ok(new File([new Uint8Array(size)], name)) } as unknown as FileSystemFileEntry;
}
function dirEntry(name: string, batches: FileSystemEntry[][]): FileSystemDirectoryEntry {
  return {
    name, isFile: false, isDirectory: true,
    createReader: () => {
      const queue = [...batches];
      return { readEntries: (ok: (b: FileSystemEntry[]) => void) => ok(queue.shift() ?? []) };
    },
  } as unknown as FileSystemDirectoryEntry;
}
function dataTransfer(items: { entry?: FileSystemEntry | null; file?: File }[]): DataTransfer {
  return {
    items: items.map(it => ({ kind: "file", webkitGetAsEntry: () => it.entry ?? null, getAsFile: () => it.file ?? null })),
  } as unknown as DataTransfer;
}

describe("entriesFromDrop", () => {
  it("walks nested folders, parents first, reading every batch", async () => {
    const tree = dirEntry("pics", [[fileEntry("a.png"), dirEntry("deep", [[fileEntry("b.png")], [fileEntry("c.png")]])], [fileEntry("d.png")]]);
    const out = await entriesFromDrop(dataTransfer([{ entry: tree }, { entry: fileEntry("top.txt") }]));
    expect(out.map(e => [e.rel.join("/"), !!e.file])).toEqual([
      ["pics", false], ["pics/a.png", true], ["pics/deep", false], ["pics/deep/b.png", true], ["pics/deep/c.png", true], ["pics/d.png", true], ["top.txt", true],
    ]);
  });

  it("falls back to the files when there is no entry", async () => {
    const f = new File([new Uint8Array(2)], "loose.bin");
    const out = await entriesFromDrop(dataTransfer([{ entry: null, file: f }]));
    expect(out).toEqual([{ rel: ["loose.bin"], file: f }]);
  });

  it("counts what it finds and stops on a cancel", async () => {
    const ac = new AbortController();
    const found: number[] = [];
    const tree = dirEntry("pics", [[fileEntry("a.png"), fileEntry("b.png")]]);
    await entriesFromDrop(dataTransfer([{ entry: tree }]), { onFound: n => found.push(n) });
    expect(found).toEqual([1, 2, 3]);
    const stop = dirEntry("pics", [[fileEntry("a.png")]]);
    const p = entriesFromDrop(dataTransfer([{ entry: stop }]), { signal: ac.signal, onFound: () => ac.abort() });
    await expect(p).rejects.toBeInstanceOf(TransferAbortedError);
  });

  it("an unreadable folder rejects", async () => {
    const bad = { name: "x", isFile: false, isDirectory: true, createReader: () => ({ readEntries: (_: unknown, fail: (e: Error) => void) => fail(new Error("denied")) }) } as unknown as FileSystemEntry;
    await expect(entriesFromDrop(dataTransfer([{ entry: bad }]))).rejects.toThrow("denied");
  });
});

describe("uploadEntries batches", () => {
  const file = (name: string, n = 1) => new File([new Uint8Array(n)], name);

  it("joins names with the Host's separator", async () => {
    fakeHost();
    await uploadEntries(H, "C:\\work", [{ rel: ["pics"] }, { rel: ["pics", "a.png"], file: file("a.png") }], { resolveClash: async () => "skip" });
    const seq = invoke.mock.calls.map(c => [methodOf(c), argsOf(c).path ?? argsOf(c).dir]).filter(([m]) => m === "make_dir" || m === "upload_begin");
    expect(seq).toEqual([["make_dir", "C:\\work\\pics"], ["upload_begin", "C:\\work\\pics"]]);
  });

  it("reports file i of n with its own progress", async () => {
    fakeHost();
    const at: unknown[] = [];
    await uploadEntries(H, "/d", [{ rel: ["a"], file: file("a", 2) }, { rel: ["b"], file: file("b", 3) }], { resolveClash: async () => "skip" }, (_d, _t, cur, a) => at.push([cur, a]));
    expect(at).toContainEqual(["/d/a", { i: 1, n: 2, fileSize: 2, fileDone: 2 }]);
    expect(at[at.length - 1]).toEqual(["/d/b", { i: 2, n: 2, fileSize: 3, fileDone: 3 }]);
  });

  it("an existing folder merges; a file in the way skips its subtree", async () => {
    invoke.mockImplementation(async (cmd: string, a: Record<string, unknown>) => {
      const m = methodOf([cmd, a]);
      const p = argsOf([cmd, a]);
      if (m === "make_dir") { if (p.path === "/d/blocked") throw { code: "remote", message: `${FILE_EXISTS}: /d/blocked` }; return null; }
      if (m === "upload_begin") return { upload_id: `u-${p.name}`, final_path: "" };
      if (m === "upload_commit") return { ...done, path: p.uploadId };
      return null;
    });
    const r = await uploadEntries(H, "/d", [
      { rel: ["blocked"] }, { rel: ["blocked", "in"] }, { rel: ["blocked", "in", "x"], file: file("x") },
      { rel: ["merged"] }, { rel: ["merged", "y"], file: file("y") },
    ], { resolveClash: async () => "skip" });
    expect(r.skipped).toEqual(["/d/blocked", "/d/blocked/in/x"]);
    expect(r.uploaded.map(t => t.path)).toEqual(["u-y"]);
    expect(invoke.mock.calls.filter(c => methodOf(c) === "make_dir").map(c => argsOf(c).path)).toEqual(["/d/blocked", "/d/merged"]);
  });

  it("a failing file is counted and the batch goes on", async () => {
    invoke.mockImplementation(async (cmd: string, a: Record<string, unknown>) => {
      const m = methodOf([cmd, a]);
      const p = argsOf([cmd, a]);
      if (m === "upload_begin") { if (p.name === "bad") throw { code: "remote", message: "Permission denied" }; return { upload_id: `u-${p.name}`, final_path: "" }; }
      if (m === "upload_commit") return { ...done, path: p.uploadId };
      return null;
    });
    const r = await uploadEntries(H, "/d", [{ rel: ["bad"], file: file("bad") }, { rel: ["ok"], file: file("ok") }], { resolveClash: async () => "skip" });
    expect(r.failed).toEqual([{ path: "/d/bad", error: "Permission denied" }]);
    expect(r.uploaded.map(t => t.path)).toEqual(["u-ok"]);
  });

  it("an offline Host stops the batch; the rest counts as failed", async () => {
    invoke.mockImplementation(async (cmd: string, a: Record<string, unknown>) => {
      const m = methodOf([cmd, a]);
      const p = argsOf([cmd, a]);
      if (m === "upload_begin") { if (p.name === "b") throw { code: "offline", message: "down" }; return { upload_id: `u-${p.name}`, final_path: "" }; }
      if (m === "upload_commit") return { ...done, path: p.uploadId };
      return null;
    });
    const r = await uploadEntries(H, "/d", [{ rel: ["a"], file: file("a") }, { rel: ["b"], file: file("b") }, { rel: ["c"], file: file("c") }], { resolveClash: async () => "skip" });
    expect(r.uploaded.map(t => t.path)).toEqual(["u-a"]);
    expect(r.failed.map(f => f.path)).toEqual(["/d/b", "/d/c"]);
    expect(invoke.mock.calls.filter(c => methodOf(c) === "upload_begin").map(c => argsOf(c).name)).toEqual(["a", "b"]);
  });

  it("cancel aborts the current upload, stops the queue and counts what finished", async () => {
    const ac = new AbortController();
    invoke.mockImplementation(async (cmd: string, a: Record<string, unknown>) => {
      const m = methodOf([cmd, a]);
      const p = argsOf([cmd, a]);
      if (m === "upload_begin") return { upload_id: `u-${p.name}`, final_path: "" };
      if (m === "upload_chunk" && p.uploadId === "u-b") ac.abort();
      if (m === "upload_commit") return { ...done, path: p.uploadId };
      return null;
    });
    const e = await uploadEntries(H, "/d", [{ rel: ["a"], file: file("a") }, { rel: ["b"], file: file("b") }, { rel: ["c"], file: file("c") }], { resolveClash: async () => "skip" }, undefined, ac.signal).catch(x => x);
    expect(e).toBeInstanceOf(TransferAbortedError);
    expect(e.completed).toBe(1);
    expect(invoke.mock.calls.filter(c => methodOf(c) === "upload_abort").map(c => argsOf(c).uploadId)).toEqual(["u-b"]);
    expect(invoke.mock.calls.filter(c => methodOf(c) === "upload_begin").map(c => argsOf(c).name)).toEqual(["a", "b"]);
  });
});

describe("downloadTree", () => {
  it("lists first, then creates folders and downloads files, with per-file progress", async () => {
    invoke.mockImplementation(async (cmd: string, a: Record<string, unknown>) => {
      const m = methodOf([cmd, a]);
      const p = argsOf([cmd, a]);
      if (m === "list_dir") return p.path === "/srv/src" ? [{ name: "a", path: "/srv/src/a", is_dir: false }, { name: "sub", path: "/srv/src/sub", is_dir: true }] : [{ name: "b", path: "/srv/src/sub/b", is_dir: false }];
      if (cmd === "transfer_download") { (a.onProgress as InstanceType<typeof FakeChannel>).emit({ done: 1, total: 2 }); return done; }
      return null;
    });
    const progress: unknown[] = [];
    const listing: number[] = [];
    const r = await downloadTree(H, "/srv/src", "/home/u/src (2)", (...x) => progress.push(x), undefined, n => listing.push(n));
    expect(r).toEqual({ files: 2, skipped: [] });
    expect(listing).toEqual([1, 2, 3]);
    // The root was reserved by the caller: only sub-folders are made, after the listing.
    const order = invoke.mock.calls.map(c => methodOf(c)).filter(m => m === "list_dir" || m === "make_dir");
    expect(order).toEqual(["list_dir", "list_dir", "make_dir"]);
    expect(invoke.mock.calls.filter(c => c[0] === "make_dir").map(c => (c[1] as { path: string }).path)).toEqual(["/home/u/src (2)/sub"]);
    expect(calls("transfer_download").map(c => [c[1].remotePath, c[1].localPath])).toEqual([["/srv/src/a", "/home/u/src (2)/a"], ["/srv/src/sub/b", "/home/u/src (2)/sub/b"]]);
    expect(progress).toContainEqual([0, 2, "/srv/src/a", 0.5]);
    expect(progress[progress.length - 1]).toEqual([2, 2, "", 1]);
  });

  it("names a Windows destination cannot hold are skipped, never written outside it", async () => {
    invoke.mockImplementation(async (cmd: string, a: Record<string, unknown>) => {
      const m = methodOf([cmd, a]);
      const p = argsOf([cmd, a]);
      if (m === "list_dir") {
        return p.path === "/srv/src"
          ? [{ name: "..\\notes.txt", path: "/srv/src/..\\notes.txt", is_dir: false }, { name: "a:b", path: "/srv/src/a:b", is_dir: true }, { name: "ok.txt", path: "/srv/src/ok.txt", is_dir: false }, { name: "..", path: "/srv/src/..", is_dir: true }]
          : [];
      }
      if (cmd === "transfer_download") return done;
      return null;
    });
    const r = await downloadTree(H, "/srv/src", "C:\\Downloads\\src", undefined);
    expect(r).toEqual({ files: 1, skipped: ["/srv/src/..\\notes.txt", "/srv/src/a:b", "/srv/src/.."] });
    expect(calls("transfer_download").map(c => c[1].localPath)).toEqual(["C:\\Downloads\\src\\ok.txt"]);
    // A skipped folder is not listed.
    expect(invoke.mock.calls.filter(c => methodOf(c) === "list_dir").length).toBe(1);
  });

  it("../x from a POSIX Host into a POSIX destination is skipped", async () => {
    invoke.mockImplementation(async (cmd: string, a: Record<string, unknown>) =>
      methodOf([cmd, a]) === "list_dir" ? [{ name: "../x", path: "/srv/src/../x", is_dir: false }, { name: "..", path: "/srv", is_dir: true }] : null);
    const r = await downloadTree(H, "/srv/src", "/home/u/src", undefined);
    expect(r).toEqual({ files: 0, skipped: ["/srv/src/../x", "/srv"] });
    expect(calls("transfer_download")).toEqual([]);
  });

  it("refuses too many files before creating or downloading anything", async () => {
    const many = Array.from({ length: TREE_MAX_ENTRIES + 1 }, (_, i) => ({ name: `f${i}`, path: `/srv/big/f${i}`, is_dir: false }));
    invoke.mockImplementation(async (cmd: string, a: Record<string, unknown>) => (methodOf([cmd, a]) === "list_dir" ? many : null));
    const e = await downloadTree(H, "/srv/big", "/home/u/big").catch(x => x);
    expect(e).toBeInstanceOf(TreeTooLargeError);
    expect(e.max).toBe(TREE_MAX_ENTRIES);
    expect(calls("transfer_download")).toEqual([]);
    expect(calls("make_dir")).toEqual([]);
  });

  it("folders count against the entry budget too", async () => {
    const dirs = Array.from({ length: TREE_MAX_ENTRIES + 1 }, (_, i) => ({ name: `d${i}`, path: `/srv/wide/d${i}`, is_dir: true }));
    invoke.mockImplementation(async (cmd: string, a: Record<string, unknown>) => {
      if (methodOf([cmd, a]) !== "list_dir") return null;
      return argsOf([cmd, a]).path === "/srv/wide" ? dirs : [];
    });
    const e = await downloadTree(H, "/srv/wide", "/home/u/wide").catch(x => x);
    expect(e).toBeInstanceOf(TreeTooLargeError);
    expect(e.limit).toBe("entries");
    expect(calls("make_dir")).toEqual([]);
  });

  it("refuses a folder deeper than the cap (symlink loops)", async () => {
    invoke.mockImplementation(async (cmd: string, a: Record<string, unknown>) => {
      const p = argsOf([cmd, a]);
      return methodOf([cmd, a]) === "list_dir" ? [{ name: "loop", path: `${p.path}/loop`, is_dir: true }] : null;
    });
    await expect(downloadTree(H, "/srv/l", "/home/u/l")).rejects.toMatchObject({ name: "TreeTooLargeError", limit: "depth" });
    expect(calls("make_dir")).toEqual([]);
  });

  it("a cancel during the listing stops it", async () => {
    const ac = new AbortController();
    invoke.mockImplementation(async (cmd: string, a: Record<string, unknown>) => {
      if (methodOf([cmd, a]) !== "list_dir") return null;
      ac.abort();
      return [{ name: "a", path: "/s/a", is_dir: true }];
    });
    const e = await downloadTree(H, "/s", "/l", undefined, ac.signal).catch(x => x);
    expect(e).toBeInstanceOf(TransferAbortedError);
    expect(invoke.mock.calls.filter(c => methodOf(c) === "list_dir").length).toBe(1);
  });

  it("a cancel counts the files done", async () => {
    const ac = new AbortController();
    invoke.mockImplementation(async (cmd: string, a: Record<string, unknown>) => {
      if (methodOf([cmd, a]) === "list_dir") return [{ name: "a", path: "/s/a", is_dir: false }, { name: "b", path: "/s/b", is_dir: false }];
      if (cmd === "transfer_download") {
        if (a.remotePath === "/s/b") { ac.abort(); throw { code: "remote", message: TRANSFER_CANCELLED }; }
        return done;
      }
      return null;
    });
    const e = await downloadTree(H, "/s", "/l", undefined, ac.signal).catch(x => x);
    expect(e).toBeInstanceOf(TransferAbortedError);
    expect(e.completed).toBe(1);
  });
});
