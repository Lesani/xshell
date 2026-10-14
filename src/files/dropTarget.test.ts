import { describe, expect, it } from "vitest";
import { DRAG_PATH_MIME, acceptsDrop, resolveDropTarget, rowData } from "./dropTarget";

// A stand-in for an element inside a row: `closest` finds the row's attributes.
const inRow = (attrs: Record<string, string> | null) => ({
  closest: (sel: string) => (sel === "[data-path]" && attrs ? { getAttribute: (n: string) => attrs[n] ?? null } : null),
});

describe("resolveDropTarget", () => {
  it("a folder row is the target", () => {
    expect(resolveDropTarget(rowData(inRow({ "data-path": "/p/src", "data-dir": "1" })), "/p")).toBe("/p/src");
  });

  it("a file row drops into the current folder", () => {
    expect(resolveDropTarget(rowData(inRow({ "data-path": "/p/a.txt", "data-dir": "0" })), "/p")).toBe("/p");
  });

  it("empty space drops into the current folder", () => {
    expect(rowData(inRow(null))).toBeNull();
    expect(rowData(null)).toBeNull();
    expect(resolveDropTarget(null, "C:\\work")).toBe("C:\\work");
  });
});

describe("acceptsDrop", () => {
  it("accepts files from the OS", () => {
    expect(acceptsDrop(["Files"], false)).toBe(true);
    expect(acceptsDrop(["text/uri-list", "Files"], false)).toBe(true);
  });

  it("rejects the in-app path drag and other drags", () => {
    expect(acceptsDrop([DRAG_PATH_MIME, "text/plain"], false)).toBe(false);
    expect(acceptsDrop([DRAG_PATH_MIME, "Files"], false)).toBe(false);
    expect(acceptsDrop(["text/plain"], false)).toBe(false);
    expect(acceptsDrop(undefined, false)).toBe(false);
  });

  it("rejects everything while this window's drag-out runs", () => {
    expect(acceptsDrop(["Files"], true)).toBe(false);
  });
});
