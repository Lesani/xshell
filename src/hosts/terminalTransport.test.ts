import { beforeEach, describe, expect, it, vi } from "vitest";

// Channel stand-in: tests push messages with `.emit()`.
const { FakeChannel } = vi.hoisted(() => {
  class FakeChannel<T> {
    static all: FakeChannel<unknown>[] = [];
    onmessage: (m: T) => void = () => {};
    constructor() { FakeChannel.all.push(this as FakeChannel<unknown>); }
    emit(m: T) { this.onmessage(m); }
  }
  return { FakeChannel };
});
type Ch<T> = { onmessage: (m: T) => void; emit(m: T): void };
const ch = <T,>(i: number) => FakeChannel.all[i] as unknown as Ch<T>;
const invoke = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...a: unknown[]) => invoke(...a), Channel: FakeChannel }));

import { RemoteTerminals, RetryableStartError, localShellOptions, mountTerminal, markClosing, pendingOpens, writeTerminal, resizeTerminal, type StartOptions } from "./terminalTransport";
import type { Tab } from "../types";

const H = "h_ab12cd34";
const opts: StartOptions = { cols: 80, rows: 24, shellMode: "claude", shellCommand: null, shellId: null, fullscreenRendering: true, forceSyncOutput: true };
const deferred = <T,>() => { let resolve!: (v: T) => void, reject!: (e: unknown) => void; const p = new Promise<T>((a, b) => { resolve = a; reject = b; }); return { p, resolve, reject }; };
const flush = () => new Promise(r => setTimeout(r, 0));
const sinks = () => { const data: Uint8Array[] = []; const exits: number[] = []; return { data, exits, s: { data: (b: Uint8Array) => data.push(b), exit: (c: number) => exits.push(c) } }; };
const calls = (cmd: string) => invoke.mock.calls.filter(c => c[0] === cmd);

beforeEach(() => {
  invoke.mockReset();
  pendingOpens.clear();
  FakeChannel.all = [];
});

