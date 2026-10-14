import { describe, expect, it } from "vitest";
import { NOTICE_MAX, fitNotice } from "./noticeText";
import type { StringKey } from "../hosts/strings";

const NOTICES: StringKey[] = [
  "explorer.open.downloading", "explorer.open.failed", "explorer.sync.uploading", "explorer.sync.uploaded",
  "explorer.sync.waiting", "explorer.sync.failed", "explorer.sync.conflict", "explorer.sync.restored",
  "explorer.sync.checking", "explorer.sync.missing", "explorer.sync.unknownOutcome",
];

describe("fitNotice", () => {
  const long = "x".repeat(300);
  const params = { name: `report-${long}.txt`, host: `build-server-${long}`, error: `Permission denied ${long}`, pct: 100, n: 12345 };

  it.each(NOTICES)("%s stays within the budget with huge values and keeps the full text", key => {
    const { text, full } = fitNotice(key, params);
    expect(text.length).toBeLessThanOrEqual(NOTICE_MAX);
    expect(full.length).toBeGreaterThan(NOTICE_MAX);
    expect(text).not.toMatch(/\{\w+\}/);
  });

  it("short values are not touched", () => {
    const { text, full } = fitNotice("explorer.sync.uploaded", { name: "a.txt", host: "dev" });
    expect(text).toBe("Uploaded changes to a.txt on dev.");
    expect(full).toBe(text);
  });

  it("a short value leaves its share to a long one", () => {
    const { text } = fitNotice("explorer.sync.uploaded", { name: "y".repeat(500), host: "dev" });
    expect(text.length).toBe(NOTICE_MAX);
    expect(text.endsWith("… on dev.")).toBe(true);
  });
});
