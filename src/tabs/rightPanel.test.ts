import { describe, expect, it } from "vitest";
import type { Tab } from "../types";
import { adoptRightPanel, decodeRightPanels, defaultRightPanel, ensureRightPanels, pruneRightPanels, removeRightPanelHost, rightPanelKey, setRightPanel } from "./rightPanel";

const tab = (id: string, host?: string): Tab => ({ id: `remote-${id}`, terminal: id, host, type: "terminal", title: "Agent", projectPath: "/same", shellMode: "claude" });

describe("rightPanel", () => {
  it("qualifies stable Terminal identity by wire Host, with a separate in-process namespace", () => {
    expect(rightPanelKey(tab("one"))).toBe(JSON.stringify(["daemon", "local", "one"]));
    expect(rightPanelKey(tab("one", "other"))).not.toBe(rightPanelKey(tab("one")));
    expect(rightPanelKey({ ...tab("one"), terminal: undefined })).toBe(JSON.stringify(["in-process", "remote-one"]));
    expect(rightPanelKey({ ...tab("one"), title: "Renamed", projectPath: "/else", groupId: "g" })).toBe(rightPanelKey(tab("one")));
  });
  it("keeps Files, Git and explicit Closed independent within one Project and across Hosts", () => {
    const a = tab("a"), b = tab("b"), c = tab("a", "other");
    const panels = setRightPanel(setRightPanel(setRightPanel({}, a, "files"), b, "git"), c, "closed");
    expect(ensureRightPanels(panels, [a, b, c], true)).toBe(panels);
    expect([a, b, c].map(t => panels[rightPanelKey(t)])).toEqual(["files", "git", "closed"]);
  });
  it.each([undefined, "claude", "raw"] as const)("preserves the actual legacy shellMode default (%s)", shellMode => {
    const t = { ...tab("a"), shellMode, agent: "codex" as const };
    expect(defaultRightPanel(t, true)).toBe(!shellMode || shellMode === "claude" ? "files" : "closed");
    expect(defaultRightPanel(t, false)).toBe("closed");
  });
  it("records missing defaults once, not on global setting or metadata changes", () => {
    const a = tab("a");
    const panels = ensureRightPanels({}, [a], false);
    expect(panels[rightPanelKey(a)]).toBe("closed");
    expect(ensureRightPanels(panels, [{ ...a, title: "new" }], true)).toBe(panels);
    expect(ensureRightPanels(panels, [a, tab("b")], true)[rightPanelKey(tab("b"))]).toBe("files");
  });
  it("hydrates only versioned enum entries with valid namespaces; malformed entries remain missing", () => {
    const a = tab("a"), b = tab("b"), local = { ...tab("c"), terminal: undefined };
    const entries = { [rightPanelKey(a)]: "closed", [rightPanelKey(b)]: "wide", [rightPanelKey(local)]: "git", broken: "files", '["daemon","local",""]': "files" };
    expect(decodeRightPanels({ version: 1, entries })).toEqual({ [rightPanelKey(a)]: "closed", [rightPanelKey(local)]: "git" });
    for (const value of [null, [], {}, { version: 2, entries }, { version: 1, entries: [] }]) expect(decodeRightPanels(value)).toEqual({});
    expect(decodeRightPanels(JSON.parse(JSON.stringify({ version: 1, entries: { [rightPanelKey(a)]: "files" } })))[rightPanelKey(a)]).toBe("files");
  });
  it("adopts before pruning, preserves an existing target and transfers only into missing", () => {
    const a = tab("a"), b = tab("b");
    const source = setRightPanel({}, a, "git");
    expect(adoptRightPanel(source, "local", "a", "b")).toEqual({ [rightPanelKey(b)]: "git" });
    expect(adoptRightPanel(setRightPanel(source, b, "closed"), "local", "a", "b")).toEqual({ [rightPanelKey(b)]: "closed" });
    expect(adoptRightPanel({}, "local", "a", "b")).toEqual({});
  });
  it("prunes only an authoritative list, preserves other Hosts, in-process Tabs and pending opens", () => {
    const a = tab("a"), b = tab("b"), remote = tab("r", "other"), local = { ...tab("c"), terminal: undefined };
    const panels = ensureRightPanels({}, [a, b, remote, local], true);
    expect(pruneRightPanels(panels, "local", null)).toBe(panels);
    expect(pruneRightPanels(panels, "local", undefined)).toBe(panels);
    expect(pruneRightPanels(panels, "local", ["a"], new Set(["b"]))).toBe(panels);
    const removed = pruneRightPanels(panels, "local", ["a"]);
    expect(removed).toEqual({ [rightPanelKey(a)]: "files", [rightPanelKey(remote)]: "files", [rightPanelKey(local)]: "files" });
    expect(removeRightPanelHost(removed, "other")).toEqual({ [rightPanelKey(a)]: "files", [rightPanelKey(local)]: "files" });
  });
});
