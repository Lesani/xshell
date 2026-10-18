import { AGENT_IDS, type AgentId } from "../agents";
import type { CodexProjectInfo, ProjectInfo } from "../types";
import { normalizeProjectPath } from "../utils";
import { hostQuery, type Provenanced } from "./hostInvoke";
import { encodedNameFor } from "./projectKey";
import type { HostMethod } from "./methods";
import type { HostId } from "./types";

export type ProjectQuery = (host: HostId | undefined, method: HostMethod) => Promise<Provenanced<CodexProjectInfo[]>>;
export type AgentProjectLists = Partial<Record<AgentId, CodexProjectInfo[]>>;
export interface PickerRow {
  path: string;
  name: string;
  counts: Partial<Record<AgentId, number>>;
  lastActive: string;
}

// Input must be raw per-agent discovery, never App's merged allProjects: merged counts
// cannot identify which agent owns a session. Rows are built separately for each Host.
export function buildProjectRows(lists: AgentProjectLists, filter = ""): PickerRow[] {
  const rows = new Map<string, PickerRow>();
  for (const agent of AGENT_IDS) for (const p of lists[agent] ?? []) {
    const key = normalizeProjectPath(p.path);
    const row = rows.get(key) ?? { path: p.path, name: projectBasename(p.path), counts: {}, lastActive: "" };
    row.counts[agent] = p.session_count;
    if (p.last_active > row.lastActive) row.lastActive = p.last_active;
    rows.set(key, row);
  }
  const q = filter.trim().toLowerCase();
  return [...rows.values()].filter(r => !q || r.name.toLowerCase().includes(q) || r.path.toLowerCase().includes(q))
    .sort((a, b) => b.lastActive.localeCompare(a.lastActive));
}

function projectBasename(path: string): string {
  return path.replace(/[\\/]+$/, "").split(/[\\/]/).pop() || path;
}

// null means no source was available, not an empty Host. Callers must retain their
// previous Host list on null; optional Pi failure must not discard successful Claude data.
export function mergeHostProjects(host: HostId | undefined, claude: ProjectInfo[] | null, pi: CodexProjectInfo[] | null): ProjectInfo[] | null {
  if (claude === null && pi === null) return null;
  const projects = new Map<string, ProjectInfo>();
  for (const p of claude ?? []) projects.set(normalizeProjectPath(p.path), { ...p, host });
  for (const p of pi ?? []) {
    const key = normalizeProjectPath(p.path);
    const existing = projects.get(key);
    if (existing) projects.set(key, { ...existing, session_count: existing.session_count + p.session_count, last_active: p.last_active > existing.last_active ? p.last_active : existing.last_active });
    else projects.set(key, { name: projectBasename(p.path), path: p.path, encoded_name: encodedNameFor(p), session_count: p.session_count, last_active: p.last_active, host });
  }
  return [...projects.values()].sort((a, b) => b.last_active.localeCompare(a.last_active));
}

export async function loadHostProjects(host?: HostId, query: ProjectQuery = hostQuery): Promise<{ projects: ProjectInfo[]; claude: ProjectInfo[] | null; pi: CodexProjectInfo[] | null; stale: boolean } | null> {
  const [claude, pi] = await Promise.all([
    query(host, "list_claude_projects").catch(() => null),
    query(host, "list_pi_projects").catch(() => null),
  ]);
  const projects = mergeHostProjects(host, claude?.value as ProjectInfo[] ?? null, pi?.value ?? null);
  return projects === null ? null : { projects, claude: claude?.value as ProjectInfo[] ?? null, pi: pi?.value ?? null, stale: !!(claude?.stale || pi?.stale) };
}

export async function loadPickerProjects(host?: HostId, query: ProjectQuery = hostQuery): Promise<{ lists: AgentProjectLists; stale: boolean; loaded: boolean }> {
  const methods: Record<AgentId, HostMethod> = { claude: "list_claude_projects", codex: "list_codex_projects", cursor: "list_cursor_projects", opencode: "list_opencode_projects", antigravity: "list_antigravity_projects", pi: "list_pi_projects" };
  const results = await Promise.all(AGENT_IDS.map(async id => ({ id, result: await query(host, methods[id]).catch(() => null) })));
  return {
    lists: Object.fromEntries(results.filter(x => x.result !== null).map(x => [x.id, x.result!.value])),
    stale: results.some(x => x.result?.stale),
    loaded: results.some(x => x.result !== null),
  };
}
