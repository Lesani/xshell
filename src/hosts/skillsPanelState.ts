import type { AgentId } from "../agents";
import type { PiContext, AntigravityContext, CodexContext, CursorContext, Memory, OpencodeContext, ProjectMemories, ProjectSkills } from "../types";

// Everything SkillsPanel shows is scoped to one ProjectKey (Host + path) — including the open
// document, which is read with the panel's Host. A key change resets all of it at once, and a
// result for any other key is ignored (amendment 23 / Sol finding 4).

export interface SkillsPanelState {
  key: string;
  data: ProjectSkills | null;
  memories: ProjectMemories;
  codexCtx: CodexContext | null;
  cursorCtx: CursorContext | null;
  opencodeCtx: OpencodeContext | null;
  antigravityCtx: AntigravityContext | null;
  piCtx: PiContext | null;
  loading: boolean;
  openMemory: Memory | null;
  openDoc: { path: string; title: string } | null;
}

export type SkillsPanelLoaded = Pick<SkillsPanelState, "data" | "memories" | "codexCtx" | "cursorCtx" | "opencodeCtx" | "antigravityCtx" | "piCtx" | "loading">;

export type SkillsPanelAction =
  | { type: "switch"; key: string }
  | { type: "loaded"; key: string; patch: Partial<SkillsPanelLoaded> }
  | { type: "open"; patch: Partial<Pick<SkillsPanelState, "openMemory" | "openDoc">> };

export function initialSkillsPanelState(key: string): SkillsPanelState {
  return { key, data: null, memories: { dir: "", items: [] }, codexCtx: null, cursorCtx: null, opencodeCtx: null, antigravityCtx: null, piCtx: null, loading: false, openMemory: null, openDoc: null };
}

export function skillsPanelReducer(s: SkillsPanelState, a: SkillsPanelAction): SkillsPanelState {
  switch (a.type) {
    case "switch": return a.key === s.key ? s : initialSkillsPanelState(a.key);
    case "loaded": return a.key === s.key ? { ...s, ...a.patch } : s; // stale result: dropped
    case "open": return { ...s, ...a.patch };
  }
}

// An empty Pi resource list (or an older Host without the Pi method) is not Claude context.
export function shouldShowClaudeContext(hasClaudeContent: boolean, hasOtherContext: boolean, presence?: Partial<Record<AgentId, boolean>>, piPresent = false): boolean {
  return hasClaudeContent || !!presence?.claude || (!hasOtherContext && !presence?.pi && !piPresent);
}
