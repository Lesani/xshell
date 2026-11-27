import { describe, expect, it } from "vitest";
import { localView, logPath, type LocalStatus } from "./localInfo";

const daemon = (state: "starting" | "ready" | "failed", over: Partial<LocalStatus> = {}): LocalStatus => ({
  mode: "daemon", reason: null, daemon: { state, error: null, errorKind: null, log: "/h/.xshell/log/xshelld.log" }, service: null, ...over,
});

describe("local readiness (U5)", () => {
  it("starting → failed → ready in mode daemon", () => {
    expect(localView(daemon("starting"))).toBe("ok");
    expect(localView(daemon("failed"))).toBe("failed");
    expect(localView(daemon("ready"))).toBe("ready");
  });
  it("mode failed is failed; in-process and unknown are neither", () => {
    expect(localView({ mode: "failed", reason: "home unknown", daemon: null, service: null })).toBe("failed");
    expect(localView({ mode: "in-process", reason: "test", daemon: null, service: null })).toBe("ok");
    expect(localView(null)).toBe("ok");
  });
  it("names the Daemon's log, or its default place without a Daemon", () => {
    expect(logPath(daemon("failed"))).toBe("/h/.xshell/log/xshelld.log");
    expect(logPath({ mode: "failed", reason: null, daemon: null })).toBe("~/.xshell/log/xshelld.log");
  });
});
