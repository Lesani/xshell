import { describe, expect, it } from "vitest";
import { connectionLine, formatResetTime, localLine, pairingBlocked, pairingNote, problemLine, quotaLine, relayHost, relayUrlError, validRelayUrl } from "./mobileSettings";
import { fmt, S } from "./strings";
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

describe("connection line", () => {
  it("says how the Relay connection stands", () => {
    expect(connectionLine(status())).toBe("Connected to relay.xshell.sh");
    expect(connectionLine(status({ limited: true }))).toBe(fmt("mobile.conn.limited"));
    expect(connectionLine(status({ connection: "connecting" }))).toBe("Connecting to relay.xshell.sh…");
    expect(connectionLine(status({ connection: "waiting", retryIn: 4 }))).toBe("Can't reach relay.xshell.sh. Retrying in 4s");
    expect(connectionLine(status({ connection: "waiting", retryIn: 0 }))).toBe("Can't reach relay.xshell.sh. Retrying in 1s");
    expect(connectionLine(status({ connection: "stopped" }))).toBe(fmt("mobile.conn.stopped"));
    expect(connectionLine(status({ connection: "off", enabled: false, membership: "none" }))).toBeNull();
    expect(connectionLine(status({ connection: "other-window" }))).toBe("Another xshell window manages the connection to your devices.");
  });

  it("names the host of the current relay, with the port, never the full URL", () => {
    const s = status({ relayUrl: "wss://mine.example:8443/path?x=1" });
    expect(relayHost(s.relayUrl)).toBe("mine.example:8443");
    expect(connectionLine(s)).toBe("Connected to mine.example:8443");
    expect(connectionLine(status({ ...s, connection: "connecting" }))).toBe("Connecting to mine.example:8443…");
    expect(connectionLine(status({ ...s, connection: "waiting", retryIn: 3 }))).toBe("Can't reach mine.example:8443. Retrying in 3s");
    expect(connectionLine(status({ ...s, limited: true }))).toBe(fmt("mobile.conn.limited"));
  });

  it("reports local limits and recovery", () => {
    expect(localLine(status())).toBeNull();
    expect(localLine(status({ local: "too-old" }))).toBe(fmt("mobile.local.tooOld"));
    // Create starts over after a recovery: no line; refused settings say so.
    expect(problemLine(status({ problem: "recovered", problemDetail: "/p/ring.json.bad-1" }))).toBeNull();
    expect(problemLine(status({ problem: "unreadable", problemDetail: "bad json" }))).toBe(fmt("mobile.problem.unreadable", { error: "bad json" }));
    expect(problemLine(status())).toBeNull();
  });
});

describe("pairing and the connection", () => {
  it("disables pairing only where it cannot succeed", () => {
    expect(pairingBlocked(status({ connection: "connected" }))).toBeNull();
    expect(pairingBlocked(status({ connection: "connecting" }))).toBeNull();
    expect(pairingBlocked(status({ connection: "waiting", retryIn: 4 }))).toBe("mobile.pair.needsRelay");
    expect(pairingBlocked(status({ connection: "stopped" }))).toBe("mobile.pair.needsMember");
  });

  it("notes that pairing waits for the connection, or why it is disabled", () => {
    expect(pairingNote(status({ connection: "connecting" }))).toBe("mobile.pair.connecting");
    expect(pairingNote(status({ connection: "connected" }))).toBeNull();
    expect(pairingNote(status({ connection: "waiting", retryIn: 4 }))).toBe("mobile.pair.needsRelay");
    expect(pairingNote(status({ connection: "stopped" }))).toBe("mobile.pair.needsMember");
  });

  it("keeps a shown offer waiting for the connection while it retries", () => {
    expect(pairingNote(status({ connection: "waiting", retryIn: 4 }), true)).toBe("mobile.pair.connecting");
    expect(pairingNote(status({ connection: "connecting" }), true)).toBe("mobile.pair.connecting");
    expect(pairingNote(status({ connection: "connected" }), true)).toBeNull();
    // No longer a member: the offer cannot succeed.
    expect(pairingNote(status({ connection: "stopped" }), true)).toBe("mobile.pair.needsMember");
  });

  it("has the notes' strings", () => {
    for (const k of ["mobile.pair.connecting", "mobile.pair.needsRelay", "mobile.pair.needsMember"] as const) {
      expect(S[k], k).toBeTruthy();
    }
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