describe("remote terminal lifecycle (amendment 16)", () => {
  it("close-before-connect never calls host_term_open", async () => {
    const rt = new RemoteTerminals();
    pendingOpens.set("u1", { host: H, spec: { cwd: "/p" }, meta: {}, state: "opening" });
    const gen = rt.mount(H, "u1", sinks().s);
    const ac = new AbortController();
    // The Tab is waiting for its Host; the user closes it.
    await rt.close(H, "u1");
    ac.abort();
    rt.unmount("u1", gen);
    expect(await rt.start(H, "u1", gen, opts, ac.signal)).toBeNull();
    expect(calls("host_term_open")).toEqual([]);
    expect(calls("host_term_close")).toEqual([]);
    expect(pendingOpens.has("u1")).toBe(false);
  });

  it("delayed attach resolving after unmount is detached", async () => {
    const rt = new RemoteTerminals();
    const attach = deferred<{ exitCode: null }>();
    invoke.mockImplementation((cmd: string) => (cmd === "host_term_attach" ? attach.p : Promise.resolve()));
    const gen = rt.mount(H, "u2", sinks().s);
    const started = rt.start(H, "u2", gen, opts);
    rt.unmount("u2", gen);
    expect(calls("host_term_detach")).toEqual([]);
    attach.resolve({ exitCode: null });
    expect(await started).toBeNull();
    await flush();
    expect(calls("host_term_detach")).toEqual([["host_term_detach", { host: H, terminal: "u2" }]]);
  });

  it("StrictMode double mount results in exactly one attach", async () => {
    const rt = new RemoteTerminals();
    const attach = deferred<{ exitCode: null }>();
    invoke.mockImplementation((cmd: string) => (cmd === "host_term_attach" ? attach.p : Promise.resolve()));
    const first = sinks(), second = sinks();
    // mount → start in flight → simulated unmount → remount → start again
    const g1 = rt.mount(H, "u3", first.s);
    const ac1 = new AbortController();
    const s1 = rt.start(H, "u3", g1, opts, ac1.signal);
    ac1.abort();
    rt.unmount("u3", g1);
    const g2 = rt.mount(H, "u3", second.s);
    const s2 = rt.start(H, "u3", g2, opts);
    attach.resolve({ exitCode: null });
    expect(await s1).toBeNull();
    expect(await s2).toMatchObject({ kind: "attached" });
    await flush();
    expect(calls("host_term_attach")).toHaveLength(1);
    expect(calls("host_term_detach")).toEqual([]);
    // output reaches the live mount only
    ch<ArrayBuffer>(0).emit(new Uint8Array([1, 2]).buffer);
    expect(first.data).toEqual([]);
    expect(second.data).toHaveLength(1);
  });

  it("start aborted before the call never attaches", async () => {
    const rt = new RemoteTerminals();
    const g = rt.mount(H, "u4", sinks().s);
    const ac = new AbortController();
    ac.abort();
    expect(await rt.start(H, "u4", g, opts, ac.signal)).toBeNull();
    expect(invoke).not.toHaveBeenCalled();
  });

  it("delayed open + quick close → host_term_close once", async () => {
    const rt = new RemoteTerminals();
    const open = deferred<{ pid: number }>();
    invoke.mockImplementation((cmd: string) => (cmd === "host_term_open" ? open.p : Promise.resolve()));
    pendingOpens.set("u5", { host: H, spec: { cwd: "/p", agent: "claude" }, meta: { title: "New Chat" }, state: "opening" });
    const g = rt.mount(H, "u5", sinks().s);
    const started = rt.start(H, "u5", g, opts);
    const c1 = rt.close(H, "u5");
    const c2 = rt.close(H, "u5");
    rt.unmount("u5", g);
    expect(calls("host_term_close")).toEqual([]);
    open.resolve({ pid: 42 });
    await Promise.all([c1, c2, started]);
    expect(calls("host_term_open")).toHaveLength(1);
    expect(calls("host_term_open")[0][1]).toMatchObject({ host: H, terminal: "u5", spec: { cwd: "/p", agent: "claude" }, meta: { title: "New Chat" }, cols: 80, rows: 24 });
    expect(calls("host_term_close")).toEqual([["host_term_close", { host: H, terminal: "u5" }]]);
    expect(calls("host_term_detach")).toEqual([]);
    await rt.close(H, "u5");
    expect(calls("host_term_close")).toHaveLength(1);
  });

  it("a failed open is never closed on the Host", async () => {
    const rt = new RemoteTerminals();
    invoke.mockImplementation((cmd: string) => (cmd === "host_term_open" ? Promise.reject({ code: "remote", message: "no such dir" }) : Promise.resolve()));
    pendingOpens.set("u6", { host: H, spec: { cwd: "/nope" }, meta: {}, state: "opening" });
    const g = rt.mount(H, "u6", sinks().s);
    await expect(rt.start(H, "u6", g, opts)).rejects.toMatchObject({ message: "no such dir" });
    expect(pendingOpens.get("u6")).toMatchObject({ state: "failed", error: "no such dir" });
    await rt.close(H, "u6");
    expect(calls("host_term_close")).toEqual([]);
  });

  it("re-attach after a config replacement is single-flight (amendment 20)", async () => {
    const rt = new RemoteTerminals();
    invoke.mockResolvedValue({ exitCode: null });
    const s = sinks();
    const g = rt.mount(H, "u7", s.s);
    await rt.start(H, "u7", g, opts);
    const r1 = rt.reattach(H, "u7", g, opts);
    const r2 = rt.reattach(H, "u7", g, opts);
    await Promise.all([r1, r2]);
    expect(calls("host_term_attach")).toHaveLength(2);
    // output on the replaced (old) channel is ignored
    ch<ArrayBuffer>(0).emit(new Uint8Array([9]).buffer);
    ch<ArrayBuffer>(2).emit(new Uint8Array([7]).buffer);
    expect(s.data.map(b => [...b])).toEqual([[7]]);
  });
});

