import { baseName, isLocalName, joinPath, pathStyle } from "./paths";

// "Download to…" (Lesani/xshell#46, D3): a file goes where the OS Save dialog says (the
// dialog asks before replacing); a folder goes into the picked folder, recursively, as a new
// "<name>", or "<name> (2)" and so on when that exists. The folder is reserved by creating it
// exclusively, so two downloads never merge into one. Works the same on every Host (D2).

export interface DownloadItem {
  path: string;
  name: string;
  isDir: boolean;
}

export interface DownloadToDeps {
  // The OS Save dialog for a file named `name`; null when cancelled.
  saveDialog(name: string): Promise<string | null>;
  // The OS folder picker; null when cancelled.
  folderDialog(): Promise<string | null>;
  // Create the folder `path` exclusively: true when created, false when something is there.
  reserveDir(path: string): Promise<boolean>;
  downloadFile(remote: string, local: string, onProgress: (done: number, total: number) => void, signal?: AbortSignal): Promise<unknown>;
  downloadTree(
    remote: string,
    local: string,
    onProgress: (done: number, total: number, current: string, fileFraction?: number) => void,
    signal?: AbortSignal,
    onListing?: (found: number) => void,
  ): Promise<{ files: number; skipped: string[] }>;
}

export interface DownloadCallbacks {
  // File `i` of `n` (1-based), its name and how far it is (0–100).
  progress?: (i: number, n: number, name: string, pct: number) => void;
  // Listing the folder: `found` items so far.
  listing?: (name: string, found: number) => void;
}

const MAX_SUFFIX = 999;

// Reserve `<dir>/<name>`, or the first of `<name> (2)`, `<name> (3)`, … that can be created.
export async function reserveUniqueDir(dir: string, name: string, reserve: (p: string) => Promise<boolean>): Promise<string> {
  for (let k = 1; k <= MAX_SUFFIX; k++) {
    const p = joinPath(dir, k === 1 ? name : `${name} (${k})`);
    if (await reserve(p)) return p;
  }
  throw new Error(`${joinPath(dir, name)} exists`);
}

const pctOf = (done: number, total: number) => (total > 0 ? Math.round((done / total) * 100) : 0);

export interface DownloadResult {
  path: string;
  files: number;
  skipped: string[];
}

// Ask where, then download. Resolves null when the dialog was cancelled.
export async function downloadTo(item: DownloadItem, deps: DownloadToDeps, cb: DownloadCallbacks = {}, signal?: AbortSignal): Promise<DownloadResult | null> {
  if (!item.isDir) {
    const dest = await deps.saveDialog(item.name);
    if (!dest) return null;
    cb.progress?.(1, 1, item.name, 0);
    await deps.downloadFile(item.path, dest, (d, t) => cb.progress?.(1, 1, item.name, pctOf(d, t)), signal);
    return { path: dest, files: 1, skipped: [] };
  }
  const picked = await deps.folderDialog();
  if (!picked) return null;
  if (!isLocalName(item.name, pathStyle(picked))) throw new Error(`“${item.name}” is not a valid folder name on this computer`);
  if (signal?.aborted) return null;
  const dest = await reserveUniqueDir(picked, item.name, deps.reserveDir);
  cb.listing?.(item.name, 0);
  const r = await deps.downloadTree(item.path, dest, (done, total, current, f) => {
    if (current) cb.progress?.(Math.min(done + 1, total), total, baseName(current), Math.round((f ?? 0) * 100));
  }, signal, (found) => cb.listing?.(item.name, found));
  return { path: dest, files: r.files, skipped: r.skipped };
}
