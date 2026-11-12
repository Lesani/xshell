import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const { invoke, FakeChannel } = vi.hoisted(() => ({
  invoke: vi.fn(),
  FakeChannel: class<T> {
    onmessage: (message: T) => void = () => {};
    emit(message: T) { this.onmessage(message); }
  },
}));
vi.mock("@tauri-apps/api/core", () => ({ invoke, Channel: FakeChannel }));

import { registry } from "./registry";
import { captureTerminalInput, mountTerminal, pendingOpens, remoteTerminals, RemoteTerminals, writeTerminalInput, type StartOptions } from "./terminalTransport";
import type { HostSnapshot, HostStatus, TerminalInfo } from "./types";
import type { Tab } from "../types";

const R = `r_${"a".repeat(64)}`, H = "h_ab12cd34", U = "00000000-0000-4000-8000-000000000001";
const opts: StartOptions = { cols: 80, rows: 24, shellMode: "raw", shellCommand: null, shellId: null, fullscreenRendering: true, forceSyncOutput: true };
const status = (kind: HostStatus["status"] = "connected", generation = 1): HostStatus => ({
  host: H, status: kind, phase: null, lastError: null, errorHint: null, daemonVersion: null, desktopVersion: "1.5.0", protocol: null, os: null, arch: null, incompatibleReason: null, nextRetryAt: null, sinceMs: 0, configGeneration: generation, linkGeneration: generation,
});
const terminal: TerminalInfo = { terminal: U, spec: { cwd: "/p" }, meta: {}, createdAtMs: 0, pid: 1, exitCode: null };
const snapshot = (s = status(), list: TerminalInfo[] | null = [terminal]): HostSnapshot[] => [{ status: s, terminals: list }];
const deferred = <T,>() => { let resolve!: (value: T) => void, reject!: (error: unknown) => void; const promise = new Promise<T>((a, b) => { resolve = a; reject = b; }); return { promise, resolve, reject }; };
const flush = async () => { for (let i = 0; i < 8; i++) await Promise.resolve(); };
const calls = (command: string) => invoke.mock.calls.filter(c => c[0] === command);
type Channels = { onData: InstanceType<typeof FakeChannel<ArrayBuffer>>; onExit: InstanceType<typeof FakeChannel<{ code: number; bytes: number }>>; onResync: InstanceType<typeof FakeChannel<null>> };
const channels = (index = 0) => calls("host_term_attach")[index][1] as Channels;
const sinks = () => ({ data: vi.fn(), exit: vi.fn(), reset: vi.fn() });
const controllers: AbortController[] = [];
let sequence = 100;
beforeEach(() => {
  invoke.mockReset();
  pendingOpens.clear();
  registry.applyAvailable({ sequence: ++sequence, hosts: [] });
  registry.applyAvailable({ sequence: ++sequence, hosts: [{ id: H, aliases: [H, R], name: "Host", color: null, daemonKey: "verified", routes: ["ssh"], activeRoute: "ssh", reachability: { kind: "online" } }] });
  registry.applyStatus(status());
  invoke.mockImplementation((command: string) => Promise.resolve(command === "hosts_status" ? snapshot() : { exitCode: null }));
});
afterEach(() => { registry.endRetarget(H); for (const controller of controllers.splice(0)) controller.abort(); remoteTerminals.forget(U); });
async function mounted(rt = new RemoteTerminals(), uuid = U) {
  const s = sinks(), abort = new AbortController(); controllers.push(abort);
  const gen = rt.mount(R, uuid, s);
  await rt.start(R, uuid, gen, opts, abort.signal);
  return { rt, s, abort, gen };
}

