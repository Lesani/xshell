import { describe, expect, it } from "vitest";
import { setupReducer, setupView, SETUP_CHOICES, wordGroups, type SetupAction, type SetupState } from "./setup";
import type { JoinEvent } from "./types";

const FAR = 2_000_000_000;
const WORDS = "acid-baker-cable-dance-eagle-fable-gamma-habit";
const run = (actions: SetupAction[], from: SetupState = SETUP_CHOICES) => actions.reduce(setupReducer, from);
const ev = (operation: string, state: JoinEvent["state"], over: Partial<JoinEvent> = {}): SetupAction => ({ type: "event", event: { operation, origin: "gui", state, expiresAt: FAR, ...over } });
const code = (op = "A") => run([{ type: "start", operation: op }, { type: "started", started: { operation: op, words: WORDS, expiresAt: FAR } }]);
const none = { membership: "none" as const, finishingOffline: false };

describe("setup reducer", () => {
  it("code shows words 4+4; completing shows Approved. Finishing… and never a consent state", () => {
    const s = code();
    const v = setupView(s, none);
    expect(v).toEqual({ kind: "code", words: WORDS, expiresAt: FAR });
    expect(wordGroups(WORDS)).toEqual([["acid", "baker", "cable", "dance"], ["eagle", "fable", "gamma", "habit"]]);
    const c = setupReducer(s, ev("A", "completing"));
    expect(c.state).toBe("completing");
    expect(setupView(c, none)).toEqual({ kind: "completing" });
    expect(JSON.stringify(c)).not.toContain("acid");
    expect(setupReducer(c, ev("A", "installed"))).toEqual({ state: "installed" });
  });

  it("tick at zero ends expired and offers New words, never automatically", () => {
    const s = code();
    expect(setupReducer(s, { type: "tick", nowMs: (FAR - 5) * 1000 })).toBe(s);
    const e = setupReducer(s, { type: "tick", nowMs: FAR * 1000 });
    expect(e).toEqual({ state: "ended", reason: "expired", operation: "A" });
    expect(setupView(e, none)).toEqual({ kind: "ended", text: "join.expired", action: "new" });
    // Nothing starts by itself; a new start is allowed.
    expect(setupReducer(e, { type: "start", operation: "B" })).toEqual({ state: "starting", operation: "B" });
  });

  it("backend cancelled → join.cancelled; UI cancel → choices but a later completing/installed/finishing for that operation still shows", () => {
    const cancelled = setupReducer(code(), ev("A", "cancelled"));
    expect(setupView(cancelled, none)).toEqual({ kind: "ended", text: "join.cancelled", action: "new" });
    const left = setupReducer(code(), { type: "leave" });
    expect(left).toEqual({ state: "left", operation: "A" });
    expect(setupView(left, none).kind).toBe("choices");
    expect(setupReducer(left, ev("A", "completing")).state).toBe("completing");
    expect(setupReducer(left, ev("A", "installed")).state).toBe("installed");
    expect(setupReducer(left, ev("A", "finishing")).state).toBe("finishing");
    // A definitive end after the UI cancel keeps the choices.
    expect(setupReducer(left, ev("A", "cancelled"))).toEqual(SETUP_CHOICES);
  });

  it("events for another operation or origin cli are ignored", () => {
    const s = code();
    expect(setupReducer(s, ev("B", "installed"))).toBe(s);
    expect(setupReducer(s, ev("A", "installed", { origin: "cli" }))).toBe(s);
  });

  it("start error offline: → join.offline; finishing: → finishing; create offline: → create.offline", () => {
    const offline = run([{ type: "start", operation: "A" }, { type: "startFailed", operation: "A", error: "offline: relay unreachable" }]);
    expect(setupView(offline, none)).toEqual({ kind: "ended", text: "join.offline", action: "retry" });
    const finishing = run([{ type: "start", operation: "A" }, { type: "startFailed", operation: "A", error: "finishing: earlier work" }]);
    expect(setupView(finishing, none).kind).toBe("finishing");
    const create = run([{ type: "create" }, { type: "createFailed", error: "offline: relay" }]);
    expect(setupView(create, none)).toEqual({ kind: "choices", error: "create.offline", retry: true });
    const member = run([{ type: "create" }, { type: "createFailed", error: "already_member: x" }]);
    expect(setupView(member, none)).toEqual({ kind: "choices", error: "create.alreadyMember", retry: false });
    const other = run([{ type: "start", operation: "A" }, { type: "startFailed", operation: "A", error: "other: boom" }]);
    expect(setupView(other, none)).toEqual({ kind: "ended", text: "join.failed", action: "retry" });
  });

  it("a failed event maps its code; no raw code reaches the view", () => {
    const s = setupReducer(code(), ev("A", "failed", { error: "offline: gone" }));
    expect(setupView(s, none)).toEqual({ kind: "ended", text: "join.offline", action: "retry" });
  });

  it("membership member/finishing overrides the local state", () => {
    expect(setupView(code(), { membership: "finishing", finishingOffline: true })).toEqual({ kind: "finishing", offline: true });
    expect(setupView(SETUP_CHOICES, { membership: "member", finishingOffline: false })).toEqual({ kind: "done", text: "join.done" });
    expect(setupView({ state: "creating" }, { membership: "member", finishingOffline: false })).toEqual({ kind: "done", text: "create.done" });
    expect(setupView({ state: "created" }, none)).toEqual({ kind: "done", text: "create.done" });
  });

  it("force (a removal, finishing → none) returns to the choices from every state; a later start works", () => {
    const states: SetupState[] = [code(), { state: "completing", operation: "A" }, { state: "finishing" }, { state: "creating" }, { state: "installed" }, { state: "ended", reason: "failed", code: "offline" }];
    for (const st of states) expect(setupReducer(st, { type: "force" }), st.state).toEqual(SETUP_CHOICES);
    expect(setupReducer(setupReducer({ state: "completing", operation: "A" }, { type: "force" }), { type: "start", operation: "B" })).toEqual({ state: "starting", operation: "B" });
    expect(setupReducer({ state: "starting", operation: "A" }, { type: "abort" })).toEqual(SETUP_CHOICES);
    expect(setupReducer({ state: "creating" }, { type: "abort" })).toEqual(SETUP_CHOICES);
  });

  it("a late start reply after leaving is ignored", () => {
    const left = setupReducer({ state: "starting", operation: "A" }, { type: "leave" });
    expect(setupReducer(left, { type: "started", started: { operation: "A", words: WORDS, expiresAt: FAR } })).toBe(left);
  });
});
