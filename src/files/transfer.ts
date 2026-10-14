import { Channel, invoke } from "@tauri-apps/api/core";
import { hostInvoke, rethrowHostCall } from "../hosts/hostInvoke";
import { isHostError, type HostId } from "../hosts/types";

// Chunked file transfers with a Host (the Local Host when `host` is undefined), over the Host
// methods of src-tauri/crates/core/src/transfer.rs. Downloads and uploads of a local path run
// as one Rust-side pump (`transfer_download`, `transfer_upload_path`), so their bytes never
// pass through the webview. Uploads of bytes that only exist here (dropped `File`s) loop over
// `upload_begin` / `upload_chunk` / `upload_commit` from TypeScript.
//
// Errors reject like any Host call: a Host-side failure as its message string, a connection
// problem as HostUnavailableError. The prefixes below are the Rust constants' text.

export const CHUNK = 1024 * 1024;
export const FILE_CHANGED = "file changed on the host";
export const FILE_EXISTS = "file exists";
export const UPLOAD_UNKNOWN = "unknown upload";
export const UPLOAD_OUTCOME_UNKNOWN = "upload outcome unknown";
export const TRANSFER_CANCELLED = "transfer cancelled";

export type OnClash = "fail" | "replace" | "keep_both";

export interface FileStat {
  size: number;
  mtime_ms: number;
  is_dir: boolean;
  is_symlink: boolean;
  // `sha256:<hex>`, only when asked for.
  version: string | null;
}

export interface Transferred {
  path: string;
  size: number;
  version: string;
}

export type OnProgress = (done: number, total: number) => void;

function messageOf(e: unknown): string {
  if (typeof e === "string") return e;
  if (e instanceof Error) return e.message;
  return "";
}

// The Host's file is not the version expected (`…: <current version | deleted | retargeted>`).
export const isFileChanged = (e: unknown) => messageOf(e).startsWith(FILE_CHANGED);
// The target exists (onClash "fail").
export const isFileExists = (e: unknown) => messageOf(e).startsWith(FILE_EXISTS);
// A commit that got no answer: it may have published the file. Stat the target again.
export const isUploadOutcomeUnknown = (e: unknown) => messageOf(e).startsWith(UPLOAD_OUTCOME_UNKNOWN);

export class TransferAbortedError extends Error {
  constructor() {
    super(TRANSFER_CANCELLED);
    this.name = "AbortError";
  }
}

// A commit refused with FILE_CHANGED or FILE_EXISTS: the Host keeps the uploaded bytes. The
// caller either commits again (`commitAgain`, e.g. "Overwrite" with ifVersion null, or another
// onClash) or drops them (`abortUpload`). The message is the refusal's, so isFileChanged /
// isFileExists work on it too.
export class UploadPendingError extends Error {
  readonly host: HostId | undefined;
  readonly uploadId: string;
  readonly cause: unknown;
  constructor(host: HostId | undefined, uploadId: string, cause: unknown) {
    super(messageOf(cause));
    this.name = "UploadPendingError";
    this.host = host;
    this.uploadId = uploadId;
    this.cause = cause;
  }
}

export function stat(host: HostId | undefined, path: string, withVersion = false): Promise<FileStat> {
  return hostInvoke<FileStat>(host, "file_stat", { path, version: withVersion });
}

export async function makeDir(host: HostId | undefined, path: string): Promise<void> {
  await hostInvoke<null>(host, "make_dir", { path });
}

function progressChannel(onProgress?: OnProgress): Channel<{ done: number; total: number }> {
  const ch = new Channel<{ done: number; total: number }>();
  ch.onmessage = p => onProgress?.(p.done, p.total);
  return ch;
}

