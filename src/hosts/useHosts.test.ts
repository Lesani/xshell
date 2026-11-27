import { describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
vi.mock("@tauri-apps/plugin-store", () => ({ load: vi.fn() }));

import { isConnecting, statusLabel } from "./useHosts";
import type { RegistrySnapshot } from "./registry";
import type { AvailableHost, HostStatus, Reachability } from "./types";

describe("statusLabel", () => {
  it("names every status, build differs included (#49)", () => {
    const label = (status: HostStatus["status"]) => statusLabel({ status } as HostStatus);
    expect(label("build-differs")).toBe("Different build");
    expect(label("upgrade-pending")).toBe("Upgrade pending");
    expect(label("connected")).toBe("Connected");
    expect(statusLabel(undefined)).toBe("Reconnecting");
  });
});

describe("isConnecting", () => {
  const ring = `r_${"a".repeat(64)}`;
  const row = (id: string, activeRoute: AvailableHost["activeRoute"], reachability: Reachability, aliases: string[] = [id]): AvailableHost =>
    ({ id, aliases, name: id, color: null, daemonKey: null, routes: [activeRoute], activeRoute, reachability });
  const snap = (available: AvailableHost[], status: Record<string, HostStatus["status"]>): RegistrySnapshot =>
    ({ configs: [], available, status: Object.fromEntries(Object.entries(status).map(([host, s]) => [host, { host, status: s } as HostStatus])), live: {}, agents: {}, epochs: {} });

  it("is true while the link is being made, a Host without a status included", () => {
    expect(isConnecting(snap([], { h_aaaaaaaa: "reconnecting" }), "h_aaaaaaaa")).toBe(true);
    expect(isConnecting(snap([], {}), "h_aaaaaaaa")).toBe(true);
    expect(isConnecting(snap([], {}), undefined)).toBe(false);
  });

  it.each(["connected", "offline", "incompatible", "upgrade-pending", "build-differs"] as const)("is false for %s", status => {
    expect(isConnecting(snap([], { h_aaaaaaaa: status }), "h_aaaaaaaa")).toBe(false);
  });

  it("reads the status of the Host an alias stands for", () => {
    const available = [row("h_aaaaaaaa", "ssh", { kind: "online" }, ["h_aaaaaaaa", ring])];
    expect(isConnecting(snap(available, { h_aaaaaaaa: "connected" }), ring)).toBe(false);
    expect(isConnecting(snap(available, { h_aaaaaaaa: "reconnecting" }), ring)).toBe(true);
  });

  it("a Relay machine that is not there is offline, not connecting", () => {
    const at = (reachability: Reachability, route: AvailableHost["activeRoute"] = "relay") => isConnecting(snap([row(ring, route, reachability)], { [ring]: "reconnecting" }), ring);
    expect(at({ kind: "online" })).toBe(true);
    expect(at({ kind: "unknown" })).toBe(true);
    expect(at({ kind: "unreachable", at: null })).toBe(false);
    expect(at({ kind: "closed", reason: "sleep", at: null })).toBe(false);
    expect(at({ kind: "never-connected" })).toBe(false);
    // Over SSH the link's own status decides.
    expect(at({ kind: "unreachable", at: null }, "ssh")).toBe(true);
  });
});
