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

function assertWorkingRingCss(css: string) {
  const workingRules = [...css.matchAll(/([^{}]+)\{([^{}]*)\}/g)]
    .filter(([, selector]) => selector.includes(".tab-attention-working::after"))
    .map(([, selector, declarations]) => [selector.trim(), declarations.trim()]);
  expect(workingRules).toEqual([
    [".tab-attention-working::after", "border: 2px solid #ef921c; border-top-color: #ffe073; border-right-color: #ffba38; animation: none; transition: none; transform: rotate(var(--tab-attention-angle, 0deg));"],
    [".tab-attention-working::after", "transform: none;"],
  ]);
  expect(css).toMatch(/\.tab-attention-unread::after\s*\{ border: 2px solid var\(--success\); \}/);
  expect(css).toMatch(/\.tab-attention-needs-you::after\s*\{ border: 2px solid #e5a50a; \}/);
  expect(css).not.toMatch(/tab-attention-rotate|animation-play-state/);
  expect(css).toMatch(/@media \(prefers-reduced-motion: reduce\)\s*\{\s*\.tab-attention-working::after\s*\{ transform: none;/);
  expect(css).not.toMatch(/tab-pi-working|tab-project-icon-linked|tab-dot-active|tab-agent-status/);
  for (const selector of ["tab-attention-icon", "tab-project-icon", "tab-project-img", "tab-dot", "tab-bar"]) {
    const rules = [...css.matchAll(/([^{}]+)\{([^{}]*)\}/g)]
      .filter(([, s]) => new RegExp(`\\.${selector}(?:\\s|,|$)`).test(s.trim()))
      .map(([, , declarations]) => declarations).join("");
    expect(rules, selector).not.toMatch(/(?:^|[;\s])(?:transform|animation(?:-[\w-]+)?|opacity)\s*:/);
  }
}

it("attention rotates only its outer ring and keeps reduced-motion static", async () => {
  const { readFile } = await import("node:fs/promises");
  assertWorkingRingCss(await readFile(new URL("../App.css", import.meta.url), "utf8"));
});

it.each([
  ["infinite animation", "animation: none; transition: none;", "animation: spin 1.2s infinite; transition: none;"],
  ["transform transition", "transition: none; transform: rotate(var(--tab-attention-angle, 0deg));", "transition: transform 100ms linear; transform: rotate(var(--tab-attention-angle, 0deg));"],
  ["moving glyph", ".tab-project-img {", ".tab-project-img { transform: rotate(30deg);"],
  ["missing static reduced-motion", ".tab-attention-working::after { transform: none; }", ".tab-attention-working::after { transform: rotate(var(--tab-attention-angle)); }"],
])("Working CSS contract rejects %s mutation", async (_name, original, mutation) => {
  const { readFile } = await import("node:fs/promises");
  const css = await readFile(new URL("../App.css", import.meta.url), "utf8");
  expect(css).toContain(original);
  expect(() => assertWorkingRingCss(css.replace(original, mutation))).toThrow();
});
