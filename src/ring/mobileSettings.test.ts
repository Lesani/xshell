import { afterEach, describe, expect, it, vi } from "vitest";
import { canClaim, canEnable, canRemove, canSave, connectionLine, hostLine, lastSeenLine, startsOver, initialForm, isDirty, localLine, moveLine, presenceChip, presenceKey, problemLine, relayChoice, removalNote, removalReducer, REMOVAL_IDLE, removeConfirm, removeErrorLine, removeHint, removePendingLine, roleKey, targetUrl, urlError, validRelayUrl } from "./mobileSettings";
import { fmt, S } from "./strings";
import type { HostRingState, MemberView, RingStatus } from "./types";

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
    hosts: [],
    ...over,
  };
}

function member(kind: MemberView["presence"]["kind"], over: Partial<MemberView> = {}): MemberView {
  return { name: "m", role: "daemon", signKey: "k", thisApp: false, thisComputer: false, hostId: null, removable: false, presence: { kind }, ...over };
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

describe("remote hosts", () => {
  const host = (state: HostRingState["state"], error?: string): HostRingState => ({ host: "h_aaaaaaaa", name: "build-box", state, error });

  it("says why a Host is not among the devices", () => {
    expect(hostLine(host("too-old"))).toBe("Update xshelld on build-box in Settings → Hosts to make it reachable from your phone.");
    expect(hostLine(host("other-ring"))).toBe("build-box is paired with another set of devices. Pairing here will end their mobile access to it.");
    expect(hostLine(host("full"))).toBe("build-box can't join your devices. You've reached the limit of 64. Remove a device, then try again.");
    expect(hostLine(host("failed", "timed out"))).toBe("Couldn't pair build-box: timed out");
    expect(fmt("mobile.host.claim")).toBe("Pair with this desktop");
  });

  it("offers pairing only for a Host of another set of devices", () => {
    expect(canClaim(host("other-ring"))).toBe(true);
    for (const s of ["too-old", "full", "failed"] as const) expect(canClaim(host(s))).toBe(false);
  });

  it("lists nothing when every Host joined", () => {
    expect(status().hosts).toEqual([]);
    expect(status().hosts.map(hostLine)).toEqual([]);
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

describe("removing a device", () => {
  const phone = (over: Partial<MemberView> = {}) => member("online", { name: "Pixel", role: "mobile", signKey: "p", removable: true, ...over });

  it("offers Remove only for removable devices, in the window that runs the connection", () => {
    expect(canRemove(phone(), status())).toBe(true);
    expect(canRemove(phone(), status({ connection: "waiting" }))).toBe(true);
    expect(canRemove(phone(), status({ connection: "other-window" }))).toBe(false);
    expect(canRemove(phone({ removable: false }), status())).toBe(false);
    expect(canRemove(phone({ thisApp: true }), status())).toBe(false);
    expect(canRemove(phone({ thisComputer: true }), status())).toBe(false);
  });

  it("explains why a Host has no Remove button", () => {
    const hint = fmt("mobile.remove.hostHint");
    expect(removeHint(member("online", { thisComputer: true }))).toBe(hint);
    expect(removeHint(member("online", { hostId: "h_aaaaaaaa" }))).toBe(hint);
    expect(removeHint(member("online", { thisApp: true, role: "desktop" }))).toBeNull();
    expect(removeHint(phone())).toBeNull();
    expect(removeHint(member("online", { removable: true }))).toBeNull();
  });

  describe("last seen", () => {
    afterEach(() => { vi.useRealTimers(); });

    it("says when a closed or unreachable device was last seen", () => {
      vi.useFakeTimers();
      vi.setSystemTime(new Date("2026-10-10T12:00:00Z"));
      const at = Date.parse("2026-10-10T11:55:00Z") / 1000;
      expect(lastSeenLine(member("closed", { presence: { kind: "closed", reason: "quit", at } }), status())).toBe("Last seen 5m ago");
      expect(lastSeenLine(member("unreachable", { presence: { kind: "unreachable", at: at + 290 } }), status())).toBe("Last seen just now");
      const closed = member("closed", { presence: { kind: "closed", at } });
      expect(lastSeenLine(closed, status({ connection: "waiting" }))).toBeNull();
      expect(lastSeenLine(member("closed"), status())).toBeNull();
      expect(lastSeenLine(member("online", { presence: { kind: "online", at } }), status())).toBeNull();
      expect(lastSeenLine(member("never"), status())).toBeNull();
    });
  });

  it("confirms with a body for the device's role", () => {
    const c = removeConfirm({ name: "Pixel", role: "mobile" });
    expect(c.title).toBe("Remove Pixel?");
    expect(c.body).toBe(fmt("mobile.remove.confirmBody.mobile", { name: "Pixel" }));
    expect(c.body).toContain("Pixel");
    expect(c.confirm).toBe("Remove");
    expect(removeConfirm({ name: "build-box", role: "daemon" }).body).toBe(fmt("mobile.remove.confirmBody.daemon", { name: "build-box" }));
    expect(removeConfirm({ name: "laptop", role: "desktop" }).body).toBe(fmt("mobile.remove.confirmBody.desktop", { name: "laptop" }));
  });

  it("maps the Desktop's refusals", () => {
    expect(removeErrorLine("Pixel", "in_use: this device belongs to a host")).toBe(fmt("mobile.remove.err.inUse"));
    expect(removeErrorLine("Pixel", "self: this app can't remove itself")).toBe(fmt("mobile.remove.err.self"));
    expect(removeErrorLine("Pixel", "other_window: another xshell window manages your devices")).toBe(fmt("mobile.remove.err.otherWindow"));
    expect(removeErrorLine("Pixel", "not_enabled: mobile access is not enabled")).toBe("Couldn't remove Pixel: mobile access is not enabled");
    expect(removeErrorLine("Pixel", { message: "disk full" })).toBe("Couldn't remove Pixel: disk full");
  });

  it("notes a removal made while not connected to the relay", () => {
    expect(removePendingLine(status(), "Pixel")).toBeNull();
    for (const connection of ["connecting", "waiting", "stopped"] as const) {
      expect(removePendingLine(status({ connection }), "Pixel")).toBe(fmt("mobile.remove.pending", { name: "Pixel" }));
    }
  });

  it("follows a removal from the question to its note", () => {
    const m = phone();
    let r = removalReducer(REMOVAL_IDLE, { type: "ask", member: m });
    expect(r).toEqual({ state: "confirming", member: m });
    expect(removalReducer(r, { type: "cancel" })).toEqual(REMOVAL_IDLE);
    r = removalReducer(r, { type: "start" });
    expect(r).toEqual({ state: "removing", signKey: "p", name: "Pixel" });
    // Another row's Remove does nothing meanwhile.
    expect(removalReducer(r, { type: "ask", member: member("online") })).toBe(r);
    const failed = removalReducer(r, { type: "failed", error: "in_use: x" });
    expect(removalNote(failed)).toEqual({ text: fmt("mobile.remove.err.inUse"), error: true });
    const done = removalReducer(r, { type: "done", status: status({ connection: "waiting" }) });
    expect(removalNote(done)).toEqual({ text: fmt("mobile.remove.pending", { name: "Pixel" }), error: false });
    expect(removalNote(removalReducer(r, { type: "done", status: status() }))).toBeNull();
  });
});
