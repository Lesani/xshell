import { describe, expect, it } from "vitest";
import type { AgentId } from "../agents";
import { resolveNewChatChoice } from "./newChatChoice";

describe("new Chat choice", () => {
  for (const [label, installed] of [
    ["zero", []], ["one", ["claude"]], ["multiple", ["claude", "codex"]],
  ] as [string, AgentId[]][]) {
    it(`Ask offers Shell with ${label} installed harnesses`, () => {
      expect(resolveNewChatChoice("ask", installed)).toEqual({ kind: "picker" });
    });
  }
  it("installed explicit default starts immediately", () => {
    expect(resolveNewChatChoice("codex", ["claude", "codex"])).toEqual({ kind: "agent", agent: "codex" });
    expect(resolveNewChatChoice("codex", ["codex"])).toEqual({ kind: "agent", agent: "codex" });
  });
  it("missing explicit default opens choice", () => {
    expect(resolveNewChatChoice("codex", ["claude"])).toEqual({ kind: "picker" });
    expect(resolveNewChatChoice("codex", [])).toEqual({ kind: "picker" });
  });
  it("named harness actions retain explicit selection", () => {
    expect(resolveNewChatChoice("codex", ["claude", "codex"], "claude")).toEqual({ kind: "agent", agent: "claude" });
  });
});