describe("broker viewer resync in the actual Terminal transport", () => {
  it("fences old bytes/exit/input immediately, reconciles once and attaches original r_+UUID only", async () => {
    const list = deferred<HostSnapshot[]>();
    const m = await mounted(remoteTerminals);
    const old = channels();
    const tab: Tab = { id: "view", type: "terminal", title: "Terminal", host: R, terminal: U };
    const before = captureTerminalInput(tab);
    invoke.mockImplementation((command: string) => command === "hosts_status" ? list.promise : Promise.resolve({ exitCode: null }));
    old.onResync.emit(null); old.onResync.emit(null);
    expect(remoteTerminals.inputBlocked(U, R)).toBe(true);
    old.onData.emit(new Uint8Array([99]).buffer); old.onExit.emit({ code: 0, bytes: 0 });
    const during = captureTerminalInput(tab);
    writeTerminalInput(before, "before"); writeTerminalInput(during, "during");
    await flush();
    expect(calls("hosts_status")).toHaveLength(1);
    expect(calls("host_term_attach")).toHaveLength(1);
    expect(m.s.data).not.toHaveBeenCalled(); expect(m.s.exit).not.toHaveBeenCalled();
    list.resolve(snapshot()); await flush();
    expect(calls("host_term_attach")).toHaveLength(2);
    expect(calls("host_term_attach")[1][1]).toMatchObject({ host: R, terminal: U });
    expect(remoteTerminals.inputBlocked(U, R)).toBe(false);
    expect(m.s.reset).toHaveBeenCalledTimes(1);
    old.onData.emit(new Uint8Array([98]).buffer); old.onResync.emit(null);
    channels(1).onData.emit(new Uint8Array([7]).buffer);
    writeTerminalInput(before, "late-paste"); writeTerminalInput(during, "late-paused-paste"); writeTerminalInput(captureTerminalInput(tab), "fresh");
    expect(m.s.data.mock.calls.map(c => [...c[0]])).toEqual([[7]]);
    expect(calls("host_term_input").map(c => c[1].data)).toEqual(["fresh"]);
    expect(calls("host_term_open")).toEqual([]); expect(calls("host_term_close")).toEqual([]); expect(calls("host_term_relaunch")).toEqual([]);
    expect(calls("host_term_detach")).toEqual([]);
  });

  it.each(["abort", "unmount", "forget"])("a deferred list cannot restart a %s viewer", async disposition => {
    const m = await mounted(), old = channels(), list = deferred<HostSnapshot[]>();
    invoke.mockImplementation((command: string) => command === "hosts_status" ? list.promise : Promise.resolve({ exitCode: null }));
    old.onResync.emit(null); await flush();
    if (disposition === "abort") m.abort.abort();
    else if (disposition === "unmount") m.rt.unmount(U, m.gen);
    else m.rt.forget(U);
    list.resolve(snapshot()); await flush(); old.onResync.emit(null); await flush();
    expect(calls("host_term_attach")).toHaveLength(1);
    expect(m.s.reset).not.toHaveBeenCalled();
  });

  it("resync after cancellation or from an old epoch cannot initiate work", async () => {
    const m = await mounted(), old = channels();
    await m.rt.reattach(R, U, m.gen, opts, m.abort.signal);
    old.onResync.emit(null); await flush();
    expect(calls("hosts_status")).toEqual([]);
    m.abort.abort(); channels(1).onResync.emit(null); await flush();
    expect(calls("hosts_status")).toEqual([]);
    expect(calls("host_term_attach")).toHaveLength(2);
  });

  it("offline waits for a usable event, then reconciles and recovers without polling", async () => {
    const m = await mounted(); registry.applyStatus(status("offline"));
    channels().onResync.emit(null); await flush();
    expect(calls("hosts_status")).toEqual([]);
    expect(m.rt.inputBlocked(U, R)).toBe(true);
    registry.applyStatus(status()); await flush();
    expect(calls("hosts_status")).toHaveLength(1);
    expect(calls("host_term_attach")).toHaveLength(2);
    expect(m.rt.inputBlocked(U, R)).toBe(false);
  });

  it("a transient attach failure waits for a newer usable status before recovering", async () => {
    const m = await mounted();
    invoke.mockImplementation((command: string) => {
      if (command === "hosts_status") return Promise.resolve(snapshot());
      if (command === "host_term_attach" && calls(command).length === 2) return Promise.reject({ code: "offline", message: "link replaced" });
      return Promise.resolve({ exitCode: null });
    });
    channels().onResync.emit(null); await flush();
    expect(calls("host_term_attach")).toHaveLength(2);
    expect(m.rt.inputBlocked(U, R)).toBe(true);
    await flush(); expect(calls("hosts_status")).toHaveLength(1);
    registry.applyStatus(status()); await flush();
    expect(calls("host_term_attach")).toHaveLength(3);
    expect(m.rt.inputBlocked(U, R)).toBe(false);
  });

  it.each(["unlisted", "revoked", "stale-generation"])("%s observations never authorize an Attach", async disposition => {
    const m = await mounted(), list = deferred<HostSnapshot[]>();
    invoke.mockImplementation((command: string) => command === "hosts_status" ? list.promise : Promise.resolve({ exitCode: null }));
    channels().onResync.emit(null); await flush();
    if (disposition === "revoked") registry.applyAvailable({ sequence: ++sequence, hosts: [] });
    if (disposition === "stale-generation") registry.applyStatus(status("connected", 2));
    list.resolve(snapshot(status(), disposition === "unlisted" ? [] : [terminal])); await flush();
    expect(calls("host_term_attach")).toHaveLength(1);
    expect(m.rt.inputBlocked(U, R)).toBe(true);
    expect(m.s.reset).not.toHaveBeenCalled(); expect(m.s.exit).not.toHaveBeenCalled();
    if (disposition === "revoked") expect(registry.isUsable(R)).toBe(false);
  });

  it("revocation while offline rejects the wait and cannot recover without renewed authority", async () => {
    const m = await mounted(); registry.applyStatus(status("offline"));
    channels().onResync.emit(null); await flush();
    registry.applyAvailable({ sequence: ++sequence, hosts: [] }); await flush();
    expect(calls("hosts_status")).toEqual([]); expect(calls("host_term_attach")).toHaveLength(1);
    // A subsequent authorized event can use the existing reattach path; no old h_ fallback.
    registry.applyAvailable({ sequence: ++sequence, hosts: [{ id: H, aliases: [H, R], name: "Host", color: null, daemonKey: "verified", routes: ["ssh"], activeRoute: "ssh", reachability: { kind: "online" } }] });
    registry.applyStatus(status());
    await m.rt.reattach(R, U, m.gen, opts, m.abort.signal);
    expect(calls("host_term_attach")[1][1]).toMatchObject({ host: R, terminal: U });
    expect(m.rt.inputBlocked(U, R)).toBe(false);
  });

  it("two window transports of the same UUID keep channels, input state and replay isolated", async () => {
    const first = await mounted(), second = await mounted();
    const a = channels(0), b = channels(1);
    a.onResync.emit(null);
    expect(first.rt.inputBlocked(U, R)).toBe(true); expect(second.rt.inputBlocked(U, R)).toBe(false);
    b.onData.emit(new Uint8Array([42]).buffer); b.onExit.emit({ code: 0, bytes: 1 });
    await flush();
    expect(first.s.reset).toHaveBeenCalledTimes(1); expect(second.s.reset).not.toHaveBeenCalled();
    expect(second.s.data.mock.calls.map(c => [...c[0]])).toEqual([[42]]); expect(second.s.exit).toHaveBeenCalledWith(0);
    expect(calls("host_term_attach")).toHaveLength(3);
    expect(calls("host_term_detach")).toEqual([]);
  });

  it.each(["sent", "indeterminate"])("resync during an Open with %s outcome only recovers by Attach", async outcome => {
    const rt = new RemoteTerminals(), open = deferred<{ pid: number }>(), s = sinks(), abort = new AbortController(); controllers.push(abort);
    pendingOpens.set(U, { host: R, spec: { cwd: "/p" }, meta: {}, state: "opening" });
    invoke.mockImplementation((command: string) => command === "host_term_open" ? open.promise : Promise.resolve(command === "hosts_status" ? snapshot() : { exitCode: null }));
    const gen = rt.mount(R, U, s), start = rt.start(R, U, gen, opts, abort.signal).catch(() => null);
    const old = calls("host_term_open")[0][1] as Channels;
    old.onResync.emit(null); old.onResync.emit(null); await flush();
    expect(calls("hosts_status")).toEqual([]);
    if (outcome === "sent") open.resolve({ pid: 1 }); else open.reject({ code: "indeterminate", message: "unknown outcome" });
    await start; await flush();
    expect(calls("host_term_open")).toHaveLength(1); expect(calls("host_term_attach")).toHaveLength(1);
    expect(calls("host_term_attach")[0][1]).toMatchObject({ host: R, terminal: U });
    expect(calls("host_term_close")).toEqual([]); expect(rt.inputBlocked(U, R)).toBe(false);
    expect(pendingOpens.has(U)).toBe(false);
    await rt.reattach(R, U, gen, opts, abort.signal);
    expect(calls("host_term_open")).toHaveLength(1); expect(calls("host_term_attach")).toHaveLength(2);
  });

  it("unmount during a recovery Attach fences its channels and a remount waits before recovering", async () => {
    const m = await mounted(), attach = deferred<{ exitCode: null }>();
    invoke.mockImplementation((command: string) => command === "hosts_status" ? Promise.resolve(snapshot()) : command === "host_term_attach" && calls(command).length === 2 ? attach.promise : Promise.resolve({ exitCode: null }));
    channels().onResync.emit(null); await flush();
    const retired = channels(1);
    m.rt.unmount(U, m.gen);
    const fresh = sinks(), gen = m.rt.mount(R, U, fresh), abort = new AbortController(); controllers.push(abort);
    const start = m.rt.start(R, U, gen, opts, abort.signal);
    retired.onData.emit(new Uint8Array([8]).buffer); retired.onResync.emit(null);
    await flush(); expect(calls("host_term_attach")).toHaveLength(2);
    attach.resolve({ exitCode: null }); await start; await flush();
    expect(calls("host_term_attach")).toHaveLength(3);
    expect(fresh.data).not.toHaveBeenCalled();
    channels(2).onData.emit(new Uint8Array([9]).buffer);
    expect(fresh.data.mock.calls.map(c => [...c[0]])).toEqual([[9]]);
  });

  it("retarget waits for a cancelled recovery Attach before Detach and never resurrects its channel", async () => {
    const m = await mounted(), attach = deferred<{ exitCode: null }>();
    invoke.mockImplementation((command: string) => command === "hosts_status" ? Promise.resolve(snapshot()) : command === "host_term_attach" ? attach.promise : Promise.resolve());
    channels().onResync.emit(null); await flush();
    const recovery = channels(1), rebind = m.rt.prepareIdentityRebind(H, R, [U]);
    await flush(); expect(calls("host_term_detach")).toEqual([]);
    recovery.onData.emit(new Uint8Array([8]).buffer); recovery.onResync.emit(null);
    attach.resolve({ exitCode: null }); await rebind; await flush();
    expect(calls("host_term_detach")).toEqual([["host_term_detach", { host: H, terminal: U }]]);
    expect(calls("host_term_attach")).toHaveLength(2); expect(m.s.data).not.toHaveBeenCalled();
  });

  it("abort during recovery Attach drops even fresh channel output and never resumes work", async () => {
    const m = await mounted(), attach = deferred<{ exitCode: null }>();
    invoke.mockImplementation((command: string) => command === "hosts_status" ? Promise.resolve(snapshot()) : attach.promise);
    channels().onResync.emit(null); await flush(); m.abort.abort();
    channels(1).onData.emit(new Uint8Array([8]).buffer); channels(1).onResync.emit(null);
    attach.resolve({ exitCode: null }); await flush();
    expect(m.s.data).not.toHaveBeenCalled(); expect(m.rt.inputBlocked(U, R)).toBe(true);
    expect(calls("host_term_attach")).toHaveLength(2); expect(calls("hosts_status")).toHaveLength(1);
  });

  it("a local replay reset does not count toward the fresh channel's remote exit watermark", async () => {
    const m = await mounted();
    channels().onResync.emit(null); await flush();
    expect(m.s.reset).toHaveBeenCalledTimes(1);
    const replay = channels(1);
    replay.onExit.emit({ code: 9, bytes: 3 });
    replay.onData.emit(new Uint8Array([1, 2]).buffer);
    expect(m.s.exit).not.toHaveBeenCalled();
    replay.onData.emit(new Uint8Array([3]).buffer);
    expect(m.s.data.mock.calls.map(call => [...call[0]])).toEqual([[1, 2], [3]]);
    expect(m.s.exit).toHaveBeenCalledExactlyOnceWith(9);
  });

  it.each(["listed", "unlisted"])("a guard installed during inventory fetch requires a new %s observation before Attach", async membership => {
    const m = await mounted(), first = deferred<HostSnapshot[]>(), fresh = deferred<HostSnapshot[]>();
    invoke.mockImplementation((command: string) => command === "hosts_status" ? calls(command).length === 1 ? first.promise : fresh.promise : Promise.resolve({ exitCode: null }));
    channels().onResync.emit(null); await flush();
    registry.beginRetarget(H);
    first.resolve(snapshot()); await flush();
    expect(calls("host_term_attach")).toHaveLength(1);
    expect(calls("hosts_status")).toHaveLength(1);
    expect(m.s.reset).not.toHaveBeenCalled();
    registry.endRetarget(H); await flush();
    expect(calls("hosts_status")).toHaveLength(2);
    expect(calls("host_term_attach")).toHaveLength(1);
    fresh.resolve(snapshot(status(), membership === "listed" ? [terminal] : [])); await flush();
    expect(calls("host_term_attach")).toHaveLength(membership === "listed" ? 2 : 1);
    if (membership === "listed") expect(calls("host_term_attach")[1][1]).toMatchObject({ host: R, terminal: U });
    expect(calls("host_term_open")).toEqual([]); expect(calls("host_term_close")).toEqual([]);
  });

  it("a replacement guard installed before the waiter resumes also blocks fresh inventory and Attach", async () => {
    const m = await mounted(), first = deferred<HostSnapshot[]>();
    invoke.mockImplementation((command: string) => command === "hosts_status" ? calls(command).length === 1 ? first.promise : Promise.resolve(snapshot()) : Promise.resolve({ exitCode: null }));
    channels().onResync.emit(null); await flush();
    registry.beginRetarget(H); first.resolve(snapshot()); await flush();
    expect(calls("host_term_attach")).toHaveLength(1);
    registry.endRetarget(H); registry.beginRetarget(H); await flush();
    expect(calls("hosts_status")).toHaveLength(1); expect(calls("host_term_attach")).toHaveLength(1);
    registry.endRetarget(H); await flush();
    expect(calls("hosts_status")).toHaveLength(2); expect(calls("host_term_attach")).toHaveLength(2);
    expect(calls("host_term_attach")[1][1]).toMatchObject({ host: R, terminal: U });
    expect(m.rt.inputBlocked(U, R)).toBe(false);
  });

  it.each(["abort", "unmount"])("%s while waiting for a newly installed inventory guard prevents all later recovery work", async cancel => {
    const m = await mounted(), first = deferred<HostSnapshot[]>();
    invoke.mockImplementation((command: string) => command === "hosts_status" ? first.promise : Promise.resolve({ exitCode: null }));
    channels().onResync.emit(null); await flush();
    registry.beginRetarget(H); first.resolve(snapshot()); await flush();
    expect(calls("host_term_attach")).toHaveLength(1);
    if (cancel === "abort") m.abort.abort(); else m.rt.unmount(U, m.gen);
    registry.endRetarget(H); await flush();
    expect(calls("hosts_status")).toHaveLength(1); expect(calls("host_term_attach")).toHaveLength(1);
    expect(m.s.reset).not.toHaveBeenCalled();
  });

  it("mountTerminal carries the optional callback on the actual native Attach", async () => {
    const s = sinks(), abort = new AbortController(); controllers.push(abort);
    const tab: Tab = { id: "view", type: "terminal", title: "Terminal", host: R, terminal: U };
    const terminal = mountTerminal(tab, s);
    await terminal.start(opts, abort.signal);
    expect(channels().onResync).toBeInstanceOf(FakeChannel);
    channels().onResync.emit(null); await flush();
    expect(calls("host_term_attach")).toHaveLength(2); expect(s.reset).toHaveBeenCalledTimes(1);
    terminal.end();
  });
});
