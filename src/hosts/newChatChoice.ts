import type { AgentId } from "../agents";

export type NewChatChoice = { kind: "agent"; agent: AgentId } | { kind: "picker" };

// An explicit harness action wins; Ask never skips the project-scoped choice dialog.
export function resolveNewChatChoice(defaultAgent: "ask" | AgentId, installed: AgentId[], explicitAgent?: AgentId): NewChatChoice {
  if (explicitAgent) return { kind: "agent", agent: explicitAgent };
  if (defaultAgent !== "ask" && installed.includes(defaultAgent)) return { kind: "agent", agent: defaultAgent };
  return { kind: "picker" };
}
