import { describe, expect, it } from "vitest";
import { formatBuild, sameBuild, versionWithBuild, type BuildId } from "./buildId";

const clean: BuildId = { commit: "85ee920a1b2c", dirty: null, time: 1760000000 };
const dirty: BuildId = { commit: "85ee920a1b2c", dirty: "3f9a01c2b4e5", time: 1760000000 };

describe("buildId", () => {
  it("formats like the Rust display form", () => {
    expect(formatBuild(clean)).toBe("85ee920a1b2c");
    expect(formatBuild(dirty)).toBe("85ee920a1b2c+d.3f9a01c2b4e5");
  });

  it("compares commit and digest, not time", () => {
    expect(sameBuild(clean, { ...clean, time: 1 })).toBe(true);
    expect(sameBuild(clean, dirty)).toBe(false);
    expect(sameBuild(clean, { ...clean, commit: "000000000000" })).toBe(false);
    expect(sameBuild(null, null)).toBe(false);
    expect(sameBuild(clean, undefined)).toBe(false);
  });

  it("appends the build to the version", () => {
    expect(versionWithBuild("1.5.0", clean)).toBe("1.5.0 (85ee920a1b2c)");
    expect(versionWithBuild("1.5.0", dirty)).toBe("1.5.0 (85ee920a1b2c+d.3f9a01c2b4e5)");
    expect(versionWithBuild("1.5.0", null)).toBe("1.5.0 (unknown build)");
  });
});
