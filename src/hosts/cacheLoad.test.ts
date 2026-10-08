import { describe, expect, it, vi } from "vitest";

const disk = vi.hoisted(() => ({ value: undefined as unknown, gate: null as null | Promise<void> }));
vi.mock("@tauri-apps/plugin-store", () => ({
  load: vi.fn(async () => ({
    get: async () => { if (disk.gate) await disk.gate; return disk.value; },
    set: async (_k: string, v: unknown) => { disk.value = v; },
    save: async () => {},
  })),
}));

import { cache, putTerminals, emptyCache } from "./cache";
import type { TerminalInfo } from "./types";

const H = "h_ab12cd34";
const term = (id: string): TerminalInfo => ({ terminal: id, spec: { cwd: "/p" }, meta: {}, createdAtMs: 1, pid: 1, exitCode: null });

describe("delayed cache load (amendment 21)", () => {
  it("a list that arrives before the cache file loads wins over the disk copy", async () => {
    cache._reset();
    disk.value = putTerminals(emptyCache(), H, [term("old")], 1);
    let release!: () => void;
    disk.gate = new Promise<void>(r => { release = r; });
    const loading = cache.load();
    const loading2 = cache.load();
    expect(loading2).toBe(loading);            // single load
    cache.putTerminals(H, [term("fresh")]);    // live list lands first
    release();
    await loading;
    expect(cache.terminals(H)?.map(t => t.terminal)).toEqual(["fresh"]);
    disk.gate = null;
  });

  it("disk entries for other hosts are kept", async () => {
    cache._reset();
    disk.value = putTerminals(emptyCache(), "h_zz12cd34", [term("z")], 1);
    await cache.load();
    expect(cache.terminals("h_zz12cd34")?.map(t => t.terminal)).toEqual(["z"]);
  });
});
