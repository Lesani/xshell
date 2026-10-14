import { describe, expect, it } from "vitest";
import { baseName, hostSep, isInside, isLocalName, isTransferPart, joinPath, parentOf, pathStyle, styleOfOs } from "./paths";

describe("paths", () => {
  it("keeps the Host's separator", () => {
    expect(hostSep("/home/u")).toBe("/");
    expect(hostSep("C:\\Users\\u")).toBe("\\");
    expect(joinPath("/d", "a", "b")).toBe("/d/a/b");
    expect(joinPath("/", "etc")).toBe("/etc");
    expect(joinPath("C:\\", "Users")).toBe("C:\\Users");
  });

  it("baseName on POSIX and Windows paths", () => {
    expect(baseName("/srv/a.txt")).toBe("a.txt");
    expect(baseName("/srv/dir/")).toBe("dir");
    expect(baseName("C:\\Users\\u\\a.txt")).toBe("a.txt");
    expect(baseName("C:\\")).toBe("C:");
  });

  it("parentOf on POSIX and Windows paths", () => {
    expect(parentOf("/srv/a.txt")).toBe("/srv");
    expect(parentOf("/srv")).toBe("/");
    expect(parentOf("C:\\Users")).toBe("C:\\");
    expect(parentOf("C:\\Users\\u\\a.txt")).toBe("C:\\Users\\u");
    expect(parentOf("C:")).toBeNull();
  });

  it("recognizes upload part files only", () => {
    expect(isTransferPart(".notes.md.xshell-part-0123456789ab")).toBe(true);
    expect(isTransferPart("notes.md")).toBe(false);
    expect(isTransferPart(".xshell-part-notes")).toBe(false);
    expect(isTransferPart(".a.xshell-part-0123")).toBe(false);
  });
});

describe("paths by Host style", () => {
  it("POSIX paths split on / only; a backslash is part of the name", () => {
    const p = "/srv/a\\b.txt";
    expect(pathStyle(p)).toBe("posix");
    expect(baseName(p)).toBe("a\\b.txt");
    expect(parentOf(p)).toBe("/srv");
    expect(joinPath(parentOf(p)!, baseName(p))).toBe(p);
    expect(baseName("/srv/dir\\")).toBe("dir\\");
  });

  it("drive letters and UNC paths are Windows", () => {
    expect(pathStyle("C:\\Users")).toBe("windows");
    expect(pathStyle("c:/x")).toBe("windows");
    expect(pathStyle("\\\\server\\share\\a.txt")).toBe("windows");
    expect(baseName("\\\\server\\share\\a.txt")).toBe("a.txt");
    expect(baseName("C:/Users/u/a.txt")).toBe("a.txt");
  });

  it("a known style wins over the path's form", () => {
    expect(baseName("C:\\x\\y", "posix")).toBe("C:\\x\\y");
    expect(parentOf("/a/b\\c", "windows")).toBe("/a/b");
  });
});

describe("Windows Remote Host paths", () => {
  it("C:\\Users\\me\\a.txt round-trips", () => {
    const p = "C:\\Users\\me\\a.txt";
    expect(baseName(p)).toBe("a.txt");
    expect(parentOf(p)).toBe("C:\\Users\\me");
    expect(joinPath(parentOf(p)!, baseName(p))).toBe(p);
    expect(parentOf("C:\\a.txt")).toBe("C:\\");
  });

  it("maps a reported OS to path rules", () => {
    expect(styleOfOs("windows")).toBe("windows");
    expect(styleOfOs("linux")).toBe("posix");
    expect(styleOfOs("macos")).toBe("posix");
    expect(styleOfOs(null)).toBeUndefined();
  });
});

describe("local names", () => {
  it("POSIX refuses only traversal, NUL and /", () => {
    for (const ok of ["a", "a\\b", "CON", "x.", "a:b"]) expect(isLocalName(ok, "posix")).toBe(true);
    for (const bad of ["", ".", "..", "a/b", "a\0b"]) expect(isLocalName(bad, "posix")).toBe(false);
  });

  it("Windows also refuses its reserved characters and names", () => {
    expect(isLocalName("notes.txt", "windows")).toBe(true);
    for (const bad of ["..\\notes.txt", "a\\b", "a:b", "a*b", "a?b", "a\"b", "a<b", "a>b", "a|b", "a\u0001b", "x.", "x ", "CON", "nul.txt", "COM1", "lpt9.log", ".."]) {
      expect(isLocalName(bad, "windows")).toBe(false);
    }
  });

  it("isInside checks containment by the destination's rules", () => {
    expect(isInside("C:\\Downloads\\src", "C:\\Downloads\\src\\a.txt", "windows")).toBe(true);
    expect(isInside("C:\\Downloads\\src", "C:\\Downloads\\src\\..\\notes.txt", "windows")).toBe(false);
    expect(isInside("C:\\Downloads\\src", "C:\\Downloads\\srcx\\a", "windows")).toBe(false);
    expect(isInside("/d/src", "/d/src/a/b", "posix")).toBe(true);
    expect(isInside("/d/src", "/d/src/../x", "posix")).toBe(false);
    expect(isInside("/d/src", "/d/src", "posix")).toBe(false);
  });
});
