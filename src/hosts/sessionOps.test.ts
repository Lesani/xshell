import { describe, expect, it, vi } from "vitest";
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(), Channel: class {} }));

import { planNewChat, planNewShell, planOpenSession, type OpenContext } from "./sessionOps";
import type { ProjectInfo, SessionInfo, Tab } from "../types";
import type { HostStatus } from "./types";

const H = "h_ab12cd34";
const NOW = 1_700_000_000_000;

function ctx(over: Partial<OpenContext> & { usable?: boolean; st?: HostStatus["status"] } = {}): OpenContext {
  const usable = over.usable ?? true;
  return {
    now: NOW,
    uuid: () => "11111111-2222-3333-4444-555555555555",
    fullscreenRendering: true,
    forceSyncOutput: true,
    isUsable: () => usable,
    isConfigured: (h) => h === H,
    status: () => ({ status: over.st ?? (usable ? "connected" : "offline") } as HostStatus),
    hostName: () => "Dev",
    statusLabel: (s) => (s?.status === "offline" ? "Offline" : "Connected"),
    ...over,
  };
}

function session(over: Partial<SessionInfo> = {}): SessionInfo {
  return { id: "s1", title: "Fix bug", timestamp: "", message_count: 0, project_name: "proj", project_path: "/home/u/proj", git_branch: "", claude_version: "", tool_use_count: 0, duration_ms: 0, model: "", context_tokens: 0, context_limit: 0, cost_usd: 0, is_authoritative_stats: false, daily_cost: {}, rate_limit_5h_pct: null, rate_limit_7d_pct: null, total_input_tokens: 0, total_cache_creation_tokens: 0, total_cache_read_tokens: 0, total_output_tokens: 0, daily_tokens: {}, agent: "claude", ...over };
}
const project: ProjectInfo = { name: "proj", path: "/home/u/proj", encoded_name: "-home-u-proj", session_count: 1, last_active: "" };
const remoteProject: ProjectInfo = { ...project, host: H };

describe("open session (amendment 18: one path for handleOpenSession and handleOpenSessionBackground)", () => {
  it("local: exactly the pre-hosts tab", () => {
    const plan = planOpenSession(session(), project, [], ctx());
    expect(plan).toEqual({ kind: "create", tab: { id: `terminal-s1-${NOW.toString(36)}`, type: "terminal", title: "Fix bug", sessionId: "s1", agent: "claude", projectPath: "/home/u/proj", projectName: "proj", lastActiveAt: NOW } });
  });

  it("local: project fallbacks when the session has no path", () => {
    const plan = planOpenSession(session({ project_path: "", project_name: "" }), project, [], ctx());
    expect(plan.kind === "create" && plan.tab).toMatchObject({ projectPath: "/home/u/proj", projectName: "proj" });
  });

  it("an open session is focused, not reopened", () => {
    const open: Tab = { id: "terminal-s1-x", type: "terminal", title: "T", sessionId: "s1" };
    expect(planOpenSession(session(), project, [open], ctx())).toEqual({ kind: "focus", tab: open });
  });

  it("remote: pending open with the agent spec and meta", () => {
    const plan = planOpenSession(session({ host: H }), remoteProject, [], ctx());
    if (plan.kind !== "create") throw new Error("expected create");
    expect(plan.tab).toMatchObject({ id: "remote-11111111-2222-3333-4444-555555555555", host: H, terminal: "11111111-2222-3333-4444-555555555555", sessionId: "s1", agent: "claude", projectPath: "/home/u/proj" });
    expect(plan.pending?.open).toEqual({
      host: H, state: "opening",
      spec: { agent: "claude", sessionId: "s1", cwd: "/home/u/proj", shellMode: "claude", shellId: null, shellCommand: null, fullscreenRendering: true, forceSyncOutput: true },
      meta: { title: "Fix bug", projectName: "proj", createdAt: NOW },
    });
  });

  it("remote offline: refused with the notice, nothing created", () => {
    expect(planOpenSession(session({ host: H }), remoteProject, [], ctx({ usable: false }))).toEqual({
      kind: "refuse", notice: "Can't start a terminal on Dev: Offline. Wait for it to connect or choose Reconnect now in Settings → Hosts.",
    });
    expect(planOpenSession(session({ host: H }), remoteProject, [], ctx({ usable: false, st: "incompatible" }))).toMatchObject({ kind: "refuse", notice: expect.stringContaining("Update xshell on this computer") });
    expect(planOpenSession(session({ host: "h_gone0000" }), undefined, [], ctx())).toMatchObject({ kind: "refuse", notice: expect.stringContaining("no longer configured") });
  });
});

