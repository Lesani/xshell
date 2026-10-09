import { describe, expect, it } from "vitest";
import { canSubmit, codeError, COMPUTER_IDLE, computerDesc, computerLine, computerReducer, countdown, errorCode, expiresLine, failureKey, failureLine, formatCode, normalizeCode, offersNewCode, PHONE_IDLE, phoneReducer, qrRects, secondsLeft, validCode, type ComputerState, type PhoneState } from "./pairing";
import { fmt, S, type StringKey } from "./strings";
import type { PhoneStart } from "./types";

const HOSTED = "wss://relay.xshell.app";
const OFFER: PhoneStart = { payload: "xsp1.abc", qr: { size: 2, rows: ["10", "01"] }, expiresAt: 1000 };

describe("countdown", () => {
  it("formats {m}:{ss} and rounds partial seconds up", () => {
    expect(countdown(1000, 400_000)).toEqual({ m: "10", ss: "00" });
    expect(countdown(1000, 994_500)).toEqual({ m: "0", ss: "06" });
    expect(countdown(1000, 1000_000 - 65_000)).toEqual({ m: "1", ss: "05" });
    expect(expiresLine(1000, 1000_000 - 599_000)).toBe("Code expires in 9:59");
  });

  it("never goes below zero", () => {
    expect(secondsLeft(1000, 2000_000)).toBe(0);
    expect(countdown(1000, 2000_000)).toEqual({ m: "0", ss: "00" });
  });
});

describe("pair code", () => {
  it("normalizes case, separators and look-alikes", () => {
    expect(normalizeCode("7kq4-m2xw 9pjr-h3ct")).toBe("7KQ4M2XW9PJRH3CT");
    expect(normalizeCode("oOiIlL")).toBe("001111");
  });

  it("groups the code in fours as typed", () => {
    expect(formatCode("7kq4m2xw9pjrh3ct")).toBe("7KQ4-M2XW-9PJR-H3CT");
    expect(formatCode("7kq4")).toBe("7KQ4");
    expect(formatCode("7kq4-")).toBe("7KQ4");
    expect(formatCode("7kq4m")).toBe("7KQ4-M");
  });

  it("accepts 16 Crockford base32 characters only", () => {
    expect(validCode("7KQ4-M2XW-9PJR-H3CT")).toBe(true);
    expect(validCode("7kq4 m2xw 9pjr h3ct")).toBe(true);
    expect(validCode("7KQ4-M2XW-9PJR-H3OI")).toBe(true); // O→0, I→1
    expect(validCode("7KQ4-M2XW-9PJR-H3C")).toBe(false);
    expect(validCode("7KQ4-M2XW-9PJR-H3CTX")).toBe(false);
    expect(validCode("7KQ4-M2XW-9PJR-H3CU")).toBe(false);
    expect(validCode("")).toBe(false);
  });

  it("shows the invalid line only once typing on cannot help", () => {
    expect(codeError("")).toBeNull();
    expect(codeError("7KQ4")).toBeNull();
    expect(codeError("7KQ4-M2XW-9PJR-H3CT")).toBeNull();
    expect(codeError("7KU")).toBe(S["mobile.pair.computer.invalid"]);
    expect(codeError("7KQ4-M2XW-9PJR-H3CTX")).toBe(S["mobile.pair.computer.invalid"]);
  });

  it("adds --relay only off the Hosted Relay", () => {
    expect(computerDesc({ relayUrl: HOSTED, hostedRelayUrl: HOSTED })).toBe("On the computer you want to add, run `xshelld pair`, then enter the code it shows.");
    expect(computerDesc({ relayUrl: null, hostedRelayUrl: HOSTED })).not.toContain("--relay");
    expect(computerDesc({ relayUrl: "wss://r.example", hostedRelayUrl: HOSTED })).toBe("On the computer you want to add, run `xshelld pair` --relay wss://r.example, then enter the code it shows.");
  });
});

