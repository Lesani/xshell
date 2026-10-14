// Path helpers for Host-side paths, parsed by the path rules of the Host that owns them.
// A POSIX Host (Linux, macOS) separates with `/` only; a backslash is an ordinary filename
// character there. A Windows Host (the Local Host or a Remote one) uses `\` (and accepts
// `/`), with drive roots like `C:\` and UNC paths like `\\server\share`.

export type PathStyle = "posix" | "windows";

// The style of `p`: the Host's own when known, else inferred from the path's form (a drive
// letter or a UNC prefix is Windows; anything else, notably a leading "/", is POSIX).
export function pathStyle(p: string, known?: PathStyle): PathStyle {
  if (known) return known;
  return /^[a-zA-Z]:([\\/]|$)/.test(p) || p.startsWith("\\\\") ? "windows" : "posix";
}

// The separator `base` uses.
export function hostSep(base: string, style?: PathStyle): "/" | "\\" {
  if (pathStyle(base, style) === "posix") return "/";
  return base.includes("/") && !base.includes("\\") ? "/" : "\\";
}

// Join path components onto `base` with its own separator.
export function joinPath(base: string, ...parts: string[]): string {
  const sep = hostSep(base);
  const trimmed = base.endsWith(sep) ? base.slice(0, -1) : base;
  return [trimmed, ...parts].join(sep);
}

function trimSeps(p: string, style: PathStyle): string {
  return style === "windows" ? p.replace(/[\\/]+$/, "") : p.replace(/\/+$/, "");
}

function lastSep(t: string, style: PathStyle): number {
  return style === "windows" ? Math.max(t.lastIndexOf("\\"), t.lastIndexOf("/")) : t.lastIndexOf("/");
}

// The last component ("C:\" -> "C:"; "/srv/a\b.txt" -> "a\b.txt").
export function baseName(p: string, style?: PathStyle): string {
  const st = pathStyle(p, style);
  const t = trimSeps(p, st);
  const i = lastSep(t, st);
  const b = i >= 0 ? t.slice(i + 1) : t;
  return b || t;
}

// Parent directory, or null when already at a filesystem root (drive root on Windows, "/").
export function parentOf(p: string, style?: PathStyle): string | null {
  const st = pathStyle(p, style);
  const t = trimSeps(p, st);
  const i = lastSep(t, st);
  if (i < 0) return null;          // "C:" — no parent
  if (i === 0) return "/";         // unix "/home" -> "/"
  const parent = t.slice(0, i);
  return st === "windows" && /^[a-zA-Z]:$/.test(parent) ? parent + "\\" : parent; // "C:" -> "C:\"
}

// The path rules of a Host OS as the Host reports it ("windows", "linux", "macos"), if known.
export function styleOfOs(os: string | null | undefined): PathStyle | undefined {
  if (!os) return undefined;
  return os.toLowerCase().startsWith("win") ? "windows" : "posix";
}

// The hidden part file of an upload in progress (`.<name>.xshell-part-<12 hex>`, written by
// crates/core transfer.rs). The explorer does not list these.
export function isTransferPart(name: string): boolean {
  return /^\..*\.xshell-part-[0-9a-f]{12}$/.test(name);
}
