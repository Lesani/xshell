import { describe, expect, it } from "vitest";
import { terminalHostState, terminalHostStateText, terminalOpenFailedText } from "./terminalState";
import type { HostStatus } from "./types";

const st = (status: HostStatus["status"]) => ({ status } as HostStatus);

describe("terminal Host state", () => {
  it("is null while usable, otherwise waiting/reconnecting/offline/incompatible", () => {
    expect(terminalHostState(st("connected"), [], true)).toBeNull();
    expect(terminalHostState(st("upgrade-pending"), [], true)).toBeNull();
    expect(terminalHostState(undefined, null, false)).toBe("waiting");
    expect(terminalHostState(st("reconnecting"), [], false)).toBe("waiting");
    expect(terminalHostState(st("reconnecting"), [], true)).toBe("reconnecting");
    expect(terminalHostState(st("offline"), [], true)).toBe("offline");
    expect(terminalHostState(st("incompatible"), [], true)).toBe("incompatible");
  });

  it("Remote wording names the Host; local wording never does", () => {
    expect(terminalHostStateText("waiting", false, "Dev", null)).toBe("Waiting for Dev…");
    expect(terminalHostStateText(null, true, "", null)).toBeNull();
    expect(terminalHostStateText("waiting", true, "", null)).toBe("Starting local terminals…");
    expect(terminalHostStateText("reconnecting", true, "", null)).toBe("Reconnecting to local terminals… Input is paused.");
    expect(terminalHostStateText("offline", true, "", "xshelld serve exited with 1")).toBe("Local terminals are unavailable: xshelld serve exited with 1. Input is paused; xshell will keep retrying.");
    expect(terminalHostStateText("incompatible", true, "", "x")).toBe("Local terminals are unavailable: x. Input is paused; xshell will keep retrying.");
    expect(terminalOpenFailedText(true, "", "no such dir")).toBe("Couldn't start a terminal: no such dir");
    expect(terminalOpenFailedText(false, "Dev", "e")).toBe("Couldn't start a terminal on Dev: e");
  });
});
