import { expect, it } from "vitest";
import type { Group, Tab } from "../types";
import { tabBarEntries, normalizeProjectTabs, projectDrop, projectDropGap, flattenEntries, groupTabsSetting } from "./projectOrder";
const tab = (id: string, projectPath?: string, extra: Partial<Tab> = {}): Tab => ({ id, type: "terminal", title: id, projectPath, ...extra });
const group = (id: string, a: string, b: string): Group => ({ id, name: id, layout: { kind: "split", direction: "col", ratio: .5, children: [{ kind: "leaf", tabId: a }, { kind: "leaf", tabId: b }] } });
const ids = (tabs: Tab[]) => tabs.map(t => t.id);
it("strict boolean default-on for missing, old/new and malformed settings", () => {
  for (const value of [undefined, null, 0, 1, "false", {}, true]) expect(groupTabsSetting(value)).toBe(true);
  expect(groupTabsSetting(false)).toBe(false);
});
it("normalizes actual stable first-occurrence blocks, preserving Host and path case and idempotence", () => {
  const tabs = [tab("a", "/p"), tab("b", "/q"), tab("c", "/p"), tab("host", "/p", { host: "h_a" }), tab("case", "/P"), tab("d", "/q"), tab("unknown"), tab("new", "/p")];
  const result = normalizeProjectTabs(tabs, []);
  expect(ids(result)).toEqual(["a", "c", "new", "b", "d", "host", "case", "unknown"]);
  expect(normalizeProjectTabs(result, [])).toBe(result);
  expect(result.find(t => t.id === "a")).toBe(tabs[0]);
});
it("single-Project Groups are indivisible, mixed Groups and unknowns opaque; dissolve/merge/removal recomputes", () => {
  const tabs = [tab("a", "/p"), tab("x", "/q", { groupId: "mix" }), tab("y", "/p", { groupId: "mix" }), tab("b", "/p", { groupId: "pure" }), tab("c", "/p", { groupId: "pure" }), tab("u"), tab("d", "/q"), tab("orphan", undefined, { groupId: "gone" })];
  const groups = [group("mix", "x", "y"), group("pure", "b", "c")];
  const result = normalizeProjectTabs(tabs, groups);
  expect(ids(result)).toEqual(["a", "b", "c", "x", "y", "u", "d", "orphan"]);
  expect(tabBarEntries(result, groups).map(e => e.id)).toEqual(["a", "pure", "mix", "u", "d", "orphan"]);
  expect(ids(normalizeProjectTabs(tabs.map(t => ({ ...t, groupId: undefined })), []))).toEqual(["a", "y", "b", "c", "x", "d", "u", "orphan"]);
  expect(ids(normalizeProjectTabs(result.filter(t => t.id !== "a"), groups))).toEqual(["b", "c", "x", "y", "u", "d", "orphan"]);
});
it("gap semantics: same-block entry, cross-block whole move before/after, source-removal boundaries", () => {
  const tabs = [tab("a", "/p"), tab("b", "/p"), tab("c", "/q"), tab("d", "/q"), tab("e", "/r")];
  const entries = tabBarEntries(tabs, []);
  const drop = (sourceIndex: number, destinationGap: number, targetIndex: number) => projectDrop(entries, tabs, { sourceIndex, destinationGap, targetIndex }).map(e => e.id);
  expect(drop(0, 2, 1)).toEqual(["b", "a", "c", "d", "e"]);
  expect(drop(0, 3, 2)).toEqual(["c", "d", "a", "b", "e"]);
  expect(drop(4, 1, 1)).toEqual(["e", "a", "b", "c", "d"]);
  expect(drop(0, 2, 2)).toEqual(["a", "b", "c", "d", "e"]);
  expect(drop(2, 1, 0)).toEqual(["a", "b", "c", "d", "e"]);
  expect(projectDropGap(entries, tabs, { sourceIndex: 0, destinationGap: 3, targetIndex: 2 })).toBe(4);
  expect(projectDropGap(entries, tabs, { sourceIndex: 3, destinationGap: 1, targetIndex: 1 })).toBe(0);
  expect(projectDropGap(entries, tabs, { sourceIndex: 0, destinationGap: 2, targetIndex: 1 })).toBe(2);
  expect(flattenEntries(entries, tabs)).toEqual(tabs);
});
