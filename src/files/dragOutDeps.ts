import { Channel, invoke } from "@tauri-apps/api/core";
import { DragOut } from "./dragOut";
import * as transfer from "./transfer";

// The drag-out controller wired to the Desktop commands of src-tauri/src/drag_out.rs and
// open_copies.rs (the drag area) and the transfer client.

type DragEvent = { event: "failed"; error: string } | { event: "ended"; dropped: boolean };

export const dragOut = new DragOut({
  stat: (host, path, withVersion) => transfer.stat(host, path, withVersion),
  preparePath: (host, remotePath) => invoke<string>("drag_copy_path", { host, remotePath }),
  download: (host, path, local, onProgress) => transfer.download(host, path, local, onProgress),
  release: (path) => invoke<void>("drag_copy_release", { path }),
  start: (paths) =>
    new Promise<boolean>((resolve, reject) => {
      const onEvent = new Channel<DragEvent>();
      onEvent.onmessage = (m) => (m.event === "failed" ? reject(new Error(m.error)) : resolve(m.dropped));
      invoke("drag_out_start", { paths, onEvent }).catch(reject);
    }),
  now: () => Date.now(),
  setTimeout: (fn, ms) => window.setTimeout(fn, ms),
  clearTimeout: (h) => window.clearTimeout(h as number),
});
