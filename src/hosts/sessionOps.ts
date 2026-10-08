import type { ProjectInfo, SessionInfo, Tab } from "../types";
import type { AgentId } from "../agents";
import { getShellById } from "../shells";
import { sessionKeyOf, sessionKeyOfTab } from "./projectKey";
import { remoteTabId } from "./reconcile";
import { fmt } from "./strings";
import type { HostId, HostStatus, LaunchSpec, TerminalMeta } from "./types";
import type { PendingOpen } from "./terminalTransport";

// Pure planning for every "start a Terminal" entry point. App applies the plan; tests check
// the Local plans are exactly the pre-hosts tabs and the Remote ones carry the right spec.

export interface OpenContext {
  now: number;
  uuid: () => string;
  fullscreenRendering: boolean;
  forceSyncOutput: boolean;
  isUsable: (host: HostId) => boolean;
  isConfigured: (host: HostId) => boolean;
  status: (host: HostId) => HostStatus | undefined;
  hostName: (host: HostId) => string;
  statusLabel: (s: HostStatus | undefined) => string;
}

export type Plan =
  | { kind: "focus"; tab: Tab }
  | { kind: "refuse"; notice: string }
  | { kind: "create"; tab: Tab; pending?: { uuid: string; open: PendingOpen } };

// The refusal notice for a Host that cannot start Terminals right now, or null when it can.
export function refusal(host: HostId, ctx: OpenContext): string | null {
  if (!ctx.isConfigured(host)) return fmt("notice.unknownHost");
  if (ctx.isUsable(host)) return null;
  const s = ctx.status(host);
  const name = ctx.hostName(host);
  if (s?.status === "incompatible") return fmt("notice.newTerminal.incompatible", { host: name });
  return fmt("notice.newTerminal.offline", { host: name, status: ctx.statusLabel(s) });
}

function remoteTab(host: HostId, base: Omit<Tab, "id" | "host" | "terminal">, spec: LaunchSpec, meta: TerminalMeta, ctx: OpenContext): Plan {
  const uuid = ctx.uuid();
  const tab: Tab = { ...base, id: remoteTabId(uuid), host, terminal: uuid };
  return { kind: "create", tab, pending: { uuid, open: { host, spec, meta, state: "opening" } } };
}

// Amendment 18: the single host-aware open path behind handleOpenSession and
// handleOpenSessionBackground.
export function planOpenSession(session: SessionInfo, project: ProjectInfo | undefined, tabs: Tab[], ctx: OpenContext): Plan {
  const existing = tabs.find(t => sessionKeyOfTab(t) === sessionKeyOf(session));
  if (existing) return { kind: "focus", tab: existing };
  const projectPath = session.project_path || project?.path || "";
  const projectName = session.project_name || project?.name || "";
  const host = session.host;
  if (!host) {
    // Unique tab id (not derived from session id) — otherwise, a tab that auto-switches its
    // sessionId after /branch would leave its original session id "free", and a later re-open
    // of that session would generate a colliding tab id.
    const tabId = `terminal-${session.id}-${ctx.now.toString(36)}`;
    return { kind: "create", tab: { id: tabId, type: "terminal", title: session.title, sessionId: session.id, agent: session.agent, projectPath, projectName, lastActiveAt: ctx.now } };
  }
  const refused = refusal(host, ctx);
  if (refused) return { kind: "refuse", notice: refused };
  return remoteTab(host,
    { type: "terminal", title: session.title, sessionId: session.id, agent: session.agent, projectPath, projectName, lastActiveAt: ctx.now, createdAt: ctx.now },
    { agent: session.agent, sessionId: session.id, cwd: projectPath, shellMode: "claude", shellId: null, shellCommand: null, fullscreenRendering: ctx.fullscreenRendering, forceSyncOutput: ctx.forceSyncOutput },
    { title: session.title, projectName, createdAt: ctx.now },
    ctx);
}

// New agent chat in `project` with a resolved agent. Claude gets a pre-assigned session id.
export function planNewChat(project: ProjectInfo, agent: AgentId, ctx: OpenContext): Plan {
  const base = { type: "terminal" as const, title: "New Chat", projectPath: project.path, projectName: project.name, shellMode: "claude" as const, lastActiveAt: ctx.now, createdAt: ctx.now };
  const sessionId = agent === "claude" ? ctx.uuid() : undefined;
  if (!project.host) {
    const tabId = `terminal-new-${ctx.now}`;
    if (agent === "claude") return { kind: "create", tab: { ...base, id: tabId, sessionId, agent: "claude" as const } };
    return { kind: "create", tab: { ...base, id: tabId, agent } };
  }
  const refused = refusal(project.host, ctx);
  if (refused) return { kind: "refuse", notice: refused };
  const tabBase = sessionId ? { ...base, sessionId, agent } : { ...base, agent };
  return remoteTab(project.host, tabBase,
    // Decision 9: the agent spawns directly on the Host (no Desktop shell wrapper).
    { agent, sessionId: sessionId ?? null, cwd: project.path, shellMode: "claude", shellId: null, shellCommand: null, fullscreenRendering: ctx.fullscreenRendering, forceSyncOutput: ctx.forceSyncOutput },
    { title: "New Chat", projectName: project.name, createdAt: ctx.now },
    ctx);
}

// Raw shell. project === null → Local home directory.
export function planNewShell(project: ProjectInfo | null, shellId: string, shellName: string, ctx: OpenContext): Plan {
  if (!project?.host) {
    const tabId = `terminal-shell-${ctx.now}`;
    return { kind: "create", tab: { id: tabId, type: "terminal" as const, title: shellName, projectPath: project?.path || "", projectName: project?.name || "~", shellMode: "raw", shellId, lastActiveAt: ctx.now } };
  }
  const refused = refusal(project.host, ctx);
  if (refused) return { kind: "refuse", notice: refused };
  const command = getShellById(shellId)?.command ?? null;
  return remoteTab(project.host,
    { type: "terminal", title: shellName, projectPath: project.path, projectName: project.name || "~", shellMode: "raw", shellId, lastActiveAt: ctx.now, createdAt: ctx.now },
    { agent: null, sessionId: null, cwd: project.path, shellMode: "raw", shellId, shellCommand: command, fullscreenRendering: ctx.fullscreenRendering, forceSyncOutput: ctx.forceSyncOutput },
    { title: shellName, projectName: project.name || "~", createdAt: ctx.now },
    ctx);
}
