import { fmt } from "./hosts/strings";

// Build identity (Lesani/xshell#49): which build of a version a Desktop or Daemon is. The Rust
// side is `xshell_protocol::build::BuildId`; `null` means unknown (built without git, or a
// Daemon from before #49).
export interface BuildId {
  // The first 12 hex digits of the commit.
  commit: string;
  // 12 hex digits of the digest of uncommitted changes; null when the tree was clean.
  dirty: string | null;
  // The commit time, unix seconds. Diagnostic only: `sameBuild` ignores it.
  time: number;
}

// `85ee920a1b2c`, or `85ee920a1b2c+d.3f9a01c2b4e5` with uncommitted changes.
export function formatBuild(b: BuildId): string {
  return b.dirty ? `${b.commit}+d.${b.dirty}` : b.commit;
}

// The same build: commit and digest equal. Unknown is never the same as anything.
export function sameBuild(a: BuildId | null | undefined, b: BuildId | null | undefined): boolean {
  return !!a && !!b && a.commit === b.commit && (a.dirty ?? null) === (b.dirty ?? null);
}

// `85ee920a1b2c+d.3f9a01c2b4e5` as is, or `unknown build`.
export function buildText(b: BuildId | null | undefined): string {
  return b ? formatBuild(b) : fmt("hosts.build.unknown");
}

// `1.5.0 (85ee920a1b2c)`, `1.5.0 (85ee920a1b2c+d.3f9a01c2b4e5)`, or `1.5.0 (unknown build)`.
export function versionWithBuild(version: string, b: BuildId | null): string {
  return `${version} (${buildText(b)})`;
}

// The `app_build` command.
export interface AppBuild {
  version: string;
  build: BuildId | null;
}
