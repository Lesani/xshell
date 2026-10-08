import { describe, expect, it } from "vitest";
import {
  asProjectKey, encodedNameFor, isPinned, keyOf, keyOfTab, lookupKey, newHostId, parseProjectKey,
  sameKey, sessionKey, sessionKeyOf, sessionKeyOfTab, toProjectKey, HOST_ID_RE,
} from "./projectKey";

describe("projectKey", () => {
  it("local key is the bare path", () => {
    expect(toProjectKey(undefined, "C:\\a\\b")).toBe("C:\\a\\b");
    expect(toProjectKey(undefined, "/home/u/p")).toBe("/home/u/p");
    expect(keyOf({ path: "/x" })).toBe("/x");
  });

  it("remote key format", () => {
    expect(toProjectKey("h_ab12cd34", "/home/u/p")).toBe("host:h_ab12cd34:/home/u/p");
  });

  it("parse round-trips, including colons and spaces", () => {
    const k = toProjectKey("h_ab12cd34", "/a:b c/d");
    expect(parseProjectKey(k)).toEqual({ host: "h_ab12cd34", path: "/a:b c/d" });
  });

  it("parse of local paths is local", () => {
    for (const p of ["C:\\x", "\\\\srv\\s", "/x"]) {
      const parsed = parseProjectKey(p);
      expect(parsed.host).toBeUndefined();
      expect(parsed.path).toBe(p);
    }
  });

  it("malformed prefix is local", () => {
    expect(parseProjectKey("host:BAD ID:/x")).toEqual({ path: "host:BAD ID:/x" });
    expect(parseProjectKey("host:h_short:/x")).toEqual({ path: "host:h_short:/x" });
  });

  it("sameKey/lookupKey case-insensitive, host id preserved", () => {
    const k = toProjectKey("h_ab12cd34", "/Home/U/P");
    expect(sameKey(k, "host:h_ab12cd34:/home/u/p")).toBe(true);
    expect(sameKey("C:\\A", "c:\\a")).toBe(true);
    expect(lookupKey(k)).toBe("host:h_ab12cd34:/home/u/p");
    expect(parseProjectKey(lookupKey(k)).host).toBe("h_ab12cd34");
    // the same path on another Host is another project
    expect(sameKey(toProjectKey("h_ab12cd34", "/p"), toProjectKey("h_zz12cd34", "/p"))).toBe(false);
    expect(sameKey(toProjectKey("h_ab12cd34", "/p"), "/p")).toBe(false);
  });

  it("encodedNameFor uses raw path, never the key", () => {
    expect(encodedNameFor({ path: "/home/u/p" })).toBe("-home-u-p");
    expect(encodedNameFor({ encoded_name: "", path: "C:\\a b" })).toBe("C--a-b");
    expect(encodedNameFor({ encoded_name: "rec", path: "/x" })).toBe("rec");
    const remote = { host: "h_ab12cd34", path: "/home/u/p" };
    expect(encodedNameFor(remote)).toBe("-home-u-p");
    expect(encodedNameFor(remote)).not.toContain("h_ab12cd34");
  });

  it("keyOfTab uses the tab's host", () => {
    expect(keyOfTab({ projectPath: "/p" })).toBe("/p");
    expect(keyOfTab({ host: "h_ab12cd34", projectPath: "/p" })).toBe("host:h_ab12cd34:/p");
    expect(keyOfTab({})).toBeNull();
    expect(asProjectKey("/stored")).toBe("/stored");
  });

  it("generated host ids match the single-sourced pattern", () => {
    for (let i = 0; i < 50; i++) expect(HOST_ID_RE.test(newHostId())).toBe(true);
  });

  it("session keys are host-qualified only for remote sessions", () => {
    expect(sessionKey(undefined, "s1")).toBe("s1");
    expect(sessionKeyOf({ host: "h_ab12cd34", id: "s1" })).toBe("h_ab12cd34:s1");
    expect(sessionKeyOf({ id: "s1" })).not.toBe(sessionKeyOf({ host: "h_ab12cd34", id: "s1" }));
    expect(sessionKeyOfTab({ sessionId: "s1" })).toBe("s1");
    expect(sessionKeyOfTab({ host: "h_ab12cd34" })).toBeNull();
  });
});

describe("picker checked state (amendment 25)", () => {
  it("local comparison is today's normalizePath: separators, trailing slashes, case", () => {
    expect(isPinned(["C:/Users/a/proj/"], undefined, "c:\\users\\a\\proj")).toBe(true);
    expect(isPinned(["C:\\Users\\a\\proj"], undefined, "C:/Users/a/proj//")).toBe(true);
    expect(isPinned(["/home/u/p/"], undefined, "/home/u/p")).toBe(true);
    expect(isPinned(["/home/u/p"], undefined, "/home/u/q")).toBe(false);
  });

  it("remote comparison normalizes within the host only", () => {
    const saved = [toProjectKey("h_ab12cd34", "/home/u/p/")];
    expect(isPinned(saved, "h_ab12cd34", "/home/u/p")).toBe(true);
    expect(isPinned(saved, "h_zz12cd34", "/home/u/p")).toBe(false);
    expect(isPinned(saved, undefined, "/home/u/p")).toBe(false);
    expect(isPinned(["/home/u/p"], "h_ab12cd34", "/home/u/p")).toBe(false);
  });
});
