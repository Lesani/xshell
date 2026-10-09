import { beforeEach, describe, expect, it, vi } from "vitest";

// With no Remote Hosts configured, every Host-side call must reach `invoke` with exactly the
// command and arguments the pre-hosts code used.
const invoke = vi.fn(async () => null);
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...a: unknown[]) => (invoke as any)(...a) }));
vi.mock("@tauri-apps/plugin-store", () => ({ load: vi.fn() }));

import { hostInvoke } from "./hostInvoke";

beforeEach(() => invoke.mockClear());

describe("local parity: Host-side calls", () => {
  // [command, args] exactly as App/TerminalTab/FileExplorerPanel/SkillsPanel sent them before.
  const cases: [Parameters<typeof hostInvoke>[1], Record<string, unknown> | undefined][] = [
    ["list_claude_projects", undefined],
    ["get_all_recent_sessions", { limit: 100 }],
    ["get_sessions", { encodedName: "-home-u-p" }],
    ["get_git_status", { cwd: "/home/u/p" }],
    ["get_git_log", { cwd: "/home/u/p", limit: 25 }],
    ["git_stage", { cwd: "/home/u/p", paths: ["a.ts"] }],
    ["git_unstage", { cwd: "/home/u/p", paths: ["a.ts"] }],
    ["git_discard", { cwd: "/home/u/p", path: "a.ts", mode: "unstaged" }],
    ["git_checkout", { cwd: "/home/u/p", branch: "main" }],
    ["git_diff", { cwd: "/home/u/p", path: "a.ts", mode: "staged" }],
    ["list_git_branches", { cwd: "/home/u/p" }],
    ["list_project_session_ids", { cwd: "/home/u/p" }],
    ["detect_session_branch", { cwd: "/home/u/p", currentSessionId: "s", knownSessionIds: ["s"] }],
    ["list_dir", { path: "/home/u/p" }],
    ["search_dir", { root: "/home/u/p", query: "x", limit: 300 }],
    ["save_dropped_file", { bytesBase64: "AA==", name: "clipboard.png" }],
    ["read_text_file", { path: "/home/u/p/CLAUDE.md" }],
    ["get_project_skills", { projectPath: "/home/u/p" }],
    ["get_project_memories", { projectPath: "/home/u/p" }],
    ["get_username", undefined],
    ["get_home_dir", undefined],
    ["detect_agent_binary", { binary: "claude" }],
    ["probe_statusline_setup", undefined],
    ["get_global_rate_limits", undefined],
    ["get_claude_cost_summary", undefined],
    ["get_codex_usage", undefined],
    ["list_codex_projects", undefined],
  ];
  for (const [cmd, args] of cases) {
    it(`${cmd} → invoke(${cmd}${args ? ", args" : ""})`, async () => {
      await hostInvoke(undefined, cmd, args);
      expect(invoke.mock.calls).toEqual([args === undefined ? [cmd] : [cmd, args]]);
    });
  }
});
