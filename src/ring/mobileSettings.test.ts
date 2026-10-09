import { describe, expect, it } from "vitest";
import { canEnable, canSave, connectionLine, startsOver, initialForm, isDirty, localLine, moveLine, presenceChip, presenceKey, problemLine, relayChoice, roleKey, targetUrl, urlError, validRelayUrl } from "./mobileSettings";
import { fmt, S } from "./strings";
import type { MemberView, RingStatus } from "./types";

const HOSTED = "wss://relay.xshell.app";

function status(over: Partial<RingStatus> = {}): RingStatus {
  return {
    enabled: true,
    ringId: "r",
    version: 1,
    relayUrl: HOSTED,
    hostedRelayUrl: HOSTED,
    connection: "connected",
    retryIn: null,
    connectionError: null,
    limited: false,
    move: null,
    problem: null,
    problemDetail: null,
    members: [],
    local: "daemon",
    ...over,
  };
}

function member(kind: MemberView["presence"]["kind"], over: Partial<MemberView> = {}): MemberView {
  return { name: "m", role: "daemon", signKey: "k", thisApp: false, thisComputer: false, presence: { kind }, ...over };
}

describe("presence", () => {
  it("maps each presence to its status", () => {
    const s = status();
    expect(presenceKey(member("online"), s)).toBe("mobile.status.online");
    expect(presenceKey(member("closed"), s)).toBe("mobile.status.closed");
    expect(presenceKey(member("unreachable"), s)).toBe("mobile.status.unreachable");
    expect(presenceKey(member("never"), s)).toBe("mobile.status.never");
    expect(fmt(presenceKey(member("closed"), s))).toBe("xshell closed");
  });

  it("is unknown for others while this Desktop is not connected", () => {
    for (const connection of ["connecting", "waiting", "stopped"] as const) {
      expect(presenceKey(member("online"), status({ connection }))).toBe("mobile.status.unknown");
    }
    expect(presenceKey(member("online", { thisApp: true }), status({ connection: "connected" }))).toBe("mobile.status.online");
  });

  it("colours chips like the Host chips", () => {
    expect(presenceChip("mobile.status.online")).toBe("connected");
    expect(presenceChip("mobile.status.unreachable")).toBe("offline");
    expect(presenceChip("mobile.status.unknown")).toBe("unknown");
  });

  it("never says daemon to the user", () => {
    expect(fmt(roleKey("daemon"))).toBe("Host");
    expect(fmt(roleKey("desktop"))).toBe("Desktop");
    expect(fmt(roleKey("mobile"))).toBe("Phone");
    for (const v of Object.values(S)) expect(v.toLowerCase()).not.toContain("daemon");
  });
});

describe("connection line", () => {
  it("says how the Relay connection stands", () => {
    expect(connectionLine(status())).toBe("Connected to the relay");
    expect(connectionLine(status({ limited: true }))).toBe(fmt("mobile.conn.limited"));
    expect(connectionLine(status({ connection: "connecting" }))).toBe("Connecting to the relay…");
    expect(connectionLine(status({ connection: "waiting", retryIn: 4 }))).toBe("Can't reach the relay. Retrying in 4s");
    expect(connectionLine(status({ connection: "waiting", retryIn: 0 }))).toBe("Can't reach the relay. Retrying in 1s");
    expect(connectionLine(status({ connection: "stopped" }))).toBe(fmt("mobile.conn.stopped"));
    expect(connectionLine(status({ connection: "off", enabled: false }))).toBeNull();
    expect(connectionLine(status({ connection: "other-window" }))).toBe("Another xshell window manages the connection to your devices.");
  });

  it("reports a pending or failed move, local limits and recovery", () => {
    expect(moveLine(status())).toBeNull();
    expect(moveLine(status({ move: { state: "moving" } }))).toBe(fmt("mobile.conn.moving"));
    expect(moveLine(status({ move: { state: "failed", error: "x" } }))).toBe(fmt("mobile.conn.moveFailed"));
    expect(localLine(status())).toBeNull();
    expect(localLine(status({ local: "in-process" }))).toBe(fmt("mobile.local.inProcess"));
    expect(localLine(status({ local: "too-old" }))).toBe(fmt("mobile.local.tooOld"));
    expect(problemLine(status({ problem: "recovered", problemDetail: "/p/ring.json.bad-1" }))).toContain("/p/ring.json.bad-1");
    expect(problemLine(status())).toBeNull();
  });
});