// Run a Rust pump; an abort cancels it. One that finished anyway (its commit was under way)
// resolves.
async function runPump(cmd: string, args: Record<string, unknown>, host: HostId | undefined, signal?: AbortSignal): Promise<Transferred> {
  if (signal?.aborted) throw new TransferAbortedError();
  const transferId = args.transferId as string;
  const onAbort = () => { invoke("transfer_cancel", { transferId }).catch(() => {}); };
  signal?.addEventListener("abort", onAbort, { once: true });
  try {
    return await invoke<Transferred>(cmd, args);
  } catch (e) {
    if (isHostError(e) && e.message === TRANSFER_CANCELLED) throw new TransferAbortedError();
    return rethrowHostCall(host, e);
  } finally {
    signal?.removeEventListener("abort", onAbort);
  }
}

// Download `remotePath` to `localPath` on this machine (replaced if it exists). `localPath`
// comes from a Save dialog or the app's own temp area only. `private`: owner-only (0600).
export function download(
  host: HostId | undefined,
  remotePath: string,
  localPath: string,
  onProgress?: OnProgress,
  signal?: AbortSignal,
  opts?: { private?: boolean },
): Promise<Transferred> {
  return runPump("transfer_download", {
    transferId: crypto.randomUUID(),
    host: host ?? null,
    remotePath,
    localPath,
    private: !!opts?.private,
    onProgress: progressChannel(onProgress),
  }, host, signal);
}

export type UploadSource = Blob | Uint8Array | { localPath: string };

export interface UploadOptions {
  // Defaults to the File's name or the local path's last component.
  name?: string;
  onClash: OnClash;
  // The target must currently be this version (`deleted`: missing), else FILE_CHANGED.
  ifVersion?: string | null;
}

function lastComponent(p: string): string {
  const parts = p.split(/[\\/]/).filter(Boolean);
  return parts[parts.length - 1] ?? "";
}

// Upload `src` to `dir` on `host`. A `{ localPath }` runs as a Rust pump, which aborts the
// Host's session on FILE_CHANGED / FILE_EXISTS (just upload again). Bytes from here reject
// with UploadPendingError then instead, keeping what was sent.
export async function upload(
  host: HostId | undefined,
  dir: string,
  src: UploadSource,
  opts: UploadOptions,
  onProgress?: OnProgress,
  signal?: AbortSignal,
): Promise<Transferred> {
  if (signal?.aborted) throw new TransferAbortedError();
  if (!(src instanceof Blob) && !(src instanceof Uint8Array)) {
    return runPump("transfer_upload_path", {
      transferId: crypto.randomUUID(),
      host: host ?? null,
      localPath: src.localPath,
      dir,
      name: opts.name ?? lastComponent(src.localPath),
      onClash: opts.onClash,
      ifVersion: opts.ifVersion ?? null,
      onProgress: progressChannel(onProgress),
    }, host, signal);
  }
  const name = opts.name ?? (typeof File !== "undefined" && src instanceof File ? src.name : "");
  if (!name) throw new Error("an upload needs a name");
  const size = src instanceof Blob ? src.size : src.byteLength;
  const read = async (start: number, end: number): Promise<Uint8Array> =>
    src instanceof Blob ? new Uint8Array(await src.slice(start, end).arrayBuffer()) : src.subarray(start, end);
  const begun = await hostInvoke<{ upload_id: string; final_path: string }>(host, "upload_begin", { dir, name, size, onClash: opts.onClash });
  const uploadId = begun.upload_id;
  let sent = 0;
  onProgress?.(0, size);
  try {
    while (sent < size) {
      if (signal?.aborted) throw new TransferAbortedError();
      const end = Math.min(size, sent + CHUNK);
      const bytes = await read(sent, end);
      await hostInvoke(host, "upload_chunk", { uploadId, offset: sent, bytesBase64: bytesToBase64(bytes) });
      sent = end;
      onProgress?.(sent, size);
    }
    if (signal?.aborted) throw new TransferAbortedError();
  } catch (e) {
    void abortUpload(host, uploadId);
    throw e;
  }
  return commit(host, uploadId, { ifVersion: opts.ifVersion ?? null });
}

