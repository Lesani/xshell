import { describe, expect, it } from "vitest";
import {
  collectLeafIds,
  countLeaves,
  hasLeaf,
  insertLeaf,
  removeLeaf,
  setRatioAt,
} from "./layout";
import type { LayoutNode } from "./types";

const leaf = (tabId: string): LayoutNode => ({ kind: "leaf", tabId });
const split = (
  a: LayoutNode,
  b: LayoutNode,
  direction: "col" | "row" = "col",
  ratio = 0.5,
): LayoutNode => ({ kind: "split", direction, children: [a, b], ratio });

// split(a, split(b, c))
const tree = (): LayoutNode => split(leaf("a"), split(leaf("b"), leaf("c"), "row", 0.4), "col", 0.6);

describe("layout", () => {
  it("countLeaves counts every leaf in a nested split", () => {
    expect(countLeaves(tree())).toBe(3);
    expect(countLeaves(leaf("a"))).toBe(1);
  });

  it("collectLeafIds returns ids left-to-right", () => {
    expect(collectLeafIds(tree())).toEqual(["a", "b", "c"]);
  });

  it("hasLeaf finds present and rejects absent ids", () => {
    expect(hasLeaf(tree(), "c")).toBe(true);
    expect(hasLeaf(tree(), "z")).toBe(false);
  });

  it("insertLeaf right/bottom places the new leaf second", () => {
    const target = leaf("t");
    expect(insertLeaf(target, "t", "n", "right")).toEqual({
      kind: "split",
      direction: "col",
      children: [target, leaf("n")],
      ratio: 0.5,
    });
    const bottom = insertLeaf(target, "t", "n", "bottom");
    expect(bottom).toEqual({
      kind: "split",
      direction: "row",
      children: [target, leaf("n")],
      ratio: 0.5,
    });
  });

  it("insertLeaf left/top places the new leaf first", () => {
    for (const zone of ["left", "top"] as const) {
      const result = insertLeaf(leaf("t"), "t", "n", zone);
      expect(result.kind).toBe("split");
      if (result.kind !== "split") continue;
      expect(result.children[0]).toEqual(leaf("n"));
      expect(result.children[1]).toEqual(leaf("t"));
      expect(result.direction).toBe(zone === "left" ? "col" : "row");
    }
  });

  it("insertLeaf leaves non-matching leaves untouched and does not mutate input", () => {
    const root = tree();
    const before = structuredClone(root);
    const result = insertLeaf(root, "b", "n", "right");
    expect(root).toEqual(before);
    if (root.kind !== "split" || result.kind !== "split") throw new Error("expected splits");
    // The untouched leaf "a" keeps its identity.
    expect(result.children[0]).toBe(root.children[0]);
    expect(collectLeafIds(result)).toEqual(["a", "b", "n", "c"]);
  });

  it("setRatioAt updates the split at a path", () => {
    const root = tree();
    const atRoot = setRatioAt(root, [], 0.3);
    expect(atRoot.kind === "split" && atRoot.ratio).toBe(0.3);

    const nested = setRatioAt(root, [1], 0.7);
    if (nested.kind !== "split") throw new Error("expected split");
    const right = nested.children[1];
    expect(right.kind === "split" && right.ratio).toBe(0.7);
    expect(nested.ratio).toBe(0.6);
  });

  it("setRatioAt returns input unchanged for a path ending at a leaf or an invalid index", () => {
    // Immediate returns keep reference identity.
    const single = leaf("a");
    expect(setRatioAt(single, [], 0.9)).toBe(single);
    expect(setRatioAt(single, [0], 0.9)).toBe(single);
    const root = tree();
    expect(setRatioAt(root, [2], 0.9)).toBe(root);
    // Nested no-ops rebuild the parents, so only deep equality holds.
    expect(setRatioAt(root, [0], 0.9)).toEqual(root);
    expect(setRatioAt(root, [1, 0], 0.9)).toEqual(root);
    expect(setRatioAt(root, [1, 5], 0.9)).toEqual(root);
  });

  it("removeLeaf collapses the parent split to the sibling", () => {
    expect(removeLeaf(split(leaf("a"), leaf("b")), "b")).toEqual(leaf("a"));
    const result = removeLeaf(tree(), "b");
    expect(result && collectLeafIds(result)).toEqual(["a", "c"]);
  });

  it("removeLeaf returns null when the tree becomes empty", () => {
    expect(removeLeaf(leaf("a"), "a")).toBeNull();
  });
});
