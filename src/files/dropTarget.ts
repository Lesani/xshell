// Where an OS drop onto the explorer goes (Lesani/xshell#46, D4), and which drags the
// explorer accepts. The explorer's rows carry `data-path` and `data-dir="1"` (folders).

// Dnd mime carrying an item's absolute path. The terminal body listens for a drop of this
// type and writes the path into the active PTY (TerminalTab's onDrop handler); the explorer
// ignores it.
export const DRAG_PATH_MIME = "application/x-xshell-path";

export interface RowData {
  path: string;
  isDir: boolean;
}

interface AttrSource {
  getAttribute(name: string): string | null;
}

// The explorer row an event target is in, if any.
export function rowData(target: { closest(selector: string): AttrSource | null } | null): RowData | null {
  const row = target?.closest("[data-path]") ?? null;
  const path = row?.getAttribute("data-path");
  if (!row || !path) return null;
  return { path, isDir: row.getAttribute("data-dir") === "1" };
}

// The folder a drop goes into: the folder row under the pointer, else the explorer's
// current folder (file rows included).
export function resolveDropTarget(row: RowData | null, cwd: string): string {
  return row?.isDir ? row.path : cwd;
}

// Accept files from the OS, but not an in-app row drag (it carries DRAG_PATH_MIME) and
// nothing while this window's own native drag-out runs (it would upload the file back).
export function acceptsDrop(types: ArrayLike<string> | Iterable<string> | null | undefined, dragOutActive: boolean): boolean {
  if (dragOutActive || !types) return false;
  const list = Array.from(types as ArrayLike<string>);
  return list.includes("Files") && !list.includes(DRAG_PATH_MIME);
}

// A local path as a file:// URI: POSIX `/a b` → `file:///a%20b`; Windows `C:\x` →
// `file:///C:/x`, UNC `\\srv\share\x` → `file://srv/share/x`, and `\\?\` long paths
// as their plain form. A POSIX backslash stays part of the name.
export function fileUri(path: string): string {
  const enc = (p: string) => p.split("/").map(encodeURIComponent).join("/");
  let p = path.replace(/^\\\\\?\\UNC\\/i, "\\\\").replace(/^\\\\\?\\(?=[a-zA-Z]:)/, "");
  if (/^[a-zA-Z]:[\\/]/.test(p)) {
    p = p.replace(/\\/g, "/");
    return "file:///" + p.slice(0, 2) + enc(p.slice(2));
  }
  if (p.startsWith("\\\\")) return "file://" + enc(p.slice(2).replace(/\\/g, "/"));
  return "file://" + enc(p);
}
