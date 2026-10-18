import type { Tab } from "../types";

// shellMode "claude" means an agent Terminal, not necessarily the Claude CLI.
// Only Claude forks create a new JSONL session ID; Pi branches keep their header ID.
export function usesClaudeBranchDetection(tab: Pick<Tab, "agent" | "shellMode">): boolean {
  return (tab.shellMode ?? "claude") === "claude" && (tab.agent ?? "claude") === "claude";
}
