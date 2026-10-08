import { describe, expect, it } from "vitest";
import { callKey, dropHost, emptyCache, getCall, MAX_CALLS_PER_HOST, parseCache, putCall, putTerminals } from "./cache";
import type { TerminalInfo } from "./types";

const H = "h_ab12cd34";
const term = (id: string): TerminalInfo => ({ terminal: id, spec: { cwd: "/p" }, meta: {}, createdAtMs: 1, pid: 1, exitCode: null });

describe("cache", () => {
  it("putTerminals/getCall round trip", () => {
    let s = putTerminals(emptyCache(), H, [term("a")], 10);
    s = putCall(s, H, callKey("get_sessions", { encodedName: "x" }), [1, 2], 11);
    expect(s.hosts[H].terminals.map(t => t.terminal)).toEqual(["a"]);
    expect(s.hosts[H].terminalsAt).toBe(10);
    expect(getCall(s, H, callKey("get_sessions", { encodedName: "x" }))).toEqual({ v: [1, 2], at: 11 });
    expect(getCall(s, H, callKey("get_sessions", { encodedName: "y" }))).toBeUndefined();
  });

  it("callKey is stable across key order", () => {
    expect(callKey("m", { a: 1, b: { d: 2, c: 3 } })).toBe(callKey("m", { b: { c: 3, d: 2 }, a: 1 }));
    expect(callKey("m")).toBe(callKey("m", {}));
  });

  it("LRU caps at 200", () => {
    let s = emptyCache();
    for (let i = 0; i < MAX_CALLS_PER_HOST + 5; i++) s = putCall(s, H, `k${i}`, i, i);
    // refresh k10 so it becomes most recent and survives the next eviction
    s = putCall(s, H, "k10", "fresh", 999);
    s = putCall(s, H, "extra", 0, 1000);
    const keys = Object.keys(s.hosts[H].calls);
    expect(keys.length).toBe(MAX_CALLS_PER_HOST);
    expect(keys).not.toContain("k0");
    expect(keys).not.toContain("k5");
    expect(getCall(s, H, "k10")?.v).toBe("fresh");
    expect(keys[keys.length - 1]).toBe("extra");
  });

  it("dropHost removes everything for the host", () => {
    let s = putTerminals(emptyCache(), H, [term("a")], 1);
    s = putCall(s, H, "k", 1, 1);
    s = putCall(s, "h_zz12cd34", "k", 2, 1);
    s = dropHost(s, H);
    expect(s.hosts[H]).toBeUndefined();
    expect(getCall(s, "h_zz12cd34", "k")?.v).toBe(2);
  });

  it("version mismatch ignored", () => {
    expect(parseCache({ version: 2, hosts: { [H]: {} } })).toEqual(emptyCache());
    expect(parseCache(null)).toEqual(emptyCache());
    expect(parseCache("x")).toEqual(emptyCache());
    const ok = putTerminals(emptyCache(), H, [term("a")], 1);
    expect(parseCache(JSON.parse(JSON.stringify(ok)))).toEqual(ok);
  });
});

import { tabFromTerminal } from "./reconcile";
describe("cache at startup", () => {
  it("cached terminals produce stale tabs at startup", () => {
    const s = putTerminals(emptyCache(), H, [term("b"), { ...term("a"), meta: { title: "Agent", projectName: "p" } }], 5);
    const tabs = s.hosts[H].terminals.map(i => tabFromTerminal(H, i));
    expect(tabs.map(t => t.id)).toEqual(["remote-b", "remote-a"]);
    expect(tabs[1]).toMatchObject({ host: H, terminal: "a", title: "Agent", projectName: "p", projectPath: "/p" });
  });
});
