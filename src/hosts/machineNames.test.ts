import { afterEach, describe, expect, it } from "vitest";
import { cache, parseCache } from "./cache";
import { registry } from "./registry";
import { ringHostId } from "./ringIdentity";
import { restorableTabs } from "./appTabs";
import type { TerminalInfo } from "./types";

const ID = `r_${"ab".repeat(32)}`;
const KEY = btoa(String.fromCharCode(...new Array(32).fill(0xab))).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");

afterEach(() => { cache._reset(); registry._dispose(); });

describe("machine names (never a raw Host id)", () => {
  it("a Roster key maps to the backend's Ring Host id", () => {
    expect(ringHostId(KEY)).toBe(ID);
    expect(ringHostId("short")).toBeNull();
    expect(ringHostId(null)).toBeNull();
  });

  it("remembers the last name per Host id and alias, and keeps it across a reload of the cache file", () => {
    cache.rememberNames([{ id: ID, aliases: ["h_aaaaaaaa"], name: "hotel" }]);
    expect(cache.lastName(ID)).toBe("hotel");
    expect(cache.lastName("h_aaaaaaaa")).toBe("hotel");
    const reloaded = parseCache(JSON.parse(JSON.stringify(cache.getState())));
    expect(reloaded.names?.[ID]).toBe("hotel");
    expect(parseCache({ version: 1, hosts: {}, names: { [ID]: 5 } }).names).toEqual({});
  });

  it("labels: the last known name, '(not in Ring)' once the Roster dropped it, else the neutral wording", () => {
    expect(registry.hostName(ID)).toBe("a machine"); // COPY-GAP fallback, never the id
    cache.rememberNames([{ id: ID, name: "hotel" }]);
    expect(registry.hostName(ID)).toBe("hotel");
    registry.setRingHosts([`r_${"cd".repeat(32)}`]);
    expect(registry.isDeparted(ID)).toBe(true);
    expect(registry.hostName(ID)).toBe("hotel (not in Ring)");
    expect(registry.machineName(ID)).toBe("hotel");
    registry.setRingHosts([ID]);
    expect(registry.isDeparted(ID)).toBe(false);
    registry.setRingHosts(null);
    expect(registry.isDeparted(ID)).toBe(false);
    // SSH Hosts never count as having left the Ring.
    registry.setRingHosts([]);
    expect(registry.isDeparted("h_aaaaaaaa")).toBe(false);
  });

  it("restore makes one Tab per Terminal: the Host still in the Ring wins over one that left", () => {
    const NEW = `r_${"cd".repeat(32)}`;
    const info: TerminalInfo = { terminal: "same-uuid", spec: { agent: "claude", cwd: "/p" }, meta: {}, createdAtMs: 1, pid: 1, exitCode: null };
    registry.setRingHosts([NEW]);
    const tabs = restorableTabs({ saved: [], hosts: [ID, NEW], localDaemon: false, cached: () => [info] });
    expect(tabs).toHaveLength(1);
    expect(tabs[0].host).toBe(NEW);
  });
});
