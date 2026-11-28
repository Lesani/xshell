import { expect, it, vi } from "vitest";
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({ listen: async () => () => {} }));
vi.mock("@tauri-apps/plugin-store", () => ({ load: async () => ({ get: async () => null, set: async () => {}, save: async () => {} }) }));
import { moveProjectKey, moveProjectSettings, moveSessionFolderId, moveSidebarLayout, moveTerminalKeys, ringMoveTarget } from "./sshRing";
import type { ProjectSettings, SidebarItem } from "../types";
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

it("the Sidebar Layout keeps order and folders; a Project pinned under both ids is kept once", () => {
  const key = (h: string, p: string) => `host:${h}:${p}` as never;
  const layout: SidebarItem[] = [
    { kind: "project", path: key(R, "/both") },
    { kind: "folder", id: "f", name: "Work", collapsed: false, projectPaths: [key(H, "/a"), "/local" as never, key(H, "/both")] },
    { kind: "folder", id: "g", name: "Only", collapsed: true, projectPaths: [key(H, "/both")] },
    { kind: "project", path: key(H, "/b") },
  ];
  expect(moveSidebarLayout(layout, H, R)).toEqual([
    { kind: "project", path: key(R, "/both") },
    { kind: "folder", id: "f", name: "Work", collapsed: false, projectPaths: [key(R, "/a"), "/local"] },
    { kind: "project", path: key(R, "/b") },
  ]);
  const other: SidebarItem[] = [{ kind: "project", path: "/local" as never }];
  expect(moveSidebarLayout(other, H, R)).toBe(other);
});

it("project settings under both ids are merged: the Ring entry's look wins, the SSH entry's folders stay", () => {
  const pi = (h: string, id: string) => JSON.stringify([h, "pi", "/p", id]);
  const icons: Record<string, ProjectSettings> = {
    // A minimal Ring entry: only an icon.
    [`host:${R}:/p`]: { icon: "RG" },
    [`host:${H}:/p`]: { icon: "SH", color: "#123456", customName: "Proj", folders: [
      { id: "f1", name: "Bugs", sessionIds: ["s1", pi(H, "p1")] },
      { id: "f2", name: "Ideas", collapsed: true, sessionIds: ["s2"] },
    ] },
    [`host:${H}:/solo`]: { customName: "Solo" },
    "/local": { customName: "Mine" },
  };
  const moved = moveProjectSettings(icons, H, R);
  expect(moved).toEqual({
    [`host:${R}:/p`]: { icon: "RG", color: "#123456", customName: "Proj", folders: [
      { id: "f1", name: "Bugs", sessionIds: ["s1", pi(R, "p1")] },
      { id: "f2", name: "Ideas", collapsed: true, sessionIds: ["s2"] },
    ] },
    [`host:${R}:/solo`]: { customName: "Solo" },
    "/local": { customName: "Mine" },
  });
  // Both have folders: the same folder gets both memberships under the Ring entry's name; a
  // session the Ring entry filed stays there; other SSH folders are appended.
  const both = moveProjectSettings({
    [`host:${R}:/p`]: { customName: "Ring name", folders: [{ id: "f1", name: "Ring bugs", sessionIds: ["r1", "s2"] }] },
    [`host:${H}:/p`]: { customName: "SSH name", icon: "SH", folders: [{ id: "f1", name: "Bugs", sessionIds: ["s1", "r1"] }, { id: "f2", name: "Ideas", sessionIds: ["s2", "s3"] }] },
  }, H, R);
  expect(both).toEqual({ [`host:${R}:/p`]: { customName: "Ring name", icon: "SH", folders: [{ id: "f1", name: "Ring bugs", sessionIds: ["r1", "s2", "s1"] }, { id: "f2", name: "Ideas", sessionIds: ["s3"] }] } });
  const untouched = { "/local": { customName: "Mine" } };
  expect(moveProjectSettings(untouched, H, R)).toBe(untouched);
});
