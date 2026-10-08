import { describe, expect, it } from "vitest";
import { latestGate } from "./requestGate";
import { toProjectKey } from "./projectKey";

// Amendment 23: switching between two Hosts with identical paths must never show the
// previous Host's data. The gate is what SkillsPanel and the project page use.
describe("host-qualified component identity", () => {
  it("a slow response for the previous host is dropped when the path is identical", async () => {
    const gate = latestGate();
    const results: string[] = [];
    const fetchFor = (host: string, delay: number) => {
      const token = gate.begin(toProjectKey(host, "/home/u/app"));
      return new Promise<void>(r => setTimeout(() => { if (gate.isCurrent(token)) results.push(host); r(); }, delay));
    };
    const a = fetchFor("h_aaaaaaaa", 20); // slow, superseded
    const b = fetchFor("h_bbbbbbbb", 1);
    await Promise.all([a, b]);
    expect(results).toEqual(["h_bbbbbbbb"]);
    expect(gate.currentKey()).toBe("host:h_bbbbbbbb:/home/u/app");
  });

  it("identical paths on two hosts (and local) are distinct keys", () => {
    expect(new Set([toProjectKey(undefined, "/p"), toProjectKey("h_aaaaaaaa", "/p"), toProjectKey("h_bbbbbbbb", "/p")]).size).toBe(3);
  });
});
