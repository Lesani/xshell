import { invoke } from "@tauri-apps/api/core";
import { HostUnavailableError } from "../hosts/hostInvoke";
import { registry } from "../hosts/registry";
import { SyncManager, messageOf, type CopyMeta, type CopyPaths, type CopyStat, type Snapshot } from "./syncBack";
import * as transfer from "./transfer";
import { styleOfOs } from "./paths";

// The sync-back manager wired to the Desktop commands of src-tauri/src/open_copies.rs, the
// transfer client and the Host registry.

// A remote `file_stat` of a path that does not exist (core transfer.rs: "Failed to stat
// <path>: No such file or directory (os error 2)").
const NOT_FOUND = /No such file or directory|\(os error 2\)/;

export const syncBack = new SyncManager({
  copyPaths: (host, remotePath) => invoke<CopyPaths>("open_copy_path", { host, remotePath }),
  statCopy: path => invoke<CopyStat | null>("open_copy_stat", { path }),
  hashCopy: async path => {
    const st = await transfer.stat(undefined, path, true);
    if (!st.version) throw new Error(`not a file: ${path}`);
    return st.version;
  },
  snapshot: payload => invoke<Snapshot>("open_copy_snapshot", { payload }),
  dropFile: path => invoke<void>("open_copy_release", { path, payload: null, expect: null }).catch(() => {}),
  install: (incoming, payload, expect) => invoke<CopyStat>("open_copy_install", { incoming, payload, expect }),
  download: (host, remotePath, localPath, onProgress) =>
    transfer.download(host, remotePath, localPath, onProgress, undefined, { private: true }),
  upload: (host, dir, localPath, name, ifVersion) =>
    transfer.upload(host, dir, { localPath }, { name, onClash: "replace", ifVersion }),
  remoteStat: async (host, path) => {
    try {
      const st = await transfer.stat(host, path, true);
      return st.version ?? "directory";
    } catch (e) {
      if (!(e instanceof HostUnavailableError) && NOT_FOUND.test(messageOf(e))) return "deleted";
      throw e;
    }
  },
  openLocal: path => invoke<void>("open_local_path", { path }),
  readMeta: dir => invoke<CopyMeta | null>("open_copy_meta", { dir, meta: null }),
  writeMeta: async (dir, meta) => { await invoke("open_copy_meta", { dir, meta }); },
  release: (dir, guard) => invoke<void>("open_copy_release", { path: dir, payload: guard?.payload ?? null, expect: guard?.expect ?? null }),
  isHostUnavailable: e => e instanceof HostUnavailableError,
  isFileChanged: transfer.isFileChanged,
  isOutcomeUnknown: transfer.isUploadOutcomeUnknown,
  hostUsable: host => registry.isUsable(host),
  hostPathStyle: host => styleOfOs(registry.getStatus(host)?.os),
  onHostUsable: cb => registry.onUsable(cb),
});

let restored = false;

// Startup: resume the copies whose changes were not uploaded before xshell quit.
export function restorePendingCopies(): void {
  if (restored) return;
  restored = true;
  invoke<CopyMeta[]>("open_copies_pending")
    .then(metas => syncBack.restorePending(metas))
    .catch(() => {});
}
