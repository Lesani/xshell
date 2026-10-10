import { describe, expect, it } from "vitest";
import dispatchSrc from "../../src-tauri/crates/core/src/dispatch.rs?raw";
import { HOST_METHODS, CACHEABLE_METHODS } from "./methods";
import { HOST_ID_PATTERN } from "./projectKey";

// Rust sources that could define the Host id pattern (hostlink config, desktop glue).
const rustSources = import.meta.glob(["/src-tauri/crates/*/src/**/*.rs", "/src-tauri/src/**/*.rs"], { query: "?raw", import: "default", eager: true }) as Record<string, string>;

function coreMethods(src: string): string[] {
  const m = /pub const METHODS:\s*&\[&str\]\s*=\s*&\[([\s\S]*?)\];/.exec(src);
  if (!m) throw new Error("METHODS not found in dispatch.rs");
  return [...m[1].matchAll(/"([a-z_]+)"/g)].map(x => x[1]);
}

describe("hostMethods", () => {
  it("TS list equals core METHODS", () => {
    const rust = coreMethods(dispatchSrc);
    expect(rust.length).toBe(36);
    expect(new Set(HOST_METHODS)).toEqual(new Set(rust));
    expect(HOST_METHODS.length).toBe(rust.length);
  });

  it("cacheable methods are host methods", () => {
    for (const m of CACHEABLE_METHODS) expect(HOST_METHODS).toContain(m);
  });

  // Amendment 3: the Host id pattern is single-sourced — one TS constant, one Rust constant,
  // and the two strings are equal. Rust patterns are found as string literals starting `^h_`.
  const rustPatterns = Object.entries(rustSources).flatMap(([file, src]) =>
    [...src.matchAll(/"(\^h_[^"]*)"/g)].map(m => ({ file, pattern: m[1] })));
  it.skipIf(rustPatterns.length === 0)("Rust host id pattern equals the TS pattern", () => {
    for (const { pattern } of rustPatterns) expect(pattern).toBe(HOST_ID_PATTERN);
  });
});