describe("host-qualified session identity (amendment 19)", () => {
  it("the same session id on local and a remote host are different sessions", () => {
    const local: Tab = { id: "terminal-s1-x", type: "terminal", title: "T", sessionId: "s1" };
    const plan = planOpenSession(session({ host: H }), remoteProject, [local], ctx());
    expect(plan.kind).toBe("create");
    const remote: Tab = { id: "remote-u", type: "terminal", title: "R", sessionId: "s1", host: H, terminal: "u" };
    expect(planOpenSession(session(), project, [remote], ctx()).kind).toBe("create");
    expect(planOpenSession(session({ host: H }), remoteProject, [local, remote], ctx())).toEqual({ kind: "focus", tab: remote });
  });
});

describe("new chat / new shell", () => {
  it("local claude chat: pre-assigned session id, pre-hosts shape", () => {
    expect(planNewChat(project, "claude", ctx())).toEqual({ kind: "create", tab: { id: `terminal-new-${NOW}`, type: "terminal", title: "New Chat", projectPath: "/home/u/proj", projectName: "proj", shellMode: "claude", lastActiveAt: NOW, createdAt: NOW, sessionId: "11111111-2222-3333-4444-555555555555", agent: "claude" } });
  });

  it("local codex chat starts unlinked", () => {
    expect(planNewChat(project, "codex", ctx())).toEqual({ kind: "create", tab: { id: `terminal-new-${NOW}`, type: "terminal", title: "New Chat", projectPath: "/home/u/proj", projectName: "proj", shellMode: "claude", lastActiveAt: NOW, createdAt: NOW, agent: "codex" } });
  });

  it("remote chat: agent spawns directly (no shell wrapper), claude keeps its pre-assigned id", () => {
    const plan = planNewChat(remoteProject, "claude", ctx());
    if (plan.kind !== "create") throw new Error("expected create");
    expect(plan.tab.sessionId).toBe("11111111-2222-3333-4444-555555555555");
    expect(plan.pending?.open.spec).toEqual({ agent: "claude", sessionId: "11111111-2222-3333-4444-555555555555", cwd: "/home/u/proj", shellMode: "claude", shellId: null, shellCommand: null, fullscreenRendering: true, forceSyncOutput: true });
    expect(planNewChat(remoteProject, "codex", ctx({ usable: false })).kind).toBe("refuse");
  });

  it("local shell: pre-hosts shape, home when no project", () => {
    expect(planNewShell(project, "zsh", "Zsh", ctx())).toEqual({ kind: "create", tab: { id: `terminal-shell-${NOW}`, type: "terminal", title: "Zsh", projectPath: "/home/u/proj", projectName: "proj", shellMode: "raw", shellId: "zsh", lastActiveAt: NOW } });
    expect(planNewShell(null, "powershell", "Windows PowerShell", ctx())).toMatchObject({ kind: "create", tab: { projectPath: "", projectName: "~", shellId: "powershell" } });
  });

  it("remote shell: raw spec with the host shell's command in the project dir", () => {
    const plan = planNewShell(remoteProject, "bash", "Bash", ctx());
    if (plan.kind !== "create") throw new Error("expected create");
    expect(plan.pending?.open.spec).toEqual({ agent: null, sessionId: null, cwd: "/home/u/proj", shellMode: "raw", shellId: "bash", shellCommand: "bash", fullscreenRendering: true, forceSyncOutput: true });
  });
});

describe("skip permissions is never part of a remote open", () => {
  // A Daemon that predates it would silently drop the field; it is only set by term.relaunch.
  it("no spec carries the key", () => {
    const plans = [
      planOpenSession(session({ host: H }), remoteProject, [], ctx()),
      planNewChat(remoteProject, "claude", ctx()),
      planNewChat(remoteProject, "codex", ctx()),
      planNewShell(remoteProject, "bash", "Bash", ctx()),
    ];
    for (const plan of plans) {
      if (plan.kind !== "create" || !plan.pending) throw new Error("expected a remote create");
      expect(Object.keys(plan.pending.open.spec)).not.toContain("skipPermissions");
    }
  });
});

