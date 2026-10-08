import { load } from "@tauri-apps/plugin-store";
import type { HostId, TerminalInfo } from "./types";

// Offline cache for Remote Hosts: the last `terminals` list per Host plus the last result of
// each cacheable read call. Kept in its own store file so list churn never rewrites
// settings.json. Loaded only when at least one Host is configured.

export const CACHE_FILE = "hosts-cache.json";
const CACHE_KEY = "cache";
export const CACHE_VERSION = 1;
export const MAX_CALLS_PER_HOST = 200;
const WRITE_DEBOUNCE_MS = 1000;

export interface CachedCall { v: unknown; at: number }
export interface HostCacheEntry {
  terminals: TerminalInfo[];
  terminalsAt: number;
  calls: Record<string, CachedCall>;
}
export interface CacheState {
  version: 1;
  hosts: Record<HostId, HostCacheEntry>;
}

export function emptyCache(): CacheState {
  return { version: CACHE_VERSION, hosts: {} };
}

// Anything that is not a version-1 cache is ignored (treated as empty).
export function parseCache(raw: unknown): CacheState {
  if (!raw || typeof raw !== "object") return emptyCache();
  const r = raw as Partial<CacheState>;
  if (r.version !== CACHE_VERSION || !r.hosts || typeof r.hosts !== "object") return emptyCache();
  return { version: CACHE_VERSION, hosts: r.hosts };
}

// Stable JSON: object keys sorted at every level, so `{a,b}` and `{b,a}` share a key.
export function stableStringify(v: unknown): string {
  if (v === undefined) return "null";
  if (v === null || typeof v !== "object") return JSON.stringify(v);
  if (Array.isArray(v)) return `[${v.map(stableStringify).join(",")}]`;
  const obj = v as Record<string, unknown>;
  const keys = Object.keys(obj).filter(k => obj[k] !== undefined).sort();
  return `{${keys.map(k => `${JSON.stringify(k)}:${stableStringify(obj[k])}`).join(",")}}`;
}

export function callKey(method: string, args?: Record<string, unknown>): string {
  return `${method}:${stableStringify(args ?? {})}`;
}

function entryOf(state: CacheState, host: HostId): HostCacheEntry {
  return state.hosts[host] ?? { terminals: [], terminalsAt: 0, calls: {} };
}

// Insert (or refresh) a call result; the entry moves to the most-recent end. Oldest entries
// beyond MAX_CALLS_PER_HOST are evicted.
export function putCall(state: CacheState, host: HostId, key: string, v: unknown, at: number): CacheState {
  const entry = entryOf(state, host);
  const calls: Record<string, CachedCall> = {};
  for (const [k, c] of Object.entries(entry.calls)) if (k !== key) calls[k] = c;
  calls[key] = { v, at };
  const keys = Object.keys(calls);
  for (let i = 0; i < keys.length - MAX_CALLS_PER_HOST; i++) delete calls[keys[i]];
  return { ...state, hosts: { ...state.hosts, [host]: { ...entry, calls } } };
}

export function getCall(state: CacheState, host: HostId, key: string): CachedCall | undefined {
  return state.hosts[host]?.calls[key];
}

export function putTerminals(state: CacheState, host: HostId, list: TerminalInfo[], at: number): CacheState {
  const entry = entryOf(state, host);
  return { ...state, hosts: { ...state.hosts, [host]: { ...entry, terminals: list, terminalsAt: at } } };
}

export function dropHost(state: CacheState, host: HostId): CacheState {
  if (!(host in state.hosts)) return state;
  const hosts = { ...state.hosts };
  delete hosts[host];
  return { ...state, hosts };
}

// ── Runtime cache (module singleton) ────────────────────────────────

let state: CacheState = emptyCache();
let loaded = false;
let loadPromise: Promise<CacheState> | null = null;
let writeTimer: ReturnType<typeof setTimeout> | null = null;

async function storeHandle() {
  return load(CACHE_FILE, { defaults: {}, autoSave: false });
}

function scheduleWrite() {
  if (!loaded) return;
  if (writeTimer) clearTimeout(writeTimer);
  writeTimer = setTimeout(async () => {
    writeTimer = null;
    try {
      const store = await storeHandle();
      await store.set(CACHE_KEY, state);
      await store.save();
    } catch (_) {}
  }, WRITE_DEBOUNCE_MS);
}

export const cache = {
  // Loads the cache file once. Merges anything written before the load finished.
  load(): Promise<CacheState> {
    if (!loadPromise) {
      loadPromise = (async () => {
        let fromDisk = emptyCache();
        try {
          const store = await storeHandle();
          fromDisk = parseCache(await store.get(CACHE_KEY));
        } catch (_) {}
        // Writes made before the load landed win over the disk copy.
        state = { version: CACHE_VERSION, hosts: { ...fromDisk.hosts, ...state.hosts } };
        loaded = true;
        return state;
      })();
    }
    return loadPromise;
  },
  isLoaded: () => loaded,
  getState: () => state,
  get(host: HostId, method: string, args?: Record<string, unknown>): CachedCall | undefined {
    return getCall(state, host, callKey(method, args));
  },
  put(host: HostId, method: string, args: Record<string, unknown> | undefined, v: unknown) {
    state = putCall(state, host, callKey(method, args), v, Date.now());
    scheduleWrite();
  },
  putTerminals(host: HostId, list: TerminalInfo[]) {
    state = putTerminals(state, host, list, Date.now());
    scheduleWrite();
  },
  terminals(host: HostId): TerminalInfo[] | null {
    return state.hosts[host]?.terminals ?? null;
  },
  dropHost(host: HostId) {
    state = dropHost(state, host);
    scheduleWrite();
  },
  // Tests only.
  _reset() {
    state = emptyCache();
    loaded = false;
    loadPromise = null;
    if (writeTimer) clearTimeout(writeTimer);
    writeTimer = null;
  },
};
