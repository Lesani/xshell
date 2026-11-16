// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { QuickActionsDialog } from "./QuickActionsDialog";
import { AGENT_IDS, type AgentId } from "../agents";
import type { Tab } from "../types";
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(), Channel: class {} }));
const hostOs = vi.hoisted(() => ({ value: null as "linux" | "macos" | "windows" | null }));
vi.mock("../hosts/useHosts", async importOriginal => ({
  ...(await importOriginal<typeof import("../hosts/useHosts")>()),
  useHostsSnapshot: () => ({ status: { r_windows: { os: hostOs.value } }, live: {} }),
}));
let root: Root, container: HTMLDivElement;
beforeEach(() => { vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true); Element.prototype.scrollIntoView = vi.fn(); container = document.createElement("div"); document.body.append(container); root = createRoot(container); });
afterEach(() => { act(() => root.unmount()); container.remove(); vi.unstubAllGlobals(); });
function render(tabs: Tab[]) {
  act(() => root.render(<QuickActionsDialog tabs={tabs} activeTabId="a" projectIcons={{}} pinnedProjects={[]} contextProject={null} hoveredProjectKey={null} linkedProjectKey={null} selectedProjectKey={null} hasActiveTab installedAgents={Object.fromEntries(AGENT_IDS.map(a => [a, true])) as Record<AgentId, boolean>} onSelectTab={vi.fn()} onCloseTab={vi.fn()} onNewChat={vi.fn()} onNewShell={vi.fn()} onGoHome={vi.fn()} onOpenSettings={vi.fn()} onToggleSidebar={vi.fn()} onClose={vi.fn()} />));
}
function search(value: string) { const input = container.querySelector("input")!; act(() => { Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(input, value); input.dispatchEvent(new Event("input", { bubbles: true })); }); }
describe("QuickActionsDialog effective live names", () => {
  it("renders_and_searches_custom_automatic_Project_and_shell_terms", () => {
    const tabs: Tab[] = [{ id: "a", type: "terminal", title: "Automatic agent", customTitle: "Shared agent", projectName: "Project", projectPath: "/p" }, { id: "s", type: "terminal", title: "Bash", customTitle: "Build shell", projectName: "Other Project", shellMode: "raw", shellId: "bash" }];
    render(tabs);
    expect(Array.from(container.querySelectorAll(".ts-row-title")).map(el => el.textContent)).toEqual(["Shared agent", "Build shell"]);
    expect(container.querySelectorAll(".ts-row-sub")[1].textContent).toBe("Other Project · Bash");
    for (const term of ["Shared agent", "Automatic agent", "Project"]) { search(term); expect(container.querySelector(".ts-row-title")?.textContent).toBe("Shared agent"); }
    search("bash"); expect(container.querySelector(".ts-row-title")?.textContent).toBe("Build shell");
    search("missing"); expect(container.querySelector(".ts-row-title")).toBeNull();
    search(""); render([{ ...tabs[0], title: "Latest automatic", customTitle: undefined }, { ...tabs[1], customTitle: undefined }]);
    expect(Array.from(container.querySelectorAll(".ts-row-title")).map(el => el.textContent)).toEqual(["Latest automatic", "Other Project"]);
  });
});

describe("QuickActionsDialog shells follow the Host's OS", () => {
  // Round 7: a Ring Host's OS arrives from its Daemon after the dialog may already be open;
  // the shell list must follow it, not keep the Linux fallback.
  it("re-lists the shells when the context Host's OS becomes known", () => {
    const project = { name: "tester", path: "C:\\Users\\tester", encoded_name: "x", session_count: 0, last_active: "", host: "r_windows" };
    const show = () => act(() => root.render(<QuickActionsDialog tabs={[]} activeTabId="" projectIcons={{}} pinnedProjects={[]} contextProject={project} hoveredProjectKey={null} linkedProjectKey={null} selectedProjectKey={null} hasActiveTab={false} installedAgents={Object.fromEntries(AGENT_IDS.map(a => [a, true])) as Record<AgentId, boolean>} onSelectTab={vi.fn()} onCloseTab={vi.fn()} onNewChat={vi.fn()} onNewShell={vi.fn()} onGoHome={vi.fn()} onOpenSettings={vi.fn()} onToggleSidebar={vi.fn()} onClose={vi.fn()} />));
    hostOs.value = null;
    show();
    const row = Array.from(container.querySelectorAll<HTMLElement>(".ts-row")).find(el => el.textContent?.includes("New shell in"))!;
    act(() => row.click());
    const names = () => Array.from(container.querySelectorAll(".ts-row-title")).map(el => el.textContent);
    expect(names()).toContain("Bash");
    expect(names()).not.toContain("Windows PowerShell");
    hostOs.value = "windows";
    show();
    expect(names()).toContain("Windows PowerShell");
    expect(names()).not.toContain("Bash");
  });
});
