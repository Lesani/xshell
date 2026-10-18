import { expect, it, vi } from "vitest";
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(), Channel: class {} }));
import type { ProjectInfo } from "../types";
import { resolveTabProject, runtimeProjectKey, titlePollProjects } from "./projectRuntime";
import { planNewShell, type OpenContext } from "./sessionOps";

const project = (path: string, host?: string): ProjectInfo => ({ path, host, name: path, encoded_name: "", session_count: 1, last_active: "" });

it("active case-distinct Unix project opens Shell in its own cwd", () => {
  const upper = project("/Work/P", "h_test");
  const lower = project("/work/p", "h_test");
  const selected = resolveTabProject([upper, lower], { host: "h_test", projectPath: lower.path, projectName: "P" });
  expect(selected).toBe(lower);
  const context: OpenContext = { now: 1, uuid: () => "shell-terminal", fullscreenRendering: true, forceSyncOutput: true, isUsable: () => true, isConfigured: () => true, status: () => undefined, hostName: h => h, statusLabel: () => "connected" };
  const plan = planNewShell(selected, "bash", "Bash", context);
  expect(plan.kind).toBe("create");
  if (plan.kind === "create") expect(plan.pending?.open.spec.cwd).toBe(lower.path);
});

it("title polling preserves case-distinct Unix Projects and Host identity", () => {
  const refs = titlePollProjects([
    { projectPath: "/Work/P", host: "h_one" },
    { projectPath: "/work/p", host: "h_one" },
    { projectPath: "/work/p", host: "h_two" },
  ]);
  expect(refs.size).toBe(3);
  expect([...refs.values()].map(p => p.path)).toEqual(["/Work/P", "/work/p", "/work/p"]);
});

it("runtime Windows aliases share identity without changing the selected raw cwd", () => {
  const recorded = project("C:\\Work\\P", "h_win");
  expect(runtimeProjectKey(recorded)).toBe(runtimeProjectKey(project("c:/work/p/", "h_win")));
  expect(resolveTabProject([recorded], { host: "h_win", projectPath: "c:/work/p/", projectName: "P" })).toBe(recorded);
});
