import { describe, expect, it } from "vitest";
import { startView, type StartInputs } from "./firstLaunch";

const base = (over: Partial<StartInputs> = {}, status: Partial<NonNullable<StartInputs["status"]>> = {}): StartInputs => ({
  status: { membership: "none", removed: null, oldRing: null, ...status },
  local: "ok", overview: { unavailable: null }, skipped: false, oldSeenAt: null, ...over,
});

describe("start view", () => {
  it("none + not skipped → first", () => expect(startView(base())).toBe("first"));
  it("skipped → null", () => expect(startView(base({ skipped: true }))).toBeNull());
  it("removed → removed even when skipped", () => {
    expect(startView(base({ skipped: true }, { membership: "removed", removed: { ringId: "r", at: 5 } }))).toBe("removed");
  });
  it("oldRing unseen → old; seen at the same at → null", () => {
    expect(startView(base({ skipped: true }, { oldRing: { at: 7 } }))).toBe("old");
    expect(startView(base({ skipped: true, oldSeenAt: 7 }, { oldRing: { at: 7 } }))).toBeNull();
    expect(startView(base({ skipped: true, oldSeenAt: 6 }, { oldRing: { at: 7 } }))).toBe("old");
  });
  it("member/finishing/failed local/unloaded/platform → null", () => {
    expect(startView(base({}, { membership: "member" }))).toBeNull();
    expect(startView(base({}, { membership: "finishing" }))).toBeNull();
    expect(startView(base({ local: "failed" }))).toBeNull();
    expect(startView({ ...base(), status: null })).toBeNull();
    expect(startView(base({ overview: { unavailable: "platform" } }))).toBeNull();
  });
});
