import { describe, expect, it } from "vitest";
import { cliCardsFrom, cliLine, cliReducer, consentBody, createFailedLine, JOIN_IDLE, joinBusy, joinOperation, joinReducer, offersNewCode, outcomeLine, showsJoinPanel, trackedOperation, wordGroups, type JoinAction, type JoinUi } from "./join";
import { fmt, S } from "./strings";
import type { JoinEvent, JoinPreview } from "./types";

const WORDS = "acid-baker-cable-dance-eagle-fable-gamma-habit";
const FAR = Math.floor(Date.now() / 1000) + 600;
const preview: JoinPreview = { ringId: "abcdefgh1234", ringShort: "abcdefgh", approver: "tower", computer: "laptop", expiresAt: FAR };
const run = (s: JoinUi, ...as: JoinAction[]) => as.reduce(joinReducer, s);
const ev = (operation: string, state: JoinEvent["state"], over: Partial<JoinEvent> = {}): JoinEvent => ({ operation, origin: "gui", state, expiresAt: FAR, ...over });
const code = (op = "A") => run(JOIN_IDLE, { type: "start", operation: op }, { type: "started", started: { operation: op, words: WORDS, expiresAt: FAR } });

describe("Join ring code screen", () => {
  it("groups the eight words 4+4", () => {
    expect(wordGroups(WORDS)).toEqual([["acid", "baker", "cable", "dance"], ["eagle", "fable", "gamma", "habit"]]);
    expect(wordGroups("acid baker cable dance eagle fable gamma habit")[1]).toEqual(["eagle", "fable", "gamma", "habit"]);
  });

  it("shows the words once started, and they are dropped at consent", () => {
    const c = code();
    expect(c).toEqual({ state: "code", operation: "A", words: WORDS, expiresAt: FAR });
    expect(joinBusy(c)).toBe(true);
    expect(showsJoinPanel(c)).toBe(true);
    const consent = run(c, { type: "event", event: ev("A", "awaitingConfirmation", { preview }) });
    expect(consent).toEqual({ state: "consent", operation: "A", preview, expiresAt: FAR });
    expect(JSON.stringify(consent)).not.toContain("acid");
  });

  it("ignores stale operations and CLI-started events", () => {
    const c = code("B");
    expect(run(c, { type: "event", event: ev("A", "expired") })).toBe(c);
    expect(run(c, { type: "event", event: ev("B", "expired", { origin: "cli" }) })).toBe(c);
    expect(run(c, { type: "started", started: { operation: "A", words: WORDS, expiresAt: FAR } })).toBe(c);
    expect(run(c, { type: "cancel", operation: "A" })).toBe(c);
    const starting = run(JOIN_IDLE, { type: "start", operation: "B" });
    expect(run(starting, { type: "startFailed", operation: "A", error: "relay" })).toBe(starting);
  });

  it("consent → Confirm → completing → installed", () => {
    let s = run(code(), { type: "event", event: ev("A", "awaitingConfirmation", { preview }) });
    s = run(s, { type: "confirm", operation: "A" });
    expect(s.state).toBe("confirming");
    // A repeated awaitingConfirmation does not re-enable the buttons.
    expect(run(s, { type: "event", event: ev("A", "awaitingConfirmation", { preview }) }).state).toBe("confirming");
    s = run(s, { type: "event", event: ev("A", "completing") });
    expect(s).toEqual({ state: "completing", operation: "A" });
    expect(offersNewCode(s)).toBe(false);
    s = run(s, { type: "event", event: ev("A", "installed") });
    expect(s).toEqual({ state: "installed" });
    expect(outcomeLine(s)?.text).toBe(S["ring.joined"]);
    expect(consentBody(preview, "—")).toBe("“tower” has requested to add this computer, “laptop”, to Ring abcdefgh. Confirming adds both its Desktop and its Host and gives the Ring access to this computer’s Host.");
  });

  it("expiry offers Generate new code, never automatically", () => {
    const byEvent = run(code(), { type: "event", event: ev("A", "expired") });
    expect(byEvent).toEqual({ state: "ended", reason: "expired" });
    expect(offersNewCode(byEvent)).toBe(true);
    expect(outcomeLine(byEvent)?.text).toBe(S["ring.expired"]);
    // The local countdown ends the screen but keeps tracking until the backend's result.
    const byClock = run(code(), { type: "tick", nowMs: (FAR + 1) * 1000 });
    expect(byClock).toEqual({ state: "ended", reason: "expired", operation: "A" });
    expect(offersNewCode(byClock)).toBe(false);
    expect(run(byClock, { type: "start", operation: "C" })).toBe(byClock);
    expect(run(code(), { type: "tick", nowMs: Date.now() }).state).toBe("code");
    const final = run(byClock, { type: "event", event: ev("A", "expired") });
    expect(final).toEqual({ state: "ended", reason: "expired" });
    expect(offersNewCode(final)).toBe(true);
    // A late reconciliation after the local expiry is still shown.
    expect(run(byClock, { type: "event", event: ev("A", "reconciliationRequired") }).state).toBe("reconcile");
    // Generate new code starts a fresh operation.
    expect(run(final, { type: "start", operation: "C" })).toEqual({ state: "starting", operation: "C" });
  });

  it("decline → declined line and a fresh code is offered", () => {
    const consent = run(code(), { type: "event", event: ev("A", "awaitingConfirmation", { preview }) });
    const declined = run(consent, { type: "declined", operation: "A" });
    expect(declined).toEqual({ state: "ended", reason: "declined" });
    expect(outcomeLine(declined)?.text).toBe(S["ring.declined"]);
    expect(offersNewCode(declined)).toBe(true);
    expect(run(consent, { type: "event", event: ev("A", "declined") })).toEqual(declined);
  });

  it("reconciliation offers no retry", () => {
    const r = run(code(), { type: "event", event: ev("A", "reconciliationRequired", { error: "reconciliation_required: x" }) });
    expect(r.state).toBe("reconcile");
    expect(offersNewCode(r)).toBe(false);
    expect(joinOperation(r)).toBeNull();
    expect(outcomeLine(r)?.text).toBe(S["ring.reconciliationRequired"]);
    // A start can't leave reconciliation either.
    expect(run(r, { type: "start", operation: "Z" })).toBe(r);
  });

  it("a start refused with reconciliation_required goes to reconcile; a cancelled start to idle", () => {
    const starting = run(JOIN_IDLE, { type: "start", operation: "A" });
    expect(run(starting, { type: "startFailed", operation: "A", error: "reconciliation_required: an earlier Join is unresolved" }).state).toBe("reconcile");
    expect(run(starting, { type: "startFailed", operation: "A", error: "cancelled: Join cancelled before its code was shown" })).toEqual(JOIN_IDLE);
    const failed = run(starting, { type: "startFailed", operation: "A", error: "relay: down" });
    expect(failed).toEqual({ state: "ended", reason: "failed", error: "relay: down" });
    expect(outcomeLine(failed)?.text).toBe(fmt("ring.joinFailed", { error: "relay: down" }));
  });

  it("cancel leaves the screen but tracks the operation until its definitive result", () => {
    const left = run(code(), { type: "cancel", operation: "A" });
    expect(left).toEqual({ state: "cancelled", operation: "A" });
    expect(showsJoinPanel(left)).toBe(false);
    expect(joinBusy(left)).toBe(true);
    expect(joinOperation(left)).toBeNull();
    expect(trackedOperation(left)).toBe("A");
    expect(run(left, { type: "event", event: ev("A", "cancelled") })).toEqual(JOIN_IDLE);
    expect(run(left, { type: "event", event: ev("A", "failed", { error: "cancelled" }) })).toEqual(JOIN_IDLE);
    expect(run(code(), { type: "event", event: ev("A", "cancelled") })).toEqual(JOIN_IDLE);
    // A start cancelled before its reply never registered: its refusal ends the tracking.
    const early = run(JOIN_IDLE, { type: "start", operation: "B" }, { type: "cancel", operation: "B" });
    expect(run(early, { type: "startFailed", operation: "B", error: "cancelled: x" })).toEqual(JOIN_IDLE);
  });

  it("cancel after Confirm still shows a later reconciliationRequired", () => {
    let s = run(code(), { type: "event", event: ev("A", "awaitingConfirmation", { preview }) }, { type: "confirm", operation: "A" });
    s = run(s, { type: "cancel", operation: "A" });
    expect(s).toEqual({ state: "cancelled", operation: "A" });
    expect(run(s, { type: "event", event: ev("A", "completing") })).toBe(s);
    const r = run(s, { type: "event", event: ev("A", "reconciliationRequired", { error: "reconciliation_required: x" }) });
    expect(r.state).toBe("reconcile");
    expect(offersNewCode(r)).toBe(false);
    expect(run(s, { type: "event", event: ev("A", "installed") })).toEqual({ state: "installed" });
  });

  it("Create ring: creating, created, and its failure lines", () => {
    const c = run(JOIN_IDLE, { type: "create" });
    expect(c).toEqual({ state: "creating" });
    expect(run(c, { type: "start", operation: "A" })).toBe(c);
    expect(outcomeLine(run(c, { type: "created" }))?.text).toBe(S["ring.created"]);
    expect(run(c, { type: "createFailed", error: "reconciliation_required: an earlier Join is unresolved" }).state).toBe("reconcile");
    expect(createFailedLine("not_registered: x")).toBe(S["ring.hostNotReady"]);
    expect(createFailedLine("unavailable: platform")).toBe(S["ring.unsupported"]);
    expect(createFailedLine("relay: down")).toBe("Couldn’t create the Ring: relay: down");
  });
});

