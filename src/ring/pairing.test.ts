import { describe, expect, it } from "vitest";
import { canSubmit, COMPUTER_IDLE, computerLine, computerReducer, countdown, errorCode, expiresLine, failureKey, failureLine, normalizeWords, offersNewCode, PHONE_IDLE, phoneReducer, qrRects, secondsLeft, validWords, wordsError, type ComputerState, type PhoneState } from "./pairing";
import { fmt, S, type StringKey } from "./strings";
import type { PhoneStart } from "./types";

const HOSTED = "wss://relay.xshell.sh";
const OFFER: PhoneStart = { operation: "test-op", payload: "xsp1.abc", qr: { size: 2, rows: ["10", "01"] }, expiresAt: 1000 };

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

describe("eight-word Device Code", () => {
  const WORDS = "acid baker cable dance eagle fable gamma habit";

  it("normalizeWords lowercases and accepts spaces, hyphens and mixed separators", () => {
    expect(normalizeWords("Acid BAKER cable-Dance  eagle--fable\tgamma habit")).toBe("acid-baker-cable-dance-eagle-fable-gamma-habit");
    expect(normalizeWords("  acid-baker ")).toBe("acid-baker");
    expect(normalizeWords("")).toBe("");
  });

  it("accepts exactly eight words of letters", () => {
    expect(validWords(WORDS)).toBe(true);
    expect(validWords(WORDS.toUpperCase().replace(/ /g, "-"))).toBe(true);
    expect(validWords("acid baker cable dance eagle fable gamma")).toBe(false); // 7
    expect(validWords(`${WORDS} ivory`)).toBe(false); // 9
    expect(validWords("acid baker cable dance eagle fable gamma h4bit")).toBe(false); // digits
    expect(validWords("")).toBe(false);
  });

  it("shows the invalid line only once typing on cannot help", () => {
    expect(wordsError("")).toBeNull();
    expect(wordsError("acid baker")).toBeNull();
    expect(wordsError(WORDS)).toBeNull();
    expect(wordsError("acid b4ker")).toBe(S["mobile.pair.computer.invalid"]);
    expect(wordsError(`${WORDS} ivory`)).toBe(S["mobile.pair.computer.invalid"]);
  });
});

