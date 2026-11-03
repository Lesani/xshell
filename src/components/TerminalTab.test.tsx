// @vitest-environment happy-dom
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";
import { TerminalTab } from "./TerminalTab";
import type { Tab } from "../types";
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(), Channel: class {} }));
vi.mock("../hosts/useHosts", () => ({ useHostsSnapshot: () => ({ configs: [], status: {}, live: {}, agents: {}, epochs: {} }), useHostLive: () => null, useHostStatus: () => undefined }));
function heading(tab: Tab) {
  const html = renderToStaticMarkup(<TerminalTab tab={tab} isActive={false} gitLazyPolling gitChangesTree={false} rightPanel="closed" onRightPanelChange={vi.fn()} terminalBgColor="#141413" defaultFontSize={14} defaultShellId="bash" fullscreenRendering={false} forceSyncOutput={false} webglRendering={false} terminalFontWeight={400} eagerInit={false} theme="dark" projectEncodedName="" showTerminalHeaderStats={false} onBranchSwitch={vi.fn()} onSkipPermissionsChange={vi.fn()} />);
  const container = document.createElement("div"); container.innerHTML = html;
  return container.querySelector(".terminal-header-label")?.textContent;
}
describe("TerminalTab pane heading", () => {
  it.each(["claude", "raw"] as const)("actual_pane_uses_custom_then_latest_automatic_name_and_keeps_Project (%s)", shellMode => {
    const tab: Tab = { id: "tab", type: "terminal", title: "Old automatic", customTitle: "Shared pane", projectName: "Project", projectPath: "/p", groupId: "personal-group", shellMode };
    expect(heading(tab)).toBe("Project·Shared pane");
    expect(heading({ ...tab, title: "New automatic" })).toBe("Project·Shared pane");
    expect(heading({ ...tab, title: "New automatic", customTitle: undefined })).toBe("Project·New automatic");
    expect(tab.title).toBe("Old automatic");
  });
});