describe("failures", () => {
  it("maps codes to their lines", () => {
    const cases: [string, StringKey][] = [
      ["role", "mobile.pair.err.role"],
      ["full", "mobile.pair.err.full"],
      ["not_found", "mobile.pair.computer.notFound"],
      ["invalid_code", "mobile.pair.computer.invalid"],
      ["expired", "mobile.pair.expired"],
      ["relay", "mobile.pair.failed"],
      ["other", "mobile.pair.failed"],
    ];
    for (const [code, key] of cases) expect(failureKey(code)).toBe(key);
    expect(failureLine("crypto")).toBe("Couldn't pair the device: crypto");
    expect(failureLine("other_window", "other_window: busy")).toBe("Couldn't pair the device: other_window: busy");
    expect(failureLine("full")).toBe(S["mobile.pair.err.full"]);
  });

  it("reads the code a command error starts with", () => {
    expect(errorCode("other_window: another window owns it")).toBe("other_window");
    expect(errorCode("not_enabled")).toBe("not_enabled");
    expect(errorCode("invalid_code")).toBe("invalid_code");
    expect(errorCode("something broke")).toBe("other");
  });
});

describe("phone panel", () => {
  const run = (s: PhoneState, ...as: Parameters<typeof phoneReducer>[1][]) => as.reduce(phoneReducer, s);

  it("goes idle → starting → waiting → paired", () => {
    const w = run(PHONE_IDLE, { type: "start" }, { type: "started", offer: OFFER });
    expect(w).toEqual({ state: "waiting", offer: OFFER });
    expect(run(w, { type: "event", event: { flow: "phone", state: "waiting" } })).toBe(w);
    expect(run(w, { type: "event", event: { flow: "phone", state: "paired", name: "Pixel", role: "mobile" } })).toEqual({ state: "paired", name: "Pixel" });
  });

  it("expires from the event or the countdown, then offers a new code", () => {
    const w = run(PHONE_IDLE, { type: "start" }, { type: "started", offer: OFFER });
    expect(run(w, { type: "tick", nowMs: 999_000 })).toBe(w);
    const e = run(w, { type: "tick", nowMs: 1000_000 });
    expect(e).toEqual({ state: "expired" });
    expect(run(w, { type: "event", event: { flow: "phone", state: "expired" } })).toEqual({ state: "expired" });
    expect(offersNewCode(e)).toBe(true);
    expect(run(e, { type: "start" })).toEqual({ state: "starting" });
  });

  it("fails from an event or the command", () => {
    const w = run(PHONE_IDLE, { type: "start" }, { type: "started", offer: OFFER });
    expect(run(w, { type: "event", event: { flow: "phone", state: "failed", code: "role" } })).toEqual({ state: "failed", code: "role" });
    const f = run(PHONE_IDLE, { type: "start" }, { type: "startFailed", error: "not_enabled: off" });
    expect(f).toEqual({ state: "failed", code: "not_enabled", error: "not_enabled: off" });
    expect(offersNewCode(f)).toBe(true);
  });

  it("ignores other flows, idle events and the replaced offer's cancel", () => {
    const w = run(PHONE_IDLE, { type: "start" }, { type: "started", offer: OFFER });
    expect(run(w, { type: "event", event: { flow: "computer", state: "expired" } })).toBe(w);
    expect(run(w, { type: "event", event: { flow: "phone", state: "failed", code: "cancelled" } })).toBe(w);
    expect(run(PHONE_IDLE, { type: "event", event: { flow: "phone", state: "paired", name: "x", role: "mobile" } })).toBe(PHONE_IDLE);
    expect(run(w, { type: "start" })).toBe(w);
    expect(run(w, { type: "cancel" })).toEqual(PHONE_IDLE);
  });
});