describe("exit ordered after output (amendment 8)", () => {
  it("exit arriving before the last data chunk is applied only after the chunk", async () => {
    const rt = new RemoteTerminals();
    invoke.mockResolvedValue({ exitCode: null });
    const s = sinks();
    const g = rt.mount(H, "x1", s.s);
    await rt.start(H, "x1", g, opts);
    const data = ch<ArrayBuffer>(0), exit = ch<unknown>(1);
    data.emit(new Uint8Array([1, 2, 3]).buffer);
    exit.emit({ code: 0, bytes: 5 });
    expect(s.exits).toEqual([]);
    data.emit(new Uint8Array([4, 5]).buffer);
    expect(s.exits).toEqual([0]);
    expect(s.data.map(b => b.length)).toEqual([3, 2]);
  });

  it("exit with everything already received fires at once", async () => {
    const rt = new RemoteTerminals();
    invoke.mockResolvedValue({ exitCode: null });
    const s = sinks();
    const g = rt.mount(H, "x2", s.s);
    await rt.start(H, "x2", g, opts);
    const data = ch<ArrayBuffer>(0), exit = ch<unknown>(1);
    data.emit(new Uint8Array([1]).buffer);
    exit.emit({ code: 3, bytes: 1 });
    expect(s.exits).toEqual([3]);
  });
});

describe("local parity: terminal commands", () => {
  const tab: Tab = { id: "terminal-s1-abc", type: "terminal", title: "T", sessionId: "s1", agent: "claude", projectPath: "/home/u/p", projectName: "p" };

  it("spawn_terminal gets today's exact arguments", async () => {
    invoke.mockResolvedValue(undefined);
    const t = mountTerminal(tab, sinks().s);
    const shell = localShellOptions(tab, "bash");
    expect(shell).toEqual({ shellMode: "claude", shellId: "bash", shellCommand: "bash" });
    await t.start({ cols: 120, rows: 40, ...shell, fullscreenRendering: true, forceSyncOutput: false });
    expect(invoke).toHaveBeenCalledTimes(1);
    const [cmd, args] = invoke.mock.calls[0];
    expect(cmd).toBe("spawn_terminal");
    expect(Object.keys(args)).toEqual(["id", "sessionId", "cwd", "cols", "rows", "shellMode", "shellCommand", "shellId", "agent", "fullscreenRendering", "forceSyncOutput", "onData", "onExit"]);
    expect({ ...args, onData: undefined, onExit: undefined }).toEqual({ id: "terminal-s1-abc", sessionId: "s1", cwd: "/home/u/p", cols: 120, rows: 40, shellMode: "claude", shellCommand: "bash", shellId: "bash", agent: "claude", fullscreenRendering: true, forceSyncOutput: false, onData: undefined, onExit: undefined });
    t.end();
    expect(invoke.mock.calls[1]).toEqual(["close_terminal", { id: "terminal-s1-abc" }]);
  });

  it("raw shell and new-chat defaults: cwd '.', null session, explicit shell", async () => {
    invoke.mockResolvedValue(undefined);
    const raw: Tab = { id: "terminal-shell-1", type: "terminal", title: "PowerShell", projectPath: "", projectName: "~", shellMode: "raw", shellId: "powershell" };
    const t = mountTerminal(raw, sinks().s);
    await t.start({ cols: 80, rows: 24, ...localShellOptions(raw, "bash"), fullscreenRendering: true, forceSyncOutput: true });
    expect(invoke.mock.calls[0][1]).toMatchObject({ id: "terminal-shell-1", sessionId: null, cwd: ".", shellMode: "raw", shellId: "powershell", shellCommand: "powershell.exe", agent: null });
    const codex: Tab = { id: "terminal-new-1", type: "terminal", title: "New Chat", projectPath: "/p", shellMode: "claude", agent: "codex" };
    expect(localShellOptions(codex, "zsh")).toEqual({ shellMode: "claude", shellId: "zsh", shellCommand: "zsh" });
  });

  it("write/resize use write_terminal/resize_terminal; local close never touches host_* commands", async () => {
    invoke.mockResolvedValue(undefined);
    writeTerminal(tab, "ls\r");
    resizeTerminal(tab, 100, 30);
    markClosing([tab]);
    const t = mountTerminal(tab, sinks().s);
    t.end(); // never started → no close_terminal (as before)
    expect(invoke.mock.calls).toEqual([["write_terminal", { id: tab.id, data: "ls\r" }], ["resize_terminal", { id: tab.id, cols: 100, rows: 30 }]]);
  });

  it("an unmount before the start never spawns", async () => {
    const t = mountTerminal(tab, sinks().s);
    const ac = new AbortController();
    ac.abort();
    await t.start(opts, ac.signal);
    t.end();
    expect(invoke).not.toHaveBeenCalled();
  });

  it("remote write/resize use host_term_input/host_term_resize", () => {
    invoke.mockResolvedValue(undefined);
    const r: Tab = { id: "remote-u", type: "terminal", title: "R", host: H, terminal: "u" };
    writeTerminal(r, "x");
    resizeTerminal(r, 10, 5);
    expect(invoke.mock.calls).toEqual([["host_term_input", { host: H, terminal: "u", data: "x" }], ["host_term_resize", { host: H, terminal: "u", cols: 10, rows: 5 }]]);
  });
});

