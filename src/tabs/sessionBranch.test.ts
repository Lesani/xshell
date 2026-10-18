import { expect, it } from "vitest";
import { usesClaudeBranchDetection } from "./sessionBranch";

it("Pi never runs Claude-only branch detection", () => {
  expect(usesClaudeBranchDetection({ agent: "pi", shellMode: "claude" })).toBe(false);
  expect(usesClaudeBranchDetection({})).toBe(true);
  expect(usesClaudeBranchDetection({ agent: "claude", shellMode: "raw" })).toBe(false);
  expect(usesClaudeBranchDetection({ agent: "codex" })).toBe(false);
});
