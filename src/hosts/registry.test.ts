import { beforeEach, describe, expect, it, vi } from "vitest";

type Handler = (e: { payload: unknown }) => void;
const handlers: Record<string, Handler> = {};
const invoke = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...a: unknown[]) => invoke(...a) }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async (name: string, h: Handler) => { handlers[name] = h; return () => { delete handlers[name]; }; }) }));
vi.mock("@tauri-apps/plugin-store", () => ({ load: vi.fn() }));

import { HostRegistry } from "./registry";
import type { HostConfig, HostStatus, HostSnapshot } from "./types";

const H = "h_ab12cd34";
const cfg: HostConfig = { id: H, name: "Dev", sshTarget: "dev" };
const status = (s: HostStatus["status"], over: Partial<HostStatus> = {}): HostStatus => ({
  host: H, status: s, phase: null, lastError: null, errorHint: null, daemonVersion: "1.5.0", desktopVersion: "1.5.0",
  protocol: 1, os: "linux", arch: "x86_64", incompatibleReason: null, nextRetryAt: null, sinceMs: 0, ...over,
});
const deferred = <T,>() => { let resolve!: (v: T) => void; const p = new Promise<T>(r => { resolve = r; }); return { p, resolve }; };
const flush = () => new Promise(r => setTimeout(r, 0));

beforeEach(() => {
  invoke.mockReset();
  for (const k of Object.keys(handlers)) delete handlers[k];
});

describe("registry", () => {
  it("stays dormant with no hosts: no listeners, no Tauri calls", async () => {
    const r = new HostRegistry();
    await r.init([]);
    expect(invoke).not.toHaveBeenCalled();
    expect(Object.keys(handlers)).toEqual([]);
    expect(r.initialized).toBe(false);
  });

  it("init is single-flight (StrictMode-safe) and orders listen → configure → status", async () => {
    invoke.mockImplementation(async (cmd: string) => (cmd === "hosts_status" ? [] : undefined));
    const r = new HostRegistry();
    const p1 = r.init([cfg]);
    const p2 = r.init([cfg]);
    expect(p1).toBe(p2);
    await p1;
    expect(invoke.mock.calls.map(c => c[0])).toEqual(["hosts_configure", "hosts_status"]);
    expect(invoke.mock.calls[0][1]).toEqual({ hosts: [cfg] });
    expect(Object.keys(handlers).sort()).toEqual(["hosts:status", "hosts:terminals"]);
    r._dispose();
  });

  it("event during configure wins over the older snapshot", async () => {
    const configure = deferred<void>();
    invoke.mockImplementation((cmd: string) => {
      if (cmd === "hosts_configure") return configure.p;
      if (cmd === "hosts_status") return Promise.resolve([{ status: status("reconnecting"), terminals: null }] as HostSnapshot[]);
      return Promise.resolve();
    });
    const r = new HostRegistry();
    const done = r.init([cfg]);
    await flush();
    handlers["hosts:status"]({ payload: status("connected") });
    handlers["hosts:terminals"]({ payload: { host: H, list: [] } });
    configure.resolve();
    await done;
    expect(r.getStatus(H)?.status).toBe("connected");
    expect(r.getSnapshot().live[H]).toEqual([]);
    r._dispose();
  });

  it("snapshot resolving after an event is ignored for that host", async () => {
    const snap = deferred<HostSnapshot[]>();
    invoke.mockImplementation((cmd: string) => (cmd === "hosts_status" ? snap.p : Promise.resolve()));
    const r = new HostRegistry();
    const done = r.init([cfg]);
    await flush(); await flush();
    expect(invoke.mock.calls.map(c => c[0])).toContain("hosts_status");
    handlers["hosts:status"]({ payload: status("offline") });
    snap.resolve([{ status: status("connected"), terminals: [] }]);
    await done;
    expect(r.getStatus(H)?.status).toBe("offline");
    r._dispose();
  });

  it("snapshot applies when no event arrived", async () => {
    invoke.mockImplementation(async (cmd: string) => (cmd === "hosts_status" ? [{ status: status("connected"), terminals: null }] : cmd === "host_call" ? { installed: false } : undefined));
    const r = new HostRegistry();
    await r.init([cfg]);
    expect(r.isUsable(H)).toBe(true);
    expect(r.getSnapshot().live[H]).toBeNull();
    r._dispose();
  });

  it("waitUsable resolves on connect, rejects on abort and on removal", async () => {
    invoke.mockImplementation(async (cmd: string) => (cmd === "hosts_status" ? [] : cmd === "host_call" ? { installed: true } : undefined));
    const r = new HostRegistry();
    await r.init([cfg]);
    const w = r.waitUsable(H);
    handlers["hosts:status"]({ payload: status("upgrade-pending") });
    await expect(w).resolves.toBeUndefined();

    handlers["hosts:status"]({ payload: status("offline") });
    const ac = new AbortController();
    const w2 = r.waitUsable(H, ac.signal);
    ac.abort();
    await expect(w2).rejects.toMatchObject({ name: "AbortError" });

    const w3 = r.waitUsable(H);
    await r.configure([]);
    await expect(w3).rejects.toMatchObject({ code: "unknown-host" });
    r._dispose();
  });

  it("probes agents through host_call on the first usable transition", async () => {
    invoke.mockImplementation(async (cmd: string, args: any) => (cmd === "hosts_status" ? [] : cmd === "host_call" ? { installed: args.params.binary === "codex" } : undefined));
    const r = new HostRegistry();
    await r.init([cfg]);
    handlers["hosts:status"]({ payload: status("connected") });
    await flush();
    const calls = invoke.mock.calls.filter(c => c[0] === "host_call");
    expect(calls.every(c => c[1].method === "detect_agent_binary" && c[1].host === H)).toBe(true);
    expect(r.getSnapshot().agents[H].codex).toBe(true);
    expect(r.getSnapshot().agents[H].claude).toBe(false);
    r._dispose();
  });

  it("configGeneration change notifies re-attach listeners (amendment 20)", async () => {
    invoke.mockImplementation(async (cmd: string) => (cmd === "hosts_status" ? [] : cmd === "host_call" ? { installed: false } : undefined));
    const r = new HostRegistry();
    await r.init([cfg]);
    const seen: string[] = [];
    r.onReattach(h => seen.push(h));
    handlers["hosts:status"]({ payload: status("connected", { configGeneration: 1 }) });
    handlers["hosts:status"]({ payload: status("connected", { configGeneration: 1 }) });
    expect(seen).toEqual([]);
    handlers["hosts:status"]({ payload: status("reconnecting", { configGeneration: 2 }) });
    expect(seen).toEqual([H]);
    r._dispose();
  });

  it("configure on a dormant registry starts it", async () => {
    invoke.mockImplementation(async (cmd: string) => (cmd === "hosts_status" ? [] : undefined));
    const r = new HostRegistry();
    await r.init([]);
    await r.configure([cfg]);
    expect(invoke.mock.calls.map(c => c[0])).toEqual(["hosts_configure", "hosts_status"]);
    await r.configure([cfg, { ...cfg, id: "h_zz12cd34" }]);
    expect(invoke.mock.calls[invoke.mock.calls.length - 1]).toEqual(["hosts_configure", { hosts: [cfg, { ...cfg, id: "h_zz12cd34" }] }]);
    r._dispose();
  });
});
