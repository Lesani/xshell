import { load } from "@tauri-apps/plugin-store";
import type { AvailableHost, HostConfig, HostStatus, HostId, TerminalInfo } from "./types";

// Offline cache for Remote Hosts: the last `terminals` list per Host plus the last result of
// each cacheable read call. Kept in its own store file so list churn never rewrites
// settings.json. Loaded at startup even without saved SSH: retained Ring data is stale,
// personal data, never membership or route authorization.

export const CACHE_FILE = "hosts-cache.json";
const CACHE_KEY = "cache";
export const CACHE_VERSION = 1;
export const MAX_CALLS_PER_HOST = 200;
const WRITE_DEBOUNCE_MS = 1000;

export interface CachedCall { v: unknown; at: number }
export interface HostCacheEntry {
  provenance?: { daemonKey?: string; savedTarget?: string; host?: HostId };
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
type Binding = { key: string; daemonKey?: string; savedTarget?: string; generation: string; aliases: string[] };
let bindings: Record<string, Binding> = {};
const retiredTargets = new Set<string>();
function binding(host: string): Binding {
  return bindings[host] ?? { key: host, generation: "", aliases: [host] };
}
function cachedEntry(host: string, field: string): HostCacheEntry | undefined {
  const hasField = (entry: HostCacheEntry) => field === "terminals" ? entry.terminalsAt > 0 : field in entry.calls;
  const b = binding(host);
  if (!bindings[host]) {
    const retained = Object.values(state.hosts).find(e => e.provenance?.host === host && hasField(e));
    if (retained) return retained;
  }
  if (!b.daemonKey && b.savedTarget) {
    const sameTarget = Object.values(state.hosts).find(e => e.provenance?.host === host && e.provenance?.savedTarget === b.savedTarget && hasField(e));
    if (sameTarget) return sameTarget;
  }
  for (const key of [b.key, ...b.aliases]) {
    const entry = state.hosts[key];
    if (!entry || !hasField(entry)) continue;
    if (entry.provenance?.daemonKey) {
      if (entry.provenance.daemonKey === b.daemonKey) return entry;
    } else if (entry.provenance?.savedTarget) {
      if (entry.provenance.savedTarget === b.savedTarget && !b.daemonKey) return entry;
    } else if (!b.daemonKey && !retiredTargets.has(host) && key === host) return entry;
  }
  return undefined;
}
function stamp(host: string) {
  const b = binding(host);
  const entry = state.hosts[b.key];
  const retainedHost = b.daemonKey ? b.aliases.find(alias => /^r_[0-9a-f]{64}$/.test(alias)) ?? host : host;
  if (entry) entry.provenance = { daemonKey: b.daemonKey, savedTarget: b.savedTarget, host: retainedHost };
}
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
  bindHosts(hosts: AvailableHost[], configs: HostConfig[], status: Record<string, HostStatus>) {
    const next: Record<string, Binding> = {};
    const ids = new Set([...hosts.flatMap(h => [h.id, ...h.aliases]), ...configs.map(c => c.id)]);
    for (const id of ids) {
      const row = hosts.find(h => h.id === id || h.aliases.includes(id));
      const config = configs.find(c => c.id === id);
      const savedTarget = config ? JSON.stringify([config.sshTarget, config.daemonCommand ?? ""]) : undefined;
      const old = bindings[id];
      if (old?.savedTarget && old.savedTarget !== savedTarget) retiredTargets.add(id);
      const daemonKey = row?.daemonKey ?? undefined;
      const s = status[row?.id ?? id];
      next[id] = { key: daemonKey ? `daemon:${daemonKey}` : savedTarget ? `saved:${id}:${savedTarget}` : id, daemonKey, savedTarget,
        aliases: row?.aliases ?? [id],
        generation: JSON.stringify([daemonKey, savedTarget, s?.configGeneration, s?.linkGeneration]) };
    }
    for (const [id, old] of Object.entries(bindings)) {
      if (!next[id]) next[id] = { ...old, generation: `withdrawn:${old.daemonKey ?? old.savedTarget ?? id}` };
    }
    bindings = next;
  },
  dataIdentity: (host: string): string => binding(host).key,
  capture: (host: string): string => binding(host).generation,
  isCurrent: (host: string, token: string): boolean => binding(host).generation === token,
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
  retainedHosts(): HostId[] {
    return [...new Set(Object.entries(state.hosts).map(([key, entry]) => entry.provenance?.host ?? key)
      .filter(id => id === "local" || /^(?:h_[a-z0-9]{8}|r_[0-9a-f]{64})$/.test(id)))];
  },
  getState: () => state,
  get(host: HostId, method: string, args?: Record<string, unknown>): CachedCall | undefined {
    return cachedEntry(host, callKey(method, args))?.calls[callKey(method, args)];
  },
  put(host: HostId, method: string, args: Record<string, unknown> | undefined, v: unknown) {
    state = putCall(state, binding(host).key, callKey(method, args), v, Date.now());
    stamp(host);
    scheduleWrite();
  },
  putTerminals(host: HostId, list: TerminalInfo[]) {
    state = putTerminals(state, binding(host).key, list, Date.now());
    stamp(host);
    scheduleWrite();
  },
  terminals(host: HostId): TerminalInfo[] | null {
    return cachedEntry(host, "terminals")?.terminals ?? null;
  },
  dropHost(host: HostId) {
    for (const [key, entry] of Object.entries(state.hosts)) {
      if (key === host || entry.provenance?.host === host) state = dropHost(state, key);
    }
    scheduleWrite();
  },
  // Tests only.
  _reset() {
    state = emptyCache();
    loaded = false;
    bindings = {};
    retiredTargets.clear();
    loadPromise = null;
    if (writeTimer) clearTimeout(writeTimer);
    writeTimer = null;
  },
};
