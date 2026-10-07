use serde::{Deserialize, Serialize};

// ── Codex project context ─────────────────────────────────────────────
// What the context tree shows for Codex. Returned as GENERIC titled sections rather than
// Codex-specific fields — the frontend renders sections without knowing the agent, which
// is the pattern every future agent's context command should follow (Claude's richer
// panel predates this and stays bespoke).

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AgentContextItem {
    pub name: String,
    pub detail: String, // secondary line (scope, command, …); empty when none
    pub path: String,   // openable file path; empty when not file-backed
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AgentContextSection {
    pub title: String,
    pub items: Vec<AgentContextItem>,
}