describe("close intent through the TerminalTab-facing API", () => {
  it("markClosing closes once; the later unmount neither detaches nor closes again, even after the list dropped it", async () => {
    invoke.mockResolvedValue({ exitCode: null });
    const tab: Tab = { id: "remote-c1", type: "terminal", title: "R", host: H, terminal: "c1" };
    const t = mountTerminal(tab, sinks().s);
    await t.start(opts);
    markClosing([tab]);
    await flush();
    t.end();
    await flush();
    expect(calls("host_term_close")).toEqual([["host_term_close", { host: H, terminal: "c1" }]]);
    expect(calls("host_term_detach")).toEqual([]);
  });

  it("an unmount without close intent detaches", async () => {
    invoke.mockResolvedValue({ exitCode: null });
    const tab: Tab = { id: "remote-c2", type: "terminal", title: "R", host: H, terminal: "c2" };
    const t = mountTerminal(tab, sinks().s);
    await t.start(opts);
    t.end();
    expect(calls("host_term_detach")).toEqual([["host_term_detach", { host: H, terminal: "c2" }]]);
    expect(calls("host_term_close")).toEqual([]);
  });
});

describe("connection failures during start", () => {
  it("an open that never reached the Daemon is retried, not failed", async () => {
    const rt = new RemoteTerminals();
    invoke.mockRejectedValueOnce({ code: "offline", message: "not connected" }).mockResolvedValueOnce({ pid: 7 });
    pendingOpens.set("o1", { host: H, spec: { cwd: "/p" }, meta: {}, state: "opening" });
    const g = rt.mount(H, "o1", sinks().s);
    await expect(rt.start(H, "o1", g, opts)).rejects.toBeInstanceOf(RetryableStartError);
    expect(pendingOpens.get("o1")?.state).toBe("opening");
    await expect(rt.start(H, "o1", g, opts)).resolves.toMatchObject({ kind: "opened", pid: 7 });
    expect(calls("host_term_open")).toHaveLength(2);
    expect(calls("host_term_attach")).toEqual([]);
  });

  it("an attach that fails offline is retryable", async () => {
    const rt = new RemoteTerminals();
    invoke.mockRejectedValueOnce({ code: "offline", message: "down" }).mockResolvedValueOnce({ exitCode: 3 });
    const g = rt.mount(H, "a1", sinks().s);
    await expect(rt.start(H, "a1", g, opts)).rejects.toBeInstanceOf(RetryableStartError);
    await expect(rt.start(H, "a1", g, opts)).resolves.toMatchObject({ kind: "attached", exitCode: 3 });
  });
});

