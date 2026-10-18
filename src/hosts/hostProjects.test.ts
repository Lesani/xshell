import { describe, expect, it, vi } from "vitest";
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
import { buildProjectRows, loadHostProjects, loadPickerProjects, mergeHostProjects, type ProjectQuery } from "./hostProjects";
import type { CodexProjectInfo, ProjectInfo } from "../types";

const pi: CodexProjectInfo = { path: "/PiOnly", session_count: 3, last_active: "2026-10-18" };
const claude: ProjectInfo = { name: "Mixed", path: "/Mixed", encoded_name: "-Mixed", session_count: 2, last_active: "2026-10-17" };
const value = (v: CodexProjectInfo[], stale = false) => ({ value: v, stale, at: 0 });
const query: ProjectQuery = async (_host, method) => {
  if (method === "list_pi_projects") return value([pi, { ...pi, path: claude.path }]);
  if (method === "list_claude_projects") return value([claude]);
  return value([]);
};

describe("Host Pi project discovery", () => {
  it("Pi-only projects merge by Host and path", async () => {
    const local = await loadHostProjects(undefined, query);
    const remote = await loadHostProjects("h_ab12cd34", query);
    expect(local?.projects.find(p => p.path === pi.path)).toMatchObject({ name: "PiOnly", encoded_name: "-PiOnly", session_count: 3, host: undefined });
    expect(remote?.projects).toHaveLength(2);
    expect(remote?.projects.every(p => p.host === "h_ab12cd34")).toBe(true);
    expect(remote?.projects.find(p => p.path === claude.path)).toMatchObject({ session_count: 5, last_active: pi.last_active, encoded_name: claude.encoded_name });
  });

  it("old Host missing Pi methods preserves existing projects", async () => {
    const old: ProjectQuery = async (_host, method) => {
      if (method === "list_pi_projects") throw new Error("unknown method");
      return value([claude], true);
    };
    const result = await loadHostProjects("h_ab12cd34", old);
    expect(result?.projects).toEqual([{ ...claude, host: "h_ab12cd34" }]);
    expect(result?.pi).toBeNull();
    expect(result?.stale).toBe(true);
    expect(await loadHostProjects(undefined, async () => { throw new Error("offline, no cache"); })).toBeNull();
    expect(mergeHostProjects(undefined, null, null)).toBeNull();
    expect(mergeHostProjects(undefined, [], [])).toEqual([]);
  });

  it("Pi discovery works without Claude storage", async () => {
    const onlyPi: ProjectQuery = async (_host, method) => {
      if (method === "list_claude_projects") throw new Error("missing Claude storage");
      return value([pi]);
    };
    expect((await loadHostProjects(undefined, onlyPi))?.projects).toHaveLength(1);
  });

  it("Pi-only picker rows have exact counts and no Claude badge", async () => {
    const result = await loadPickerProjects(undefined, query);
    const rows = buildProjectRows(result.lists);
    expect(rows.find(p => p.path === pi.path)?.counts).toEqual({ pi: 3 });
    expect(rows.find(p => p.path === claude.path)?.counts).toEqual({ claude: 2, pi: 3 });
  });

  it("picker uses raw per-agent discovery, not parent-merged counts", async () => {
    const parentMerged = (await loadHostProjects(undefined, query))!.projects;
    expect(parentMerged.find(p => p.path === claude.path)?.session_count).toBe(5);
    const picker = await loadPickerProjects(undefined, query);
    expect(buildProjectRows(picker.lists).find(p => p.path === claude.path)?.counts).toEqual({ claude: 2, pi: 3 });
  });

  it("picker tolerates missing optional Pi methods independently", async () => {
    const result = await loadPickerProjects("h_ab12cd34", async (host, method) => {
      if (method === "list_pi_projects") throw new Error("unsupported");
      return query(host, method);
    });
    expect(result.loaded).toBe(true);
    expect(buildProjectRows(result.lists)).toMatchObject([{ path: claude.path, counts: { claude: 2 } }]);
    expect(result.lists.pi).toBeUndefined();
  });

  it("merges Windows spelling but does not alias Unix case", () => {
    const rows = buildProjectRows({ claude: [{ ...claude, path: "C:\\Work\\P" }], pi: [{ ...pi, path: "c:/work/p/" }, { ...pi, path: "/Case" }, { ...pi, path: "/case" }] });
    expect(rows).toHaveLength(3);
    expect(rows.find(r => r.name === "P")?.counts).toEqual({ claude: 2, pi: 3 });
    const merged = mergeHostProjects(undefined, [{ ...claude, path: "C:\\Work\\P" }], [{ ...pi, path: "c:/work/p/" }]);
    expect(merged).toHaveLength(1);
    expect(merged?.[0].session_count).toBe(5);
  });
});
