import { invoke } from "@tauri-apps/api/core";
import type { HostErrorCode, HostId } from "./types";
import { isHostError } from "./types";
import { CACHEABLE_METHODS, type HostMethod } from "./methods";
import { cache, callKey } from "./cache";

// The one way to run a Host-side command.
//   • host undefined → `invoke(cmd, args)`, byte-identical to the pre-hosts code.
//   • Remote Host    → `invoke("host_call", { host, method, params })`.
// Errors: a Daemon-side failure (`remote`) rejects with its message string, exactly like a
// local command error. Connection problems reject with HostUnavailableError. Cacheable reads
// fall back to the last cached value when the Host is offline/incompatible/unknown.

export class HostUnavailableError extends Error {
  readonly code: HostErrorCode;
  readonly host: HostId;
  constructor(host: HostId, code: HostErrorCode, message: string) {
    super(message);
    this.name = "HostUnavailableError";
    this.code = code;
    this.host = host;
  }
}

// Result with provenance (amendment 24). `stale` = served from the cache, not a live call.
export interface Provenanced<T> { value: T; stale: boolean; at: number }

const FALLBACK_CODES: ReadonlySet<HostErrorCode> = new Set<HostErrorCode>(["offline", "incompatible", "unknown-host"]);

// Per-result staleness: a cacheable read served from the cache stays stale until a live
// fetch of the same (host, method, args) succeeds — regardless of the Host's status.
const staleKeys = new Set<string>();
const staleListeners = new Set<() => void>();
function setStale(k: string, stale: boolean) {
  const had = staleKeys.has(k);
  if (stale === had) return;
  if (stale) staleKeys.add(k); else staleKeys.delete(k);
  for (const l of staleListeners) l();
}
export function isStale(host: HostId | undefined, method: HostMethod, args?: Record<string, unknown>): boolean {
  return !!host && staleKeys.has(`${host}|${callKey(method, args)}`);
}
export function subscribeStale(l: () => void): () => void {
  staleListeners.add(l);
  return () => { staleListeners.delete(l); };
}

function localInvoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  // Keep the exact call shape of the pre-hosts code: no second argument when none was given.
  return args === undefined ? invoke<T>(cmd) : invoke<T>(cmd, args);
}

async function remoteCall<T>(host: HostId, cmd: HostMethod, args: Record<string, unknown> | undefined, allowCache: boolean): Promise<Provenanced<T>> {
  const cacheable = CACHEABLE_METHODS.has(cmd);
  const sk = `${host}|${callKey(cmd, args)}`;
  try {
    const value = await invoke<T>("host_call", { host, method: cmd, params: args ?? {} });
    if (cacheable) {
      cache.put(host, cmd, args, value);
      setStale(sk, false);
    }
    return { value, stale: false, at: Date.now() };
  } catch (e) {
    if (isHostError(e)) {
      if (e.code === "remote") throw e.message;
      if (allowCache && cacheable && FALLBACK_CODES.has(e.code)) {
        const hit = cache.get(host, cmd, args);
        if (hit) {
          setStale(sk, true);
          return { value: hit.v as T, stale: true, at: hit.at };
        }
      }
      throw new HostUnavailableError(host, e.code, e.message);
    }
    throw e;
  }
}

export async function hostInvoke<T>(host: HostId | undefined, cmd: HostMethod, args?: Record<string, unknown>): Promise<T> {
  if (!host) return localInvoke<T>(cmd, args);
  return (await remoteCall<T>(host, cmd, args, true)).value;
}

// Same as hostInvoke, but tells the caller whether the value came from the cache.
export async function hostQuery<T>(host: HostId | undefined, cmd: HostMethod, args?: Record<string, unknown>): Promise<Provenanced<T>> {
  if (!host) return { value: await localInvoke<T>(cmd, args), stale: false, at: Date.now() };
  return remoteCall<T>(host, cmd, args, true);
}

// Live results only — never the cache. Used by aggregates (usage strip, rate limits, cost).
export async function hostInvokeLive<T>(host: HostId | undefined, cmd: HostMethod, args?: Record<string, unknown>): Promise<T> {
  if (!host) return localInvoke<T>(cmd, args);
  return (await remoteCall<T>(host, cmd, args, false)).value;
}

// Tests only.
export function _resetStale() { staleKeys.clear(); }