describe("relay form", () => {
  it("preselects Hosted, or the self-hosted URL in use", () => {
    expect(relayChoice(status())).toBe("hosted");
    expect(initialForm(status())).toEqual({ choice: "hosted", url: "" });
    const custom = status({ relayUrl: "wss://mine.example" });
    expect(relayChoice(custom)).toBe("custom");
    expect(initialForm(custom)).toEqual({ choice: "custom", url: "wss://mine.example" });
    expect(relayChoice(status({ relayUrl: null, enabled: false }))).toBe("hosted");
  });

  it("is dirty and savable only when it changes the Relay", () => {
    const s = status();
    expect(isDirty({ choice: "hosted", url: "" }, s)).toBe(false);
    expect(canSave({ choice: "hosted", url: "" }, s, false)).toBe(false);
    expect(canSave({ choice: "custom", url: "" }, s, false)).toBe(false);
    expect(canSave({ choice: "custom", url: "wss://mine.example" }, s, false)).toBe(true);
    expect(canSave({ choice: "custom", url: "wss://mine.example" }, s, true)).toBe(false);
    expect(canSave({ choice: "custom", url: "ws://mine.example" }, s, false)).toBe(false);
    const custom = status({ relayUrl: "wss://mine.example" });
    expect(canSave({ choice: "custom", url: " wss://mine.example " }, custom, false)).toBe(false);
    expect(canSave({ choice: "hosted", url: "wss://mine.example" }, custom, false)).toBe(true);
    expect(targetUrl({ choice: "hosted", url: "wss://x" }, custom)).toBe(HOSTED);
  });

  it("waits for a pending relay change before another", () => {
    const s = status({ move: { state: "moving" } });
    expect(canSave({ choice: "custom", url: "wss://mine.example" }, s, false)).toBe(false);
    expect(canSave({ choice: "custom", url: "wss://mine.example" }, status({ move: { state: "failed" } }), false)).toBe(false);
  });

  it("starts over only explicitly, after a recovery", () => {
    expect(startsOver(status({ enabled: false, problem: "recovered" }))).toBe(true);
    expect(startsOver(status({ enabled: false }))).toBe(false);
    // Independent of a stale enabled flag.
    expect(startsOver(status({ enabled: true, problem: "recovered" }))).toBe(true);
    expect(canEnable(status({ enabled: true, problem: "recovered" }))).toBe(true);
    expect(canEnable(status({ enabled: false, problem: "recovered" }))).toBe(true);
    expect(canEnable(status({ enabled: false, problem: "unreadable" }))).toBe(false);
    expect(canEnable(status())).toBe(false);
  });

  it("checks wss, and ws only to loopback", () => {
    for (const ok of ["wss://relay.example.com", "wss://relay.example.com:8443/x", "ws://localhost:8787", "ws://127.0.0.1", "ws://[::1]:80", " wss://r.example "]) {
      expect(validRelayUrl(ok), ok).toBe(true);
    }
    for (const bad of ["", "relay.example.com", "https://r.example", "ws://r.example", "ws://10.0.0.1", "wss://u@r.example", "wss://r.example?x", "wss://r.example#f", "wss://r%2e.example", "wss://"]) {
      expect(validRelayUrl(bad), bad).toBe(false);
    }
    expect(urlError({ choice: "custom", url: "" })).toBeNull();
    expect(urlError({ choice: "custom", url: "ws://r.example" })).toBe(fmt("mobile.relay.err.invalid"));
    expect(urlError({ choice: "hosted", url: "ws://r.example" })).toBeNull();
  });
});
