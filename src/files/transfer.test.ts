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
