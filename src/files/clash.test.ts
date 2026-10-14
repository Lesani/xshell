import { describe, expect, it } from "vitest";
import { clashResolver, uploadSummary, type ClashAnswer } from "./clash";

describe("clashResolver", () => {
  it("asks for each clash and returns the answer", async () => {
    const asked: string[] = [];
    const answers: ClashAnswer[] = [{ choice: "replace", applyAll: false }, { choice: "keep_both", applyAll: false }, { choice: "skip", applyAll: false }];
    const r = clashResolver(async (p) => { asked.push(p); return answers[asked.length - 1]; });
    expect([await r("/d/a"), await r("/d/b"), await r("/d/c")]).toEqual(["replace", "keep_both", "skip"]);
    expect(asked).toEqual(["/d/a", "/d/b", "/d/c"]);
  });

  it("apply to all answers later clashes without asking", async () => {
    let asks = 0;
    const r = clashResolver(async () => { asks++; return { choice: "keep_both", applyAll: true }; });
    expect([await r("/d/a"), await r("/d/b"), await r("/d/c")]).toEqual(["keep_both", "keep_both", "keep_both"]);
    expect(asks).toBe(1);
  });

  it("the memory is per batch", async () => {
    let asks = 0;
    const ask = async (): Promise<ClashAnswer> => { asks++; return { choice: "skip", applyAll: true }; };
    const first = clashResolver(ask);
    await first("/d/a");
    await first("/d/b");
    const second = clashResolver(ask);
    await second("/d/a");
    expect(asks).toBe(2);
  });
});

describe("uploadSummary", () => {
  const t = { path: "/d/a", size: 1, version: "sha256:00" };
  it("all uploaded: done", () => {
    expect(uploadSummary({ uploaded: [t, t], skipped: [], failed: [] }, "d")).toEqual({ key: "explorer.drop.done", params: { n: 2, folder: "d" }, tone: "info" });
  });
  it("skips or failures: partial", () => {
    expect(uploadSummary({ uploaded: [t], skipped: ["/d/b"], failed: [{ path: "/d/c", error: "x" }] }, "d"))
      .toEqual({ key: "explorer.drop.partial", params: { n: 1, skipped: 1, failed: 1 }, tone: "warn" });
  });
});