describe("computer section", () => {
  const run = (s: ComputerState, ...as: Parameters<typeof computerReducer>[1][]) => as.reduce(computerReducer, s);
  const CODE = "7KQ4-M2XW-9PJR-H3CT";

  it("connects, then pairs", () => {
    expect(canSubmit("7KQ4", COMPUTER_IDLE)).toBe(false);
    expect(canSubmit(CODE, COMPUTER_IDLE)).toBe(true);
    const c = run(COMPUTER_IDLE, { type: "submit" });
    expect(c).toEqual({ state: "connecting" });
    expect(canSubmit(CODE, c)).toBe(false);
    expect(computerLine(c)).toBe(S["mobile.pair.computer.connecting"]);
    const p = run(c, { type: "event", event: { flow: "computer", state: "paired", name: "tower", role: "daemon" } });
    expect(p).toEqual({ state: "paired", name: "tower" });
    expect(computerLine(p)).toBe("Added tower");
  });

  it("reports not found, invalid and expired", () => {
    const c = run(COMPUTER_IDLE, { type: "submit" });
    const nf = run(c, { type: "event", event: { flow: "computer", state: "failed", code: "not_found" } });
    expect(computerLine(nf)).toBe(S["mobile.pair.computer.notFound"]);
    const inv = run(c, { type: "submitFailed", error: "invalid_code" });
    expect(computerLine(inv)).toBe(S["mobile.pair.computer.invalid"]);
    const ex = run(c, { type: "event", event: { flow: "computer", state: "expired" } });
    expect(computerLine(ex)).toBe(S["mobile.pair.expired"]);
  });

  it("clears a finished attempt on edit, not a running one", () => {
    const c = run(COMPUTER_IDLE, { type: "submit" });
    expect(run(c, { type: "edit" })).toBe(c);
    const f = run(c, { type: "event", event: { flow: "computer", state: "failed", code: "relay" } });
    expect(computerLine(f)).toBe("Couldn't pair the device: relay");
    expect(run(f, { type: "edit" })).toEqual(COMPUTER_IDLE);
    expect(run(c, { type: "event", event: { flow: "phone", state: "expired" } })).toBe(c);
    expect(run(c, { type: "event", event: { flow: "computer", state: "failed", code: "cancelled" } })).toBe(c);
    expect(run(c, { type: "cancel" })).toEqual(COMPUTER_IDLE);
    expect(computerLine(COMPUTER_IDLE)).toBeNull();
  });
});

describe("QR rectangles", () => {
  it("merges horizontal runs of dark modules", () => {
    expect(qrRects({ size: 3, rows: ["110", "011", "101"] })).toEqual([
      { x: 0, y: 0, w: 2 },
      { x: 1, y: 1, w: 2 },
      { x: 0, y: 2, w: 1 },
      { x: 2, y: 2, w: 1 },
    ]);
  });
});

describe("pairing strings", () => {
  const KEYS: StringKey[] = [
    "mobile.pair.phone", "mobile.pair.phone.desc", "mobile.pair.phone.expires", "mobile.pair.phone.copy",
    "mobile.pair.phone.new", "mobile.pair.expired", "mobile.pair.waiting", "mobile.pair.paired", "mobile.pair.failed",
    "mobile.pair.computer", "mobile.pair.computer.desc", "mobile.pair.computer.label", "mobile.pair.computer.placeholder",
    "mobile.pair.computer.connecting", "mobile.pair.computer.notFound", "mobile.pair.computer.invalid",
    "mobile.pair.err.role", "mobile.pair.err.full",
  ];

  it("has every key", () => {
    for (const k of KEYS) expect(S[k], k).toBeTruthy();
  });

  it("fills the placeholders", () => {
    expect(fmt("mobile.pair.phone.expires", { m: 3, ss: "07" })).toBe("Code expires in 3:07");
    expect(fmt("mobile.pair.paired", { name: "Pixel 8" })).toBe("Added Pixel 8");
    expect(fmt("mobile.pair.failed", { error: "relay" })).toBe("Couldn't pair the device: relay");
    expect(fmt("mobile.pair.computer.desc", { relayArg: "" })).not.toContain("{");
    for (const k of KEYS) expect(fmt(k, { m: 1, ss: "00", name: "n", error: "e", relayArg: "" }), k).not.toMatch(/\{\w+\}/);
  });
});