// Sol finding 2: a failed close keeps its intent, is retried, and never resurrects the Tab.
import { reconcileHosts, tabFromTerminal } from "./reconcile";
describe("failed closes", () => {
  const listed = (uuid: string) => [{ terminal: uuid, spec: { cwd: "/p", agent: "claude" }, meta: {}, createdAtMs: 1, pid: 1, exitCode: null }];

  it("close rejected offline → reconnect → close retried, tab not resurrected", async () => {
    let usable = false;
    let becomeUsable!: () => void;
    const waiting = new Promise<void>(r => { becomeUsable = r; });
    const rt = new RemoteTerminals({ isUsable: () => usable, waitUsable: () => waiting });
    invoke.mockImplementation((cmd: string) => {
      if (cmd === "host_term_attach") return Promise.resolve({ exitCode: null });
      if (cmd === "host_term_close") return calls("host_term_close").length === 1 ? Promise.reject({ code: "offline", message: "down" }) : Promise.resolve();
      return Promise.resolve();
    });
    const g = rt.mount(H, "f1", sinks().s);
    await rt.start(H, "f1", g, opts);
    const closing = rt.close(H, "f1");
    rt.unmount("f1", g);
    await flush();
    expect(calls("host_term_close")).toHaveLength(1);
    expect(rt.isClosing("f1")).toBe(true);
    // The Host reconnects and lists the still-running Terminal before the retry lands:
    // reconcile must not bring its Tab back.
    const r = reconcileHosts([], [[H, listed("f1")]], { pending: new Set(), isClosing: u => rt.isClosing(u) });
    expect(r.deltas).toEqual([]);
    usable = true;
    becomeUsable();
    await closing;
    expect(calls("host_term_close")).toHaveLength(2);
    expect(calls("host_term_detach")).toEqual([]);
    expect(rt.isClosing("f1")).toBe(true); // retired only once the list drops it
    rt.forgetUnlisted(H, []);
    expect(rt.isClosing("f1")).toBe(false);
  });

  it("repeated close sends again after a permanent failure, and the tab is restored", async () => {
    const rt = new RemoteTerminals({ isUsable: () => true, waitUsable: () => Promise.resolve() });
    invoke.mockImplementation((cmd: string) => {
      if (cmd === "host_term_attach") return Promise.resolve({ exitCode: null });
      if (cmd === "host_term_close") return calls("host_term_close").length === 1 ? Promise.reject({ code: "remote", message: "permission denied" }) : Promise.resolve();
      return Promise.resolve();
    });
    const failures: string[] = [];
    rt.onCloseFailed(f => failures.push(`${f.uuid}: ${f.error}`));
    const g = rt.mount(H, "f2", sinks().s);
    await rt.start(H, "f2", g, opts);
    await rt.close(H, "f2");
    expect(failures).toEqual(["f2: permission denied"]);
    expect(rt.isClosing("f2")).toBe(false);
    // Still listed → its Tab comes back and can attach again.
    const r = reconcileHosts([], [[H, listed("f2")]], { pending: new Set(), isClosing: u => rt.isClosing(u) });
    expect(r.deltas[0].add.map(t => t.id)).toEqual([tabFromTerminal(H, listed("f2")[0]).id]);
    const g2 = rt.mount(H, "f2", sinks().s);
    await expect(rt.start(H, "f2", g2, opts)).resolves.toMatchObject({ kind: "attached" });
    await rt.close(H, "f2");
    expect(calls("host_term_close")).toHaveLength(2);
  });

  it("a busy Host gets the close again after a backoff", async () => {
    const rt = new RemoteTerminals({ isUsable: () => true, waitUsable: () => Promise.resolve() });
    invoke.mockImplementation((cmd: string) => cmd === "host_term_close" && calls("host_term_close").length === 1 ? Promise.reject({ code: "busy", message: "queue full" }) : Promise.resolve({ exitCode: null }));
    const g = rt.mount(H, "f3", sinks().s);
    await rt.start(H, "f3", g, opts);
    const t0 = Date.now();
    await rt.close(H, "f3");
    expect(calls("host_term_close")).toHaveLength(2);
    expect(Date.now() - t0).toBeGreaterThanOrEqual(200);
  });

  it("the list dropping the Terminal stops a close waiting for the Host", async () => {
    const rt = new RemoteTerminals({ isUsable: () => false, waitUsable: (_h, signal) => new Promise((_r, rej) => signal?.addEventListener("abort", () => rej(new Error("aborted")))) });
    invoke.mockImplementation((cmd: string) => cmd === "host_term_close" ? Promise.reject({ code: "offline", message: "down" }) : Promise.resolve({ exitCode: null }));
    const g = rt.mount(H, "f4", sinks().s);
    await rt.start(H, "f4", g, opts);
    const closing = rt.close(H, "f4");
    await flush();
    rt.forgetUnlisted(H, []);
    await closing;
    expect(calls("host_term_close")).toHaveLength(1);
  });
});
