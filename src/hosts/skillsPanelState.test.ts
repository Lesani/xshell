import { describe, expect, it } from "vitest";
import { initialSkillsPanelState, skillsPanelReducer, shouldShowClaudeContext } from "./skillsPanelState";
import { toProjectKey } from "./projectKey";
import type { ProjectSkills } from "../types";

const skills = (name: string) => ({ personal_skills: [{ name, scope: "personal", description: null, path: `/x/${name}` }], project_skills: [], plugins: [], user_mcps: [], project_mcps: [], subagents: [], slash_commands: [], hooks: [], claude_md_files: [], settings_sources: [] } as ProjectSkills);

// Sol finding 4: switching between two Hosts with the same path clears everything shown
// for the previous Host and ignores its late results.
describe("SkillsPanel project-scoped state", () => {
  const A = toProjectKey("h_aaaaaaaa", "/home/u/app");
  const B = toProjectKey("h_bbbbbbbb", "/home/u/app");

  it("a ProjectKey change resets skills, memories, contexts and the open document", () => {
    let s = initialSkillsPanelState(A);
    s = skillsPanelReducer(s, { type: "loaded", key: A, patch: { data: skills("a"), memories: { dir: "/m", items: [{ name: "n", description: "", type: "t", path: "/m/n.md" }] }, codexCtx: { present: true, trust_level: null, sections: [] }, loading: false } });
    s = skillsPanelReducer(s, { type: "open", patch: { openDoc: { path: "/home/u/app/CLAUDE.md", title: "CLAUDE.md" }, openMemory: s.memories.items[0] } });
    s = skillsPanelReducer(s, { type: "switch", key: B });
    expect(s).toEqual(initialSkillsPanelState(B));
    expect(s.openDoc).toBeNull();
    expect(s.data).toBeNull();
  });

  it("results for the previous key are ignored after the switch", () => {
    let s = skillsPanelReducer(initialSkillsPanelState(A), { type: "switch", key: B });
    s = skillsPanelReducer(s, { type: "loaded", key: A, patch: { data: skills("late-a") } });
    expect(s.data).toBeNull();
    s = skillsPanelReducer(s, { type: "loaded", key: B, patch: { data: skills("b") } });
    expect(s.data?.personal_skills[0].name).toBe("b");
  });

  it("switching to the same key keeps state", () => {
    const s = skillsPanelReducer(initialSkillsPanelState(A), { type: "loaded", key: A, patch: { data: skills("a") } });
    expect(skillsPanelReducer(s, { type: "switch", key: A })).toBe(s);
  });
});


it("Pi context resets on Host project change", () => {
  const A = toProjectKey("h_aaaaaaaa", "/project");
  const s = skillsPanelReducer(initialSkillsPanelState(A), { type: "loaded", key: A, patch: { piCtx: { present: true, sections: [{ title: "Instructions", items: [{ name: "AGENTS.md", path: "/project/AGENTS.md", detail: "project" }] }] } } });
  for (const B of [toProjectKey("h_bbbbbbbb", "/project"), toProjectKey("h_aaaaaaaa", "/other")]) {
    const next = skillsPanelReducer(s, { type: "switch", key: B });
    expect(next.piCtx).toBeNull();
    expect(skillsPanelReducer(next, { type: "loaded", key: A, patch: { piCtx: s.piCtx } })).toBe(next);
  }
});

it("Pi presence never falls back to Claude context", () => {
  expect(shouldShowClaudeContext(false, false, { pi: true })).toBe(false);
  expect(shouldShowClaudeContext(false, false, undefined, true)).toBe(false);
  expect(shouldShowClaudeContext(false, false, { pi: true, claude: true })).toBe(true);
  expect(shouldShowClaudeContext(true, false, { pi: true })).toBe(true);
  expect(shouldShowClaudeContext(false, false)).toBe(true);
});
