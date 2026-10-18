import { describe, expect, it, vi } from "vitest";
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(), Channel: class {} }));
import { defaultShellForHost } from "../shells";
import { planNewShell, type OpenContext } from "./sessionOps";
import type { ProjectInfo } from "../types";

const project: ProjectInfo = { host: "h_ab12cd34", name: "project", path: "C:\\work\\project", encoded_name: "project", session_count: 0, last_active: "" };
const context: OpenContext = {
  now: 1, uuid: () => "terminal-uuid", fullscreenRendering: false, forceSyncOutput: false,
  isConfigured: () => true, isUsable: () => true, status: () => undefined,
  hostName: () => "Host", statusLabel: () => "Offline",
};
function shellPlan(os: string | undefined, p = project, ctx = context) {
  const shell = defaultShellForHost(!!p.host, os, "fish", "linux");
  return planNewShell(p, shell.id, shell.name, ctx);
}

describe("Project Shell", () => {
  it("Shell uses remote Windows PowerShell and project cwd", () => {
    const plan = shellPlan("windows");
    expect(plan).toMatchObject({ kind: "create", tab: { host: project.host, projectPath: project.path, shellMode: "raw", shellId: "powershell" }, pending: { open: { host: project.host, spec: { cwd: project.path, shellMode: "raw", shellCommand: "powershell.exe" } } } });
  });
  it("Shell uses remote Linux default rather than Desktop default", () => {
    expect(shellPlan("linux", { ...project, path: "/work/project" })).toMatchObject({ kind: "create", pending: { open: { spec: { cwd: "/work/project", shellId: "bash", shellCommand: "bash" } } } });
    expect(defaultShellForHost(true, "linux", "powershell", "windows").id).toBe("bash");
  });
  it("remote macOS uses Zsh and unknown metadata keeps the Linux fallback", () => {
    expect(defaultShellForHost(true, "macos", "fish").id).toBe("zsh");
    expect(defaultShellForHost(true, undefined, "powershell", "windows").id).toBe("bash");
    expect(defaultShellForHost(true, "unknown", "powershell", "windows").id).toBe("bash");
  });
  it("local valid configured shell is preserved, invalid presets use the OS default", () => {
    expect(defaultShellForHost(false, undefined, "fish", "linux").id).toBe("fish");
    expect(defaultShellForHost(false, undefined, "pwsh", "windows").id).toBe("pwsh");
    expect(defaultShellForHost(false, undefined, "bash", "windows").id).toBe("powershell");
    expect(defaultShellForHost(false, undefined, "missing", "macos").id).toBe("zsh");
  });
  it("offline Shell is refused", () => {
    const uuid = vi.fn();
    expect(shellPlan("windows", project, { ...context, uuid, isUsable: () => false })).toMatchObject({ kind: "refuse", notice: expect.stringContaining("Offline") });
    expect(uuid).not.toHaveBeenCalled();
  });
  it("Shell has no agent or session identity", () => {
    for (const ctx of [context, { ...context, localDaemon: true }]) {
      for (const p of [project, { ...project, host: undefined }]) {
        const plan = shellPlan("windows", p, ctx);
        if (plan.kind !== "create") throw new Error("expected create");
        expect(plan.tab.agent).toBeUndefined();
        expect(plan.tab.sessionId).toBeUndefined();
        if (plan.pending) expect(plan.pending.open.spec).toMatchObject({ agent: null, sessionId: null, shellMode: "raw" });
      }
    }
  });
});
