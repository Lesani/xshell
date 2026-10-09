import { describe, expect, it } from "vitest";
import { daemonHost, isLocalHost, LOCAL_HOST, tabHostOf } from "./localHost";

describe("localHost", () => {
  it("daemonHost: the wire Host of a Daemon Tab, null for an in-process Tab", () => {
    expect(daemonHost({ terminal: "u" })).toBe(LOCAL_HOST);
    expect(daemonHost({ terminal: "u", host: "h_ab12cd34" })).toBe("h_ab12cd34");
    expect(daemonHost({})).toBeNull();
    // A host without a Terminal is not a Daemon Tab (never produced, guarded anyway).
    expect(daemonHost({ host: "h_ab12cd34" })).toBeNull();
  });

  it("tabHostOf: \"local\" is the Tab host undefined", () => {
    expect(tabHostOf("local")).toBeUndefined();
    expect(tabHostOf("h_ab12cd34")).toBe("h_ab12cd34");
    expect(isLocalHost("local")).toBe(true);
    expect(isLocalHost(undefined)).toBe(false);
  });
});
