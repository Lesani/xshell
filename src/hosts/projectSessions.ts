import type { ProjectInfo, SessionInfo } from "../types";
import { hostQuery } from "./hostInvoke";
import { encodedNameFor } from "./projectKey";

// A project's session list with its provenance: `stale` when it came from the offline cache.
// Remote sessions are stamped with their Host at this boundary.
export async function loadProjectSessions(project: ProjectInfo): Promise<{ sessions: SessionInfo[]; stale: boolean }> {
  const encodedName = encodedNameFor(project);
  let sessions: SessionInfo[] = [];
  let stale = false;
  try {
    const r = await hostQuery<SessionInfo[]>(project.host, "get_sessions", { encodedName });
    sessions = r.value;
    stale = r.stale;
  } catch (_) { sessions = []; }
  if (project.host) sessions = sessions.map(x => ({ ...x, host: project.host }));
  return { sessions, stale };
}
