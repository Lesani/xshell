import { isValidElement, type ReactNode, type ReactElement } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { AGENT_IDS, type AgentId } from "../agents";
import { AgentPickerDialog } from "./AgentPickerDialog";
import { RecentSessionsDropdown } from "./TabBar";
import type { ProjectInfo } from "../types";

// Inspect the components' actual rendered handlers without a browser dependency.
const { effects } = vi.hoisted(() => ({ effects: [] as (() => void | (() => void))[] }));
vi.mock("react", async importOriginal => ({
  ...await importOriginal<typeof import("react")>(),
  useRef: () => ({ current: null }),
  useState: (value: unknown) => [value, vi.fn()],
  useEffect: (effect: () => void | (() => void)) => { effects.push(effect); },
}));
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(), Channel: class {} }));

const project: ProjectInfo = { name: "project", path: "/work/project", encoded_name: "project", session_count: 0, last_active: "" };
type NodeProps = { children?: ReactNode; onClick?: () => void };
function nodes(node: ReactNode): ReactElement<NodeProps>[] {
  if (Array.isArray(node)) return node.flatMap(nodes);
  if (!isValidElement<NodeProps>(node)) return [];
  return [node, ...nodes(node.props.children)];
}
function text(node: ReactNode): string {
  if (typeof node === "string") return node;
  if (Array.isArray(node)) return node.map(text).join("");
  return isValidElement<NodeProps>(node) ? text(node.props.children) : "";
}
function click(tree: ReactNode, label: string) {
  const matches = nodes(tree).filter(n => n.props.onClick && text(n) === label);
  expect(matches).toHaveLength(1);
  matches[0].props.onClick!();
}
function picker(agents: AgentId[] = []) {
  const onPick = vi.fn(), onShell = vi.fn(), onClose = vi.fn(), onOpenSettings = vi.fn();
  const tree = AgentPickerDialog({ project, agents, onPick, onShell, onClose, onOpenSettings });
  return { tree, onPick, onShell, onClose, onOpenSettings };
}
afterEach(() => { effects.length = 0; vi.unstubAllGlobals(); });

describe("Project Shell picker component", () => {
  for (const agents of [[], ["claude"], ["claude", "codex"]] as AgentId[][]) {
    it(`always offers a separate Shell callback with ${agents.length} installed harnesses`, () => {
      const p = picker(agents);
      click(p.tree, "Shell");
      expect(p.onShell).toHaveBeenCalledExactlyOnceWith();
      expect(p.onPick).not.toHaveBeenCalled();
      if (agents.length) {
        click(p.tree, "Claude Code");
        expect(p.onPick).toHaveBeenCalledExactlyOnceWith("claude");
      }
    });
  }
  it("cancel starts nothing", () => {
    const listeners = new Map<string, (e: { key?: string }) => void>();
    vi.stubGlobal("document", {
      addEventListener: (name: string, cb: (e: { key?: string }) => void) => listeners.set(name, cb),
      removeEventListener: vi.fn(),
    });
    const p = picker(["claude"]);
    const cleanup = effects[0]();
    listeners.get("keydown")!({ key: "Escape" });
    expect(p.onClose).toHaveBeenCalledExactlyOnceWith();
    expect(p.onPick).not.toHaveBeenCalled();
    expect(p.onShell).not.toHaveBeenCalled();
    cleanup?.();
  });
  it("Settings remains available without starting a Terminal", () => {
    const p = picker();
    click(p.tree, " Open Settings");
    expect(p.onOpenSettings).toHaveBeenCalledOnce();
    expect(p.onShell).not.toHaveBeenCalled();
    expect(p.onPick).not.toHaveBeenCalled();
  });
});

describe("TabBar New chat callback", () => {
  function dropdown(installed: AgentId[]) {
    vi.stubGlobal("window", { innerWidth: 1000, innerHeight: 800 });
    vi.stubGlobal("navigator", { userAgent: "Linux" });
    const onNewChat = vi.fn(), onClose = vi.fn();
    const tree = RecentSessionsDropdown({ project, displayName: project.name, openSessionIds: new Set(), anchorRect: { left: 0, right: 20, bottom: 20 } as DOMRect, anchorEl: {} as HTMLElement, installedAgents: Object.fromEntries(AGENT_IDS.map(a => [a, installed.includes(a)])) as Record<AgentId, boolean>, onPick: vi.fn(), onPickShell: vi.fn(), onNewChat, onClose });
    return { tree, onNewChat, onClose };
  }
  for (const installed of [[], ["claude"]] as AgentId[][]) {
    it(`generic New chat row passes no agent with ${installed.length} installed harnesses`, () => {
      const d = dropdown(installed);
      click(d.tree, "New chat in project");
      expect(d.onNewChat).toHaveBeenCalledExactlyOnceWith();
      expect(d.onClose).toHaveBeenCalledOnce();
    });
  }
  it("named harness rows can pass an explicit agent", () => {
    const d = dropdown(["claude", "codex"]);
    click(d.tree, "Codex in project");
    expect(d.onNewChat).toHaveBeenCalledExactlyOnceWith("codex");
  });
});