describe("pairing window cards (xshelld pair on this computer)", () => {
  it("keep the latest event per CLI operation, never words, and ignore GUI events", () => {
    let cards = cliReducer({}, ev("X", "waiting", { origin: "cli" }));
    cards = cliReducer(cards, ev("G", "waiting"));
    cards = cliReducer(cards, ev("X", "awaitingConfirmation", { origin: "cli", preview }));
    expect(Object.keys(cards)).toEqual(["X"]);
    expect(cards.X.state).toBe("awaitingConfirmation");
    expect(JSON.stringify(cards)).not.toMatch(/words/);
    expect(cliCardsFrom([ev("Y", "waiting", { origin: "cli" })]).Y.state).toBe("waiting");
  });

  it("say where the words are while waiting and reuse the outcome copy", () => {
    expect(cliLine(ev("X", "waiting", { origin: "cli" }))?.text).toBe(S["ring.cliWaiting"]);
    expect(cliLine(ev("X", "awaitingConfirmation", { origin: "cli" }))).toBeNull();
    expect(cliLine(ev("X", "installed", { origin: "cli" }))?.text).toBe(S["ring.joined"]);
    expect(cliLine(ev("X", "reconciliationRequired", { origin: "cli" }))?.text).toBe(S["ring.reconciliationRequired"]);
  });
});
