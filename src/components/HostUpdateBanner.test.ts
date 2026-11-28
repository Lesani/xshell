import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { beforeAll, describe, it, expect, vi } from "vitest";
import type { RegistrySnapshot } from "../hosts/registry";
import type { AvailableHost, HostStatus } from "../hosts/types";
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
const snap = vi.hoisted(() => ({ current: null as unknown as RegistrySnapshot }));
vi.mock("../hosts/useHosts", async original => ({ ...await original<typeof import("../hosts/useHosts")>(), useHostsSnapshot: () => snap.current }));
vi.mock("@tauri-apps/plugin-store", () => ({ load: async () => ({ get: async () => null, set: async () => {} }) }));
import { HostUpdateBanner } from "./HostUpdateBanner";
import { skippedVersions } from "../hosts/skippedVersions";

const row = (id: string, name: string, aliases: string[] = [id]): AvailableHost => ({ id, aliases, name, color: null, daemonKey: null, routes: ["relay"], activeRoute: "relay", reachability: { kind: "online" } } as AvailableHost);
const offered = (host: string): HostStatus => ({ host, status: "upgrade-pending", phase: null, lastError: null, errorHint: null, daemonVersion: "1.5.0", desktopVersion: "1.5.0", protocol: 1, os: "linux", arch: "x86_64", incompatibleReason: null, nextRetryAt: null, sinceMs: 1, linkGeneration: 1, updateAction: "update", updateSource: "release", updateVersion: "1.6.0" });
const current = (host: string): HostStatus => ({ ...offered(host), status: "connected", updateAction: null, updateSource: null, updateVersion: null, daemonVersion: "1.6.0" });
const A = row("r_a", "box-a"), B = row("r_b", "box-b", ["r_b", "h_bbbbbbbb"]), C = row("r_c", "box-c");
function render(status: Record<string, HostStatus>, host?: string, hidden = false) {
  snap.current = { configs: [], available: [A, B, C], status, live: {}, agents: {}, epochs: {} } as unknown as RegistrySnapshot;
  return renderToStaticMarkup(createElement(HostUpdateBanner, { host, hidden }));
}
const names = (html: string) => [...html.matchAll(/(box-[abc]) is running/g)].map(m => m[1]);

describe("the update bar", () => {
  it("shows no offer before the skipped versions are loaded", () => {
    expect(render({ r_a: offered("r_a") })).toBe("");
  });
  describe("once the skipped versions are loaded", () => {
    beforeAll(() => skippedVersions.load());
    it("lists every machine with something to show, the focused one first, by its row", () => {
      const status = { r_a: offered("r_a"), r_b: offered("r_b"), r_c: current("r_c") };
      expect(names(render(status))).toEqual(["box-a", "box-b"]);
      expect(names(render(status, "r_b"))).toEqual(["box-b", "box-a"]);
      // A Tab on an alias of a row focuses that row.
      expect(names(render(status, "h_bbbbbbbb"))).toEqual(["box-b", "box-a"]);
      expect(names(render(status, "local"))).toEqual(["box-a", "box-b"]);
    });
    it("renders nothing when nothing is offered, and nothing in Settings", () => {
      const html = render({ r_a: current("r_a"), r_b: current("r_b"), r_c: current("r_c") }, "r_a");
      expect(html).not.toContain("host-update-action");
      expect(html.replace(/<[^>]+>/g, "")).toBe("");
      expect(render({ r_a: offered("r_a") }, "r_a", true)).toBe("");
    });
  });
});