async function commit(host: HostId | undefined, uploadId: string, opts: { onClash?: OnClash; ifVersion?: string | null }): Promise<Transferred> {
  const args: Record<string, unknown> = { uploadId, ifVersion: opts.ifVersion ?? null };
  if (opts.onClash) args.onClash = opts.onClash;
  try {
    return await hostInvoke<Transferred>(host, "upload_commit", args);
  } catch (e) {
    if (isFileChanged(e) || isFileExists(e)) throw new UploadPendingError(host, uploadId, e);
    throw e;
  }
}

// Commit a pending upload again with other options (D1 "Overwrite": ifVersion null; D4: an
// onClash the user chose).
export function commitAgain(host: HostId | undefined, uploadId: string, opts: { onClash?: OnClash; ifVersion?: string | null }): Promise<Transferred> {
  return commit(host, uploadId, opts);
}

// Drop an upload and its part file on the Host. Best effort: never rejects.
export async function abortUpload(host: HostId | undefined, uploadId: string): Promise<void> {
  try {
    await hostInvoke(host, "upload_abort", { uploadId });
  } catch {
    // The Host sweeps idle uploads itself.
  }
}

export function bytesToBase64(bytes: Uint8Array): string {
  let bin = "";
  for (let i = 0; i < bytes.length; i += 0x8000) {
    bin += String.fromCharCode.apply(null, bytes.subarray(i, i + 0x8000) as unknown as number[]);
  }
  return btoa(bin);
}

// ── Folders ──────────────────────────────────────────────────────────

// One dropped item: a file (`file` set) or a directory, at `rel` (path components) below
// the folder it is dropped into. Directories come before what is in them.
export interface UploadEntry {
  rel: string[];
  file?: File;
}

// The items of an OS drop, folders walked recursively. Call it synchronously in the drop
// handler: the DataTransfer's items are readable only then (the walk itself is async).
export function entriesFromDrop(dt: DataTransfer): Promise<UploadEntry[]> {
  const roots: FileSystemEntry[] = [];
  const loose: File[] = [];
  for (const item of Array.from(dt.items)) {
    if (item.kind !== "file") continue;
    const entry = item.webkitGetAsEntry?.();
    if (entry) roots.push(entry);
    else {
      const f = item.getAsFile();
      if (f) loose.push(f);
    }
  }
  return (async () => {
    const out: UploadEntry[] = loose.map(file => ({ rel: [file.name], file }));
    for (const r of roots) await walkEntry(r, [], out);
    return out;
  })();
}

async function walkEntry(entry: FileSystemEntry, parent: string[], out: UploadEntry[]): Promise<void> {
  const rel = [...parent, entry.name];
  if (entry.isFile) {
    const file = await new Promise<File>((res, rej) => (entry as FileSystemFileEntry).file(res, rej));
    out.push({ rel, file });
    return;
  }
  if (!entry.isDirectory) return;
  out.push({ rel });
  const reader = (entry as FileSystemDirectoryEntry).createReader();
  const children: FileSystemEntry[] = [];
  // readEntries answers in batches until an empty one.
  for (;;) {
    const batch = await new Promise<FileSystemEntry[]>((res, rej) => reader.readEntries(res, rej));
    if (batch.length === 0) break;
    children.push(...batch);
  }
  for (const c of children) await walkEntry(c, rel, out);
}

// Join path components onto `base` with its own separator (a Windows Local Host's paths use
// `\`).
export function joinPath(base: string, ...parts: string[]): string {
  const sep = base.includes("\\") && !base.includes("/") ? "\\" : "/";
  const trimmed = base.endsWith(sep) ? base.slice(0, -1) : base;
  return [trimmed, ...parts].join(sep);
}

export type ClashChoice = "replace" | "keep_both" | "skip";

