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

it("attention rotates only its outer ring and keeps reduced-motion static", async () => {
  const { readFile } = await import("node:fs/promises");
  const css = await readFile(new URL("../App.css", import.meta.url), "utf8");
  expect(css).toMatch(/\.tab-attention-working::after\s*\{[^}]*animation: tab-attention-rotate/);
  expect(css).toMatch(/@keyframes tab-attention-rotate\s*\{ to \{ transform: rotate\(360deg\);/);
  expect(css).toMatch(/@media \(prefers-reduced-motion: reduce\)\s*\{\s*\.tab-attention-working::after\s*\{ animation: none;/);
  expect(css).not.toMatch(/tab-pi-working|tab-project-icon-linked|tab-dot-active|tab-agent-status/);
  expect(css.match(/\.tab-attention-icon \{[^}]+/g)?.join("")).not.toMatch(/transform|animation|opacity/);
});
