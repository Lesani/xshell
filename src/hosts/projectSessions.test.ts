import { beforeEach, describe, expect, it, vi } from "vitest";
const invoke = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...a: unknown[]) => invoke(...a) }));
vi.mock("@tauri-apps/plugin-store", () => ({ load: vi.fn() }));

import { loadProjectSessions } from "./projectSessions";
import { cache } from "./cache";
import type { ProjectInfo } from "../types";

const H = "h_ab12cd34";
const local: ProjectInfo = { name: "p", path: "/home/u/p", encoded_name: "", session_count: 0, last_active: "" };
const remote: ProjectInfo = { ...local, host: H };

beforeEach(() => { invoke.mockReset(); cache._reset(); });

// Sol finding 5: the project page keeps each result's provenance; a refetch after the
// Host reconnects replaces the cached list and clears "stale".
describe("project page sessions", () => {
  it("local: today's call, never stale, not stamped", async () => {
    invoke.mockResolvedValueOnce([{ id: "s" }]);
    const r = await loadProjectSessions(local);
    expect(invoke.mock.calls).toEqual([["get_sessions", { encodedName: "-home-u-p" }]]);
    expect(r).toEqual({ sessions: [{ id: "s" }], stale: false });
  });

  it("offline shows the cache as stale; the refetch on reconnect clears it", async () => {
    invoke.mockResolvedValueOnce([{ id: "old" }]);
    await loadProjectSessions(remote);                  // warms the cache
    invoke.mockRejectedValueOnce({ code: "offline", message: "down" });
    const offline = await loadProjectSessions(remote);
    expect(offline).toEqual({ sessions: [{ id: "old", host: H }], stale: true });
    invoke.mockResolvedValueOnce([{ id: "old" }, { id: "new" }]); // Host usable again → App refetches
    const fresh = await loadProjectSessions(remote);
    expect(fresh.stale).toBe(false);
    expect(fresh.sessions.map(s => s.id)).toEqual(["old", "new"]);
  });
});