export interface UploadEntriesResult {
  uploaded: Transferred[];
  skipped: string[];
}

// Upload dropped entries into `dir`: directories are created (merged into existing ones),
// files uploaded with onClash "fail"; on a clash `resolveClash` decides (it owns "apply to
// all"). Progress counts bytes over all files; `current` is the file being sent.
export async function uploadEntries(
  host: HostId | undefined,
  dir: string,
  entries: UploadEntry[],
  opts: { resolveClash: (path: string) => Promise<ClashChoice> },
  onProgress?: (done: number, total: number, current: string) => void,
  signal?: AbortSignal,
): Promise<UploadEntriesResult> {
  const total = entries.reduce((n, e) => n + (e.file?.size ?? 0), 0);
  const result: UploadEntriesResult = { uploaded: [], skipped: [] };
  let base = 0;
  for (const e of entries) {
    if (signal?.aborted) throw new TransferAbortedError();
    const path = joinPath(dir, ...e.rel);
    if (!e.file) {
      await makeDir(host, path);
      continue;
    }
    const parent = joinPath(dir, ...e.rel.slice(0, -1));
    const name = e.rel[e.rel.length - 1];
    const prog: OnProgress = d => onProgress?.(base + d, total, path);
    let done: Transferred | null = null;
    try {
      done = await upload(host, parent, e.file, { name, onClash: "fail" }, prog, signal);
    } catch (err) {
      if (!isFileExists(err)) throw err;
      const choice = await opts.resolveClash(path);
      if (err instanceof UploadPendingError) {
        // It appeared while uploading: the bytes are on the Host already.
        // The user may have cancelled while being asked: nothing is published then.
        if (choice === "skip" || signal?.aborted) await abortUpload(host, err.uploadId);
        if (signal?.aborted) throw new TransferAbortedError();
        if (choice !== "skip") done = await commitAgain(host, err.uploadId, { onClash: choice });
      } else if (choice !== "skip") {
        done = await upload(host, parent, e.file, { name, onClash: choice }, prog, signal);
      }
    }
    if (done) result.uploaded.push(done);
    else result.skipped.push(path);
    base += e.file.size;
    onProgress?.(base, total, path);
  }
  return result;
}

const TREE_MAX_DEPTH = 32;
const TREE_MAX_FILES = 10_000;

interface DirItem {
  name: string;
  path: string;
  is_dir: boolean;
}

// Download the folder `remoteDir` into `localDir` (created, merged into an existing one).
// `list_dir` follows symlinked folders, so the walk is capped (depth 32, 10 000 files).
// Progress counts files.
export async function downloadTree(
  host: HostId | undefined,
  remoteDir: string,
  localDir: string,
  onProgress?: (done: number, total: number, current: string) => void,
  signal?: AbortSignal,
): Promise<{ files: number }> {
  const files: { remote: string; local: string }[] = [];
  const walk = async (remote: string, local: string, depth: number): Promise<void> => {
    if (depth > TREE_MAX_DEPTH) throw new Error(`folder too deep: ${remote}`);
    if (signal?.aborted) throw new TransferAbortedError();
    await hostInvoke<null>(undefined, "make_dir", { path: local });
    const items = await hostInvoke<DirItem[]>(host, "list_dir", { path: remote });
    for (const it of items) {
      const l = joinPath(local, it.name);
      if (it.is_dir) await walk(it.path, l, depth + 1);
      else {
        if (files.length >= TREE_MAX_FILES) throw new Error(`more than ${TREE_MAX_FILES} files: ${remoteDir}`);
        files.push({ remote: it.path, local: l });
      }
    }
  };
  await walk(remoteDir, localDir, 0);
  let n = 0;
  for (const f of files) {
    onProgress?.(n, files.length, f.remote);
    await download(host, f.remote, f.local, undefined, signal);
    n++;
  }
  onProgress?.(n, files.length, "");
  return { files: n };
}
