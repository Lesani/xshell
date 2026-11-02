import { describe, expect, it } from "vitest";
import type { Tab } from "../types";
import { effectiveTabName, tabName } from "./tabName";
const tab: Tab = { id: "t", type: "terminal", title: "Automatic", projectName: "Project" };
describe("Tab name presentation", () => {
  it("custom_name_wins_without_replacing_automatic_title", () => {
    const custom = { ...tab, customTitle: "Shared" };
    expect(tabName(custom)).toEqual({ primary: "Shared", secondary: "Project", tooltip: "Shared — Project" });
    expect(effectiveTabName(custom)).toBe("Shared");
    expect(custom.title).toBe("Automatic");
  });
  it("clear_exposes_latest_automatic_title", () => {
    expect(effectiveTabName({ ...tab, title: "Latest", customTitle: undefined })).toBe("Latest");
  });
  it("raw_shell_custom_name_wins_and_absent_override_keeps_layout", () => {
    const shell: Tab = { ...tab, shellMode: "raw", title: "Bash" };
    expect(tabName(shell)).toEqual({ primary: "Project", secondary: "Bash", tooltip: "Project — Bash" });
    expect(tabName({ ...shell, customTitle: "Build" }, "Personal Project")).toEqual({ primary: "Build", secondary: "Personal Project · Bash", tooltip: "Build — Personal Project · Bash" });
    expect(tabName({ ...shell, projectName: undefined }).primary).toBe("Bash");
  });
});
