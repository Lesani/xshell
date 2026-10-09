import { describe, expect, it } from "vitest";
import { defaultShellForPlatform, shellsForPlatform } from "./shells";

describe("shells", () => {
  it("unix platforms get no windows-only presets", () => {
    for (const p of ["linux", "macos"] as const) {
      const ids = shellsForPlatform(p).map(s => s.id);
      expect(ids).not.toContain("powershell");
      expect(ids).not.toContain("cmd");
      expect(ids).not.toContain("gitbash");
      expect(ids).toContain("bash");
    }
  });

  it("windows keeps its presets", () => {
    expect(shellsForPlatform("windows").map(s => s.id)).toEqual(["powershell", "pwsh", "cmd", "gitbash"]);
  });

  it("the macos default is zsh, otherwise bash (windows: powershell)", () => {
    expect(defaultShellForPlatform("macos")).toBe("zsh");
    expect(defaultShellForPlatform("linux")).toBe("bash");
    expect(defaultShellForPlatform("windows")).toBe("powershell");
  });
});
