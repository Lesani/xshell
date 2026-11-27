import { describe, expect, it } from "vitest";
import { connectionLine, formatResetTime, localLine, pairingBlocked, pairingLine, problemLine, quotaLine, relayHost, relayUrlError, validRelayUrl } from "./mobileSettings";
import { fmt, S } from "./strings";
import { C } from "./copy";
import type { RingStatus } from "./types";

const HOSTED = "wss://relay.xshell.sh";

function status(over: Partial<RingStatus> = {}): RingStatus {
  return {
    enabled: true, ringId: "r", version: 1, relayUrl: HOSTED, hostedRelayUrl: HOSTED, connection: "connected",
    retryIn: null, connectionError: null, limited: false, problem: null, problemDetail: null, members: [],
    quotaResetAt: null, relayHosted: true, local: "daemon", hosts: [], membership: "member", removed: null,
    oldRing: null, finishingOffline: false, busy: false,
    ...over,
  };
}

describe("quota notice (#42)", () => {
  const now = Date.UTC(2026, 9, 10, 12, 0, 0);
  const reset = Date.UTC(2026, 9, 11) / 1000;
  const at = (t: number) => `at ${t}`;

  it("names the hosted relay and the reset time", () => {
    expect(quotaLine(status({ quotaResetAt: reset, relayHosted: true }), now, at))
      .toBe(fmt("mobile.quota.hosted", { time: `at ${reset}` }));
  });

  it("says your own relay on a self-hosted relay", () => {
    expect(quotaLine(status({ quotaResetAt: reset, relayHosted: false }), now, at))
      .toBe(fmt("mobile.quota.ownRelay", { time: `at ${reset}` }));
  });

  it("is gone without a reset time, once it has passed, and in another window", () => {
    expect(quotaLine(status({ quotaResetAt: null }), now, at)).toBeNull();
    expect(quotaLine(status({ quotaResetAt: reset }), reset * 1000, at)).toBeNull();
    expect(quotaLine(status({ quotaResetAt: reset }), reset * 1000 + 1, at)).toBeNull();
    expect(quotaLine(status({ quotaResetAt: reset, connection: "other-window" }), now, at)).toBeNull();
  });

  it("shows the reset time in local time", () => {
    const d = new Date(reset * 1000);
    expect(formatResetTime(reset)).toBe(d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" }));
    expect(quotaLine(status({ quotaResetAt: reset }), now)).toContain(formatResetTime(reset));
  });
});

describe("connection line (copy-rjw-3)", () => {
  it("the default service: no address, \"second\" when n is 1", () => {
    expect(connectionLine(status())).toBe("Connected to the service.");
    expect(connectionLine(status({ connection: "waiting", retryIn: 4 }))).toBe("Can't reach the service. Retrying in 4 seconds.");
    expect(connectionLine(status({ connection: "waiting", retryIn: 1 }))).toBe("Can't reach the service. Retrying in 1 second.");
    expect(connectionLine(status({ connection: "waiting", retryIn: 0 }))).toBe("Can't reach the service. Retrying in 1 second.");
    expect(connectionLine(status({ connection: "connecting" }))).toBeNull();
    // The build's default address (a test Relay) is the default service too.
    expect(connectionLine(status({ relayUrl: "ws://127.0.0.1:48790", hostedRelayUrl: "ws://127.0.0.1:48790", relayHosted: true }))).toBe("Connected to the service.");
    expect(connectionLine(status({ connection: "stopped" }))).toBe(fmt("mobile.conn.stopped"));
    expect(connectionLine(status({ connection: "off", enabled: false, membership: "none" }))).toBeNull();
    expect(connectionLine(status({ limited: true }))).toBe(fmt("mobile.conn.limited"));
  });

  it("a self-chosen address names its host, never the full URL", () => {
    const own = { relayUrl: "wss://mine.example:8443/path?x=1", relayHosted: false };
    expect(relayHost(own.relayUrl)).toBe("mine.example:8443");
    expect(connectionLine(status(own))).toBe("Connected to mine.example:8443.");
    expect(connectionLine(status({ ...own, connection: "waiting", retryIn: 3 }))).toBe("Can't reach mine.example:8443. Retrying in 3 seconds.");
    expect(connectionLine(status({ ...own, connection: "waiting", retryIn: 1 }))).toBe("Can't reach mine.example:8443. Retrying in 1 second.");
  });

  it("never says relay", () => {
    for (const over of [{}, { connection: "waiting" as const, retryIn: 2 }, { connection: "connecting" as const }, { relayUrl: "wss://own.example", relayHosted: false, connection: "connecting" as const }]) {
      expect((connectionLine(status(over)) ?? "").toLowerCase()).not.toContain("relay");
    }
  });

  it("reports local limits and recovery", () => {
    expect(localLine(status())).toBeNull();
    expect(localLine(status({ local: "too-old" }))).toBe(C["mobile.local.tooOld"]);
    expect(problemLine(status({ problem: "recovered", problemDetail: "/p/ring.json.bad-1" }))).toBeNull();
    expect(problemLine(status({ problem: "unreadable", problemDetail: "bad json" }))).toBe(C["mobile.problem.unreadable"]);
    expect(problemLine(status())).toBeNull();
  });
});

describe("Pair a phone / Add a machine and the connection", () => {
  it("blocks only where pairing cannot succeed", () => {
    expect(pairingBlocked(status({ connection: "connected" }))).toBe(false);
    expect(pairingBlocked(status({ connection: "connecting" }))).toBe(false);
    expect(pairingBlocked(status({ connection: "waiting", retryIn: 4 }))).toBe(true);
    expect(pairingBlocked(status({ connection: "stopped" }))).toBe(true);
  });

  it("says why with the required line: default service, or the chosen address", () => {
    expect(pairingLine(status({ connection: "connected" }))).toBeNull();
    expect(pairingLine(status({ connection: "connecting" }))).toBeNull();
    expect(pairingLine(status({ connection: "waiting", retryIn: 4 }))).toBe(C["ring.connection.default.required"]);
    expect(pairingLine(status({ connection: "waiting", relayUrl: "wss://own.example", relayHosted: false }))).toBe("Can't reach own.example. Check your connection and try again.");
    // A phone offer already shown stays valid while the connection retries.
    expect(pairingLine(status({ connection: "waiting" }), true)).toBeNull();
    expect(pairingLine(status({ connection: "stopped" }))).toBe(S["mobile.pair.needsMember"]);
  });
});

describe("the Relay chosen before the Ring exists", () => {
  it("checks wss, and ws only to loopback", () => {
    for (const ok of ["wss://relay.example.com", "wss://relay.example.com:8443/x", "ws://localhost:8787", "ws://127.0.0.1", "ws://[::1]:80", " wss://r.example "]) {
      expect(validRelayUrl(ok), ok).toBe(true);
    }
    for (const bad of ["", "relay.example.com", "https://r.example", "ws://r.example", "ws://10.0.0.1", "wss://u@r.example", "wss://r.example?x", "wss://r.example#f", "wss://r%2e.example", "wss://"]) {
      expect(validRelayUrl(bad), bad).toBe(false);
    }
    // Empty: the hosted Relay, no error.
    expect(relayUrlError("")).toBeNull();
    expect(relayUrlError("ws://r.example")).toBe(fmt("mobile.relay.err.invalid"));
  });
});
