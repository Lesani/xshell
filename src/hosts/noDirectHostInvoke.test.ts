import { describe, expect, it } from "vitest";
import { HOST_METHODS } from "./methods";

// Every Host-side command must go through hostInvoke so a Remote Host is never bypassed.
const sources = import.meta.glob(["/src/**/*.ts", "/src/**/*.tsx", "!/src/hosts/**", "!/src/**/*.test.ts"], { query: "?raw", import: "default", eager: true }) as Record<string, string>;

describe("noDirectHostInvoke", () => {
  it("scans the frontend sources", () => {
    expect(Object.keys(sources).some(f => f.endsWith("/App.tsx"))).toBe(true);
    expect(Object.keys(sources).some(f => f.endsWith("/TerminalTab.tsx"))).toBe(true);
  });

  const re = /\binvoke\s*(?:<[^>()]*(?:<[^>]*>[^>()]*)*>)?\s*\(\s*["'`]([a-z_]+)["'`]/g;
  const hostCalls = (src: string) => [...src.matchAll(re)].map(m => m[1]).filter(c => (HOST_METHODS as readonly string[]).includes(c));

  it("the scanner detects direct calls", () => {
    expect(hostCalls(`invoke("get_sessions", {})`)).toEqual(["get_sessions"]);
    expect(hostCalls(`invoke<{ installed: boolean }>("detect_agent_binary", {})`)).toEqual(["detect_agent_binary"]);
    expect(hostCalls(`invoke<Record<string, X>>( 'list_dir')`)).toEqual(["list_dir"]);
    expect(hostCalls(`hostInvoke(undefined, "get_sessions")`)).toEqual([]);
    expect(hostCalls(`invoke("open_url", {})`)).toEqual([]);
  });

  it("no invoke( or invoke<…>( outside src/hosts/ names a Host method", () => {
    const offenders: string[] = [];
    for (const [file, src] of Object.entries(sources)) for (const c of hostCalls(src)) offenders.push(`${file}: ${c}`);
    expect(offenders).toEqual([]);
  });
});
