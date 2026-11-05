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
  const workingRules = [...css.matchAll(/([^{}]+)\{([^{}]*)\}/g)]
    .filter(([, selector]) => selector.includes(".tab-attention-working::after"))
    .map(([, selector, declarations]) => [selector.trim(), declarations.trim()]);
  expect(workingRules).toEqual([
    [".tab-attention-working::after", "border: 2px solid #ef921c; border-top-color: #ffe073; border-right-color: #ffba38; animation: tab-attention-rotate 1.2s steps(12, end) infinite;"],
    ['.tab-bar[data-attention-motion-paused="true"] .tab-attention-working::after', "animation-play-state: paused;"],
    [".tab-attention-working::after", "animation: none;"],
  ]);
  expect(css).toMatch(/\.tab-attention-unread::after\s*\{ border: 2px solid var\(--success\); \}/);
  expect(css).toMatch(/\.tab-attention-needs-you::after\s*\{ border: 2px solid #e5a50a; \}/);
  expect(css).toMatch(/@keyframes tab-attention-rotate\s*\{ to \{ transform: rotate\(360deg\);/);
  expect(css).toMatch(/@media \(prefers-reduced-motion: reduce\)\s*\{\s*\.tab-attention-working::after\s*\{ animation: none;/);
  expect(css).not.toMatch(/tab-pi-working|tab-project-icon-linked|tab-dot-active|tab-agent-status/);
  for (const selector of ["tab-attention-icon", "tab-project-icon", "tab-project-img", "tab-dot", "tab-bar"]) {
    const rules = [...css.matchAll(/([^{}]+)\{([^{}]*)\}/g)]
      .filter(([, s]) => new RegExp(`\\.${selector}(?:\\s|,|$)`).test(s.trim()))
      .map(([, , declarations]) => declarations).join("");
    expect(rules, selector).not.toMatch(/(?:^|[;\s])(?:transform|animation(?:-[\w-]+)?|opacity)\s*:/);
  }
});