describe("local Daemon mode (ADR-0005): new Local Tabs are Daemon Terminals", () => {
  const UUID = "11111111-2222-3333-4444-555555555555";
  // The Local Host is never refused, whatever its status: the Tab waits while it starts.
  const lctx = (over: Partial<OpenContext> = {}) => ctx({ localDaemon: true, usable: false, isConfigured: () => false, ...over });

  it("open session: remote-<uuid> id, no host, pending on \"local\", agent spec without a wrapper", () => {
    const plan = planOpenSession(session(), project, [], lctx());
    if (plan.kind !== "create") throw new Error("expected create");
    expect(plan.tab).toEqual({ id: `remote-${UUID}`, terminal: UUID, type: "terminal", title: "Fix bug", sessionId: "s1", agent: "claude", projectPath: "/home/u/proj", projectName: "proj", lastActiveAt: NOW, createdAt: NOW });
    expect(plan.tab.host).toBeUndefined();
    expect(plan.pending).toEqual({ uuid: UUID, open: {
      host: "local", state: "opening",
      spec: { agent: "claude", sessionId: "s1", cwd: "/home/u/proj", shellMode: "claude", shellId: null, shellCommand: null, fullscreenRendering: true, forceSyncOutput: true },
      meta: { title: "Fix bug", projectName: "proj", createdAt: NOW },
    } });
  });

  it("new chat: claude gets its session id, other agents none; no wrapper", () => {
    const c = planNewChat(project, "claude", lctx());
    if (c.kind !== "create") throw new Error("expected create");
    expect(c.tab).toMatchObject({ id: `remote-${UUID}`, terminal: UUID, sessionId: UUID, agent: "claude", title: "New Chat" });
    expect(c.tab.host).toBeUndefined();
    expect(c.pending?.open).toMatchObject({ host: "local", spec: { agent: "claude", sessionId: UUID, shellId: null, shellCommand: null, cwd: "/home/u/proj" } });
    const x = planNewChat(project, "codex", lctx());
    if (x.kind !== "create") throw new Error("expected create");
    expect(x.tab.sessionId).toBeUndefined();
    expect(x.pending?.open.spec).toMatchObject({ agent: "codex", sessionId: null, shellId: null, shellCommand: null });
  });

  it("raw shell keeps its preset command; no project → home directory", () => {
    const s = planNewShell(null, "bash", "Bash", lctx());
    if (s.kind !== "create") throw new Error("expected create");
    expect(s.tab).toMatchObject({ id: `remote-${UUID}`, terminal: UUID, shellMode: "raw", shellId: "bash", projectPath: "", projectName: "~", title: "Bash" });
    expect(s.tab.host).toBeUndefined();
    expect(s.pending?.open).toMatchObject({ host: "local", spec: { agent: null, sessionId: null, cwd: "", shellMode: "raw", shellId: "bash", shellCommand: "bash" } });
    const p = planNewShell(project, "bash", "Bash", lctx());
    expect(p.kind === "create" && p.pending?.open.spec.cwd).toBe("/home/u/proj");
  });

  it("Remote plans are unchanged in local Daemon mode", () => {
    expect(planOpenSession(session({ host: H }), remoteProject, [], ctx({ localDaemon: true }))).toEqual(planOpenSession(session({ host: H }), remoteProject, [], ctx()));
    expect(planNewChat(remoteProject, "claude", ctx({ localDaemon: true, usable: false })).kind).toBe("refuse");
  });

  it("localDaemon false: exactly the in-process plans", () => {
    for (const c of [ctx(), ctx({ localDaemon: false })]) {
      expect(planOpenSession(session(), project, [], c)).toEqual({ kind: "create", tab: { id: `terminal-s1-${NOW.toString(36)}`, type: "terminal", title: "Fix bug", sessionId: "s1", agent: "claude", projectPath: "/home/u/proj", projectName: "proj", lastActiveAt: NOW } });
      expect(planNewShell(null, "bash", "Bash", c)).toEqual({ kind: "create", tab: { id: `terminal-shell-${NOW}`, type: "terminal", title: "Bash", projectPath: "", projectName: "~", shellMode: "raw", shellId: "bash", lastActiveAt: NOW } });
      const nc = planNewChat(project, "codex", c);
      expect(nc.kind === "create" && nc.tab.id).toBe(`terminal-new-${NOW}`);
      expect(nc.kind === "create" && nc.pending).toBeUndefined();
    }
  });
});