describe("failures", () => {
  it("maps codes to their lines", () => {
    const cases: [string, StringKey][] = [
      ["role", "mobile.pair.err.role"],
      ["full", "mobile.pair.err.full"],
      ["invalid_code", "mobile.pair.computer.invalid"],
      ["not_approver", "mobile.pair.err.notApprover"],
      ["busy", "mobile.pair.err.busy"],
      ["reconciliation_required", "mobile.pair.err.reconcile"],
      ["not_found", "mobile.pair.failed"],
      ["expired", "mobile.pair.expired"],
      ["publish_failed", "mobile.pair.err.publishFailed"],
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
    const w = run(PHONE_IDLE, { type: "start", operation: "test-op" }, { type: "started", offer: OFFER });
    expect(w).toEqual({ state: "waiting", offer: OFFER });
    expect(run(w, { type: "event", event: { operation: "test-op", flow: "phone", state: "waiting" } })).toBe(w);
    expect(run(w, { type: "event", event: { operation: "test-op", flow: "phone", state: "paired", name: "Pixel", role: "mobile" } })).toEqual({ state: "paired", name: "Pixel" });
  });

  it("expires from the event or the countdown, then offers a new code", () => {
    const w = run(PHONE_IDLE, { type: "start", operation: "test-op" }, { type: "started", offer: OFFER });
    expect(run(w, { type: "tick", nowMs: 999_000 })).toBe(w);
    const e = run(w, { type: "tick", nowMs: 1000_000 });
    expect(e).toEqual({ state: "expired" });
    expect(run(w, { type: "event", event: { operation: "test-op", flow: "phone", state: "expired" } })).toEqual({ state: "expired" });
    expect(offersNewCode(e)).toBe(true);
    expect(run(e, { type: "start", operation: "test-op" })).toEqual({ state: "starting", operation: "test-op" });
  });

  it("fails from an event or the command", () => {
    const w = run(PHONE_IDLE, { type: "start", operation: "test-op" }, { type: "started", offer: OFFER });
    expect(run(w, { type: "event", event: { operation: "test-op", flow: "phone", state: "failed", code: "role" } })).toEqual({ state: "failed", code: "role" });
    const f = run(PHONE_IDLE, { type: "start", operation: "test-op" }, { type: "startFailed", operation: "test-op", error: "not_enabled: off" });
    expect(f).toEqual({ state: "failed", code: "not_enabled", error: "not_enabled: off" });
    expect(offersNewCode(f)).toBe(true);
  });

  it("ignores other flows, idle events and the replaced offer's cancel", () => {
    const w = run(PHONE_IDLE, { type: "start", operation: "test-op" }, { type: "started", offer: OFFER });
    expect(run(w, { type: "event", event: { operation: "test-op", flow: "computer", state: "expired" } })).toBe(w);
    expect(run(w, { type: "event", event: { operation: "test-op", flow: "phone", state: "failed", code: "cancelled" } })).toBe(w);
    expect(run(PHONE_IDLE, { type: "event", event: { operation: "test-op", flow: "phone", state: "paired", name: "x", role: "mobile" } })).toBe(PHONE_IDLE);
    expect(run(w, { type: "start", operation: "test-op" })).toBe(w);
    expect(run(w, { type: "cancel", operation: "test-op" })).toEqual(PHONE_IDLE);
  });
});

describe("computer section", () => {
  const run = (s: ComputerState, ...as: Parameters<typeof computerReducer>[1][]) => as.reduce(computerReducer, s);
  const WORDS = "acid baker cable dance eagle fable gamma habit";

  it("connects, then the approved reply adds the computer", () => {
    expect(canSubmit("acid baker", COMPUTER_IDLE)).toBe(false);
    expect(canSubmit(WORDS, COMPUTER_IDLE)).toBe(true);
    const c = run(COMPUTER_IDLE, { type: "submit", operation: "test-op" });
    expect(c).toEqual({ state: "connecting", operation: "test-op" });
    expect(canSubmit(WORDS, c)).toBe(false);
    expect(computerLine(c)).toBe(S["mobile.pair.computer.connecting"]);
    const p = run(c, { type: "approved", operation: "test-op", names: ["tower", "tower host"] });
    expect(p).toEqual({ state: "paired", names: ["tower", "tower host"] });
    expect(computerLine(p)).toBe("Added to the Ring: tower, tower host.");
    // A stale reply of another attempt changes nothing.
    expect(run(c, { type: "approved", operation: "old", names: ["x"] })).toBe(c);
  });

  it("not_approver, busy, reconcile and invalid have their own copy", () => {
    const c = run(COMPUTER_IDLE, { type: "submit", operation: "test-op" });
    const fail = (error: string) => computerLine(run(c, { type: "submitFailed", operation: "test-op", error }));
    expect(fail("not_approver: approval requires enrolled membership")).toBe(S["mobile.pair.err.notApprover"]);
    expect(fail("busy: another adoption operation is active")).toBe(S["mobile.pair.err.busy"]);
    expect(fail("reconciliation_required: x")).toBe(S["mobile.pair.err.reconcile"]);
    expect(fail("invalid_code: invalid pairing code: unknown device word")).toBe(S["mobile.pair.computer.invalid"]);
    expect(fail("other: nobody is waiting with this code")).toBe("Couldn't pair the device: nobody is waiting with this code");
  });

  it("clears a finished attempt on edit, not a running one", () => {
    const c = run(COMPUTER_IDLE, { type: "submit", operation: "test-op" });
    expect(run(c, { type: "edit" })).toBe(c);
    const f = run(c, { type: "submitFailed", operation: "test-op", error: "relay: down" });
    expect(computerLine(f)).toBe("Couldn't pair the device: down");
    expect(run(f, { type: "edit" })).toEqual(COMPUTER_IDLE);
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
    "mobile.pair.computer.connecting", "mobile.pair.computer.invalid", "mobile.pair.computer.added",
    "mobile.pair.err.notApprover", "mobile.pair.err.busy", "mobile.pair.err.reconcile",
    "mobile.pair.err.role", "mobile.pair.err.full", "mobile.pair.err.publishFailed", "mobile.pair.connecting",
    "mobile.pair.needsRelay", "mobile.pair.needsMember",
  ];

  it("has every key", () => {
    for (const k of KEYS) expect(S[k], k).toBeTruthy();
  });

  it("fills the placeholders", () => {
    expect(fmt("mobile.pair.phone.expires", { m: 3, ss: "07" })).toBe("Code expires in 3:07");
    expect(fmt("mobile.pair.paired", { name: "Pixel 8" })).toBe("Added Pixel 8");
    expect(fmt("mobile.pair.failed", { error: "relay" })).toBe("Couldn't pair the device: relay");
    expect(fmt("mobile.pair.computer.desc")).not.toContain("{");
    expect(S["mobile.pair.computer"]).toBeTruthy();
    for (const k of KEYS) expect(fmt(k, { m: 1, ss: "00", name: "n", error: "e", names: "a, b" }), k).not.toMatch(/\{\w+\}/);
  });
});

describe("pairing operation ownership", () => {
  it("late A start/result/event cannot populate or end reissued B", () => {
    const a = phoneReducer(PHONE_IDLE, { type: "start", operation: "A" });
    const cancelled = phoneReducer(a, { type: "cancel", operation: "A" });
    const b = phoneReducer(cancelled, { type: "start", operation: "B" });
    expect(phoneReducer(b, { type: "cancel", operation: "A" })).toBe(b);
    expect(phoneReducer(b, { type: "started", offer: { ...OFFER, operation: "A" } })).toBe(b);
    expect(phoneReducer(b, { type: "startFailed", operation: "A", error: "relay" })).toBe(b);
    expect(phoneReducer(b, { type: "event", event: { flow: "phone", operation: "A", state: "paired", name: "old", role: "mobile" } })).toBe(b);
    expect(phoneReducer(b, { type: "started", offer: { ...OFFER, operation: "B" } }).state).toBe("waiting");
  });
});
