import { expect, it, vi } from "vitest";
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({ listen: async () => () => {} }));
vi.mock("@tauri-apps/plugin-store", () => ({ load: async () => ({ get: async () => null, set: async () => {}, save: async () => {} }) }));
import { moveProjectKey, moveSessionFolderId, moveTerminalKeys, ringMoveTarget } from "./sshRing";
import type { AvailableHost, HostConfig } from "./types";

// Moving a saved SSH Host to the Ring list (#119): when it may move, and how its keys follow.
const H = "h_abcdefgh", R = `r_${"a".repeat(64)}`;
const host: HostConfig = { id: H, name: "box", sshTarget: "dev" };
const row = (over: Partial<AvailableHost> = {}): AvailableHost => ({ id: H, aliases: [H, R], name: "box", color: null, daemonKey: "key", routes: ["ssh", "relay"], activeRoute: "ssh", reachability: { kind: "online" }, ...over });

it("a Host moves only as a member the Relay reports online, with nothing only SSH can keep", () => {
  expect(ringMoveTarget(host, row())).toBe(R);
  expect(ringMoveTarget(undefined, row())).toBeNull();
  expect(ringMoveTarget(host, undefined)).toBeNull();
  for (const kind of ["unknown", "never-connected"] as const) expect(ringMoveTarget(host, row({ reachability: { kind } }))).toBeNull();
  expect(ringMoveTarget(host, row({ reachability: { kind: "unreachable", at: 1 } }))).toBeNull();
  expect(ringMoveTarget(host, row({ reachability: { kind: "closed", reason: "app-closed", at: 1 } }))).toBeNull();
  expect(ringMoveTarget(host, row({ daemonKey: null }))).toBeNull();
  expect(ringMoveTarget(host, row({ routes: ["ssh"] }))).toBeNull();
  expect(ringMoveTarget(host, row({ aliases: [H] }))).toBeNull();
  // Folded into another machine's row (this computer's own Daemon, say): never moved.
  expect(ringMoveTarget(host, row({ id: "local", aliases: ["local", H, R] }))).toBeNull();
  expect(ringMoveTarget({ ...host, launchPrefixes: { claude: "nix develop -c" } }, row())).toBeNull();
  expect(ringMoveTarget({ ...host, launchPrefixes: { claude: "" } }, row())).toBe(R);
});

it("keys of the saved Host continue under the Ring id; others are untouched", () => {
  expect(moveProjectKey(`host:${H}:/p:q`, H, R)).toBe(`host:${R}:/p:q`);
  expect(moveProjectKey("host:h_other000:/p", H, R)).toBe("host:h_other000:/p");
  expect(moveProjectKey("/local", H, R)).toBe("/local");
  expect(moveSessionFolderId(JSON.stringify([H, "pi", "/p", "s"]), H, R)).toBe(JSON.stringify([R, "pi", "/p", "s"]));
  expect(moveSessionFolderId("raw-session", H, R)).toBe("raw-session");
  expect(moveSessionFolderId(JSON.stringify([null, "pi", "/p", "s"]), H, R)).toBe(JSON.stringify([null, "pi", "/p", "s"]));
  const k = (h: string, t: string) => JSON.stringify(["daemon", h, t]);
  const panels = { [k(H, "a")]: "git", [k(H, "b")]: "files", [k(R, "b")]: "closed", [k("local", "c")]: "git", [JSON.stringify(["in-process", "t"])]: "files" };
  expect(moveTerminalKeys(panels, H, R)).toEqual({ [k(R, "a")]: "git", [k(R, "b")]: "closed", [k("local", "c")]: "git", [JSON.stringify(["in-process", "t"])]: "files" });
  const untouched = { [k(R, "a")]: "git" };
  expect(moveTerminalKeys(untouched, H, R)).toBe(untouched);
});
