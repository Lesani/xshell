import { describe, expect, it } from "vitest";
import { C, fmtc, plural } from "./copy";

describe("Ring copy", () => {
  it("plural picks .one only for 1", () => {
    expect(plural("quit.title", 1)).toBe("Keep 1 terminal running?");
    expect(plural("quit.title", 0)).toBe("Keep 0 terminals running?");
    expect(plural("quit.title", 2)).toBe("Keep 2 terminals running?");
    expect(plural("quit.stop", 1)).toBe("Stop it");
    expect(plural("quit.stop", 3)).toBe("Stop them");
  });

  it("fills placeholders and leaves unknown ones", () => {
    expect(fmtc("add.done", { name: "laptop" })).toBe("Added laptop to your Ring.");
    expect(fmtc("join.countdown", { m: 9, ss: "05" })).toBe("Expires in 9:05");
    expect(fmtc("add.done")).toBe("Added {name} to your Ring.");
    expect(fmtc("member.removeLabel", { other: "x" })).toBe("Remove {name}");
  });

  it("no copy says session (the glossary's word for an agent's history)", () => {
    for (const [k, v] of Object.entries(C)) expect(v.toLowerCase(), k).not.toMatch(/\bsessions?\b/);
  });

  it("carries the binding text verbatim", () => {
    expect(C["first.title"]).toBe("Connect your machines");
    expect(C["create.done"]).toBe("Your Ring is ready. You can now add other machines and your phone.");
    expect(C["removed.title"]).toBe("This machine was removed from your Ring");
    expect(C["ring.follower"]).toBe("This window shows your Ring. Make changes in the first xshell window on this machine.");
    expect(C["local.older.title"]).toBe("Close the other xshell window first");
  });
});
