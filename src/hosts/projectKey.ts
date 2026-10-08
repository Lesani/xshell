import type { HostId } from "./types";
import { normalizePath } from "../utils";

// A Project is identified by (Host, path). This module is the only place that knows the
// key format:
//   • Local Project  → the bare path (no migration of stored `project_paths`,
//                      `sidebar_layout` or `project_icons`).
//   • Remote Project → `host:<hostId>:<absPath>`.
// A real local path never starts with `host:` — local paths are absolute (`/…`, `C:\…`, `\\…`).
export type ProjectKey = string & { readonly __projectKey: unique symbol };

// Single-sourced Host id format (amendment 3). The Rust side enforces the same pattern in
// `config::validate`; hostMethods.test.ts asserts the two strings are equal.
export const HOST_ID_PATTERN = "^h_[a-z0-9]{8}$";
export const HOST_ID_RE = new RegExp(HOST_ID_PATTERN);
const REMOTE_KEY_RE = new RegExp(`^host:(${HOST_ID_PATTERN.slice(1, -1)}):(.+)$`, "s");

export function isHostId(id: string): boolean {
  return HOST_ID_RE.test(id);
}

// `h_` + 8 chars of [a-z0-9].
export function newHostId(): HostId {
  const alphabet = "abcdefghijklmnopqrstuvwxyz0123456789";
  const bytes = new Uint8Array(8);
  crypto.getRandomValues(bytes);
  let out = "h_";
  for (const b of bytes) out += alphabet[b % alphabet.length];
  return out;
}

export function toProjectKey(host: HostId | undefined, path: string): ProjectKey {
  return (host ? `host:${host}:${path}` : path) as ProjectKey;
}

export function parseProjectKey(key: string): { host?: HostId; path: string } {
  const m = REMOTE_KEY_RE.exec(key);
  if (m) return { host: m[1], path: m[2] };
  return { path: key };
}

// Store-load boundary only: values read from settings.json are already keys.
export function asProjectKey(stored: string): ProjectKey {
  return stored as ProjectKey;
}

export const keyOf = (p: { host?: HostId; path: string }): ProjectKey => toProjectKey(p.host, p.path);

export const keyOfTab = (t: { host?: HostId; projectPath?: string }): ProjectKey | null =>
  t.projectPath ? toProjectKey(t.host, t.projectPath) : null;

// Keeps today's case-insensitive comparison. Host ids are lower-case, so lower-casing a
// remote key never changes its host part.
export const sameKey = (a: string, b: string): boolean => a.toLowerCase() === b.toLowerCase();

// Index for `projectIcons` / active-count maps (lower-cased, like today's path keys).
export const lookupKey = (k: ProjectKey | string): string => k.toLowerCase();

// The encoded Claude project-dir name: Claude's recorded one, else the Rust encoding of the
// raw on-Host path. Never derived from the key.
export function encodedNameFor(p: { encoded_name?: string; path: string }): string {
  return p.encoded_name || p.path.replace(/[^a-zA-Z0-9]/g, "-");
}

// Basename of the path a key points at (sidebar/name fallbacks).
export function keyBasename(key: string): string {
  const { path } = parseProjectKey(key);
  return path.split(/[\\/]/).filter(Boolean).pop() || path;
}

// ── Session identity (amendment 19) ─────────────────────────────────
// Runtime identity of a session for dedup, tab claims, open-state maps and React keys.
// RPCs and stored per-project folders keep the raw id.
export function sessionKey(host: HostId | undefined, id: string): string {
  return host ? `${host}:${id}` : id;
}
export const sessionKeyOf = (s: { host?: HostId; id: string }): string => sessionKey(s.host, s.id);
export const sessionKeyOfTab = (t: { host?: HostId; sessionId?: string }): string | null =>
  t.sessionId ? sessionKey(t.host, t.sessionId) : null;

// ── Project picker checked state (amendment 25) ──────────────────────
// Within one Host, compare with today's `normalizePath` (separators, trailing slashes,
// case). Local is exactly today's comparison.
export function isPinned(savedKeys: readonly string[], host: HostId | undefined, path: string): boolean {
  const target = normalizePath(path);
  return savedKeys.some(k => {
    const parsed = parseProjectKey(k);
    return (parsed.host ?? undefined) === (host ?? undefined) && normalizePath(parsed.path) === target;
  });
}
