import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

// Execute the exact Rust-embedded module, linked only to Pi's public API and child transport.
// A separate Node VM avoids adding Pi as a frontend dependency or rewriting its algorithm.
const run = promisify(execFile);
describe("pi_status.js embedded extension", () => {
  it.each(["compatibility", "lifecycle", "idle-recovery", "retry", "latest-and-uncertain-ack", "generation-cleanup", "real-timeout"])("%s", async scenario => {
    const { stdout } = await run(process.execPath, ["--experimental-vm-modules", fileURLToPath(new URL("./piStatusExtensionHarness.mjs", import.meta.url)), scenario], { timeout: 12000 });
    expect(stdout).toContain(`PASS ${scenario}`);
  }, 15000);
});

it("Pi icon CSS uses compositor properties and disables animation for reduced motion", async () => {
  const { readFile } = await import("node:fs/promises");
  const css = await readFile(new URL("../App.css", import.meta.url), "utf8");
  expect(css).toMatch(/@keyframes tab-pi-working\s*\{[\s\S]*?transform: scale\(0\.85\); opacity: 0\.55/);
  expect(css).toMatch(/@media \(prefers-reduced-motion: reduce\)\s*\{\s*\.tab-pi-working\s*\{ animation: none;/);
});
