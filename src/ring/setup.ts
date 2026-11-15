import { fmtc, type CopyKey } from "./copy";
import { countdown, secondsLeft } from "./pairing";
import type { JoinEvent, JoinStarted, RingStatus } from "./types";

// The setup screen (first launch, Settings → Ring → Set up your Ring, a removed machine's
// Join again, old Ring data), the pure part: Create, the code screen (eight words, no
// confirmation here), its outcome. The words live only in the `code` state.

export type EndReason = "expired" | "cancelled" | "failed";

export type SetupState =
  | { state: "choices" }
  | { state: "creating" }
  | { state: "created" }
  | { state: "createFailed"; code: string }
  | { state: "starting"; operation: string }
  | { state: "code"; operation: string; words: string; expiresAt: number }
  | { state: "completing"; operation: string }
  | { state: "installed" }
  | { state: "finishing"; operation?: string }
  // `operation`: ended locally (countdown), still tracked until the backend's result.
  | { state: "ended"; reason: EndReason; code?: string; operation?: string }
  // The code screen was left (Cancel, another tab, hidden, closed): shown as the choices,
  // but a late completing/installed/finishing for that operation still shows.
  | { state: "left"; operation: string };

export type SetupAction =
  | { type: "create" }
  | { type: "created" }
  | { type: "createFailed"; error: string }
  | { type: "start"; operation: string }
  | { type: "started"; started: JoinStarted }
  | { type: "startFailed"; operation: string; error: string }
  | { type: "event"; event: JoinEvent }
  | { type: "leave" }
  // The countdown's clock.
  | { type: "tick"; nowMs: number }
  // Back to the choices (Cancel on an outcome, a new removal).
  | { type: "reset" }
  // Left before anything was sent (a registration wait), or a Create whose reply no longer
  // matters: back to the choices.
  | { type: "abort" }
  // `ring:status` went from `finishing` to `none`: the join did not complete.
  | { type: "notAdded" };

export const SETUP_CHOICES: SetupState = { state: "choices" };

// The `code` of a `code: detail` error.
export function errorCode(error: string | undefined): string {
  const m = /^([a-z_]+):/.exec(error ?? "");
  return m ? m[1] : "other";
}

// The operation the UI owns right now (Cancel ends it in the backend).
export function liveOperation(s: SetupState): string | null {
  return s.state === "starting" || s.state === "code" ? s.operation : null;
}

// The operation whose events still matter.
export function trackedOperation(s: SetupState): string | null {
  switch (s.state) {
    case "starting": case "code": case "completing": case "left": return s.operation;
    case "finishing": case "ended": return s.operation ?? null;
    default: return null;
  }
}

// Whether Create or Show words may start now (New words right after the local expiry too: a
// late result of the old words still shows through `ring:status`).
export function canStart(s: SetupState): boolean {
  return s.state === "choices" || s.state === "left" || s.state === "createFailed" || s.state === "ended";
}

function fromEvent(s: SetupState, e: JoinEvent): SetupState {
  switch (e.state) {
    case "waiting": return s;
    case "completing": return { state: "completing", operation: e.operation };
    case "installed": return { state: "installed" };
    case "finishing": return { state: "finishing", operation: e.operation };
    default:
      // A definitive end. After a UI cancel the choices stay; after the local countdown the
      // expiry stays (now with New words); otherwise it is the outcome.
      if (s.state === "left") return SETUP_CHOICES;
      if (s.state === "ended" && s.operation) return { state: "ended", reason: s.reason };
      if (e.state === "failed") return { state: "ended", reason: "failed", code: errorCode(e.error) };
      return { state: "ended", reason: e.state };
  }
}

export function setupReducer(s: SetupState, a: SetupAction): SetupState {
  switch (a.type) {
    case "create":
      return canStart(s) ? { state: "creating" } : s;
    case "created":
      return s.state === "creating" ? { state: "created" } : s;
    case "createFailed": {
      if (s.state !== "creating") return s;
      const code = errorCode(a.error);
      return code === "finishing" ? { state: "finishing" } : { state: "createFailed", code };
    }
    case "start":
      return canStart(s) ? { state: "starting", operation: a.operation } : s;
    case "started":
      return s.state === "starting" && s.operation === a.started.operation
        ? { state: "code", operation: a.started.operation, words: a.started.words, expiresAt: a.started.expiresAt }
        : s;
    case "startFailed": {
      if (s.state === "left" && s.operation === a.operation) return SETUP_CHOICES;
      if (s.state !== "starting" || s.operation !== a.operation) return s;
      const code = errorCode(a.error);
      if (code === "finishing") return { state: "finishing" };
      if (code === "cancelled") return { state: "ended", reason: "cancelled" };
      return { state: "ended", reason: "failed", code };
    }
    case "event":
      // CLI-started offers belong to their terminal; another operation's events are stale.
      if (a.event.origin !== "gui" || trackedOperation(s) !== a.event.operation) return s;
      return fromEvent(s, a.event);
    case "leave": {
      const op = liveOperation(s);
      return op ? { state: "left", operation: op } : s;
    }
    case "tick":
      return s.state === "code" && secondsLeft(s.expiresAt, a.nowMs) === 0 ? { state: "ended", reason: "expired", operation: s.operation } : s;
    case "reset":
      return liveOperation(s) || s.state === "creating" || s.state === "completing" ? s : SETUP_CHOICES;
    case "abort":
      return s.state === "creating" || s.state === "starting" ? SETUP_CHOICES : s;
    case "notAdded":
      return { state: "ended", reason: "failed", code: "not_added" };
  }
}

// What the screen shows: `ring:status` is authoritative (U3). `finishing` shows Finishing…;
// `member` shows done.
export type SetupView =
  | { kind: "choices"; error: CopyKey | null; retry: boolean }
  | { kind: "progress"; text: CopyKey }
  | { kind: "code"; words: string; expiresAt: number }
  | { kind: "joining" }
  | { kind: "completing" }
  | { kind: "finishing"; offline: boolean }
  | { kind: "done"; text: CopyKey }
  | { kind: "ended"; text: CopyKey; action: "new" | "retry" | null };

const CREATE_ERRORS: Record<string, CopyKey> = {
  offline: "create.offline",
  busy: "create.busy",
  already_member: "create.alreadyMember",
  other_window: "create.otherWindow",
  not_registered: "create.notRegistered",
  invalid_relay: "create.invalidRelay",
};

const JOIN_ERRORS: Record<string, CopyKey> = {
  offline: "join.offline",
  busy: "join.busy",
  already_member: "join.alreadyMember",
  other_window: "join.otherWindow",
  not_registered: "join.notRegistered",
  invalid_relay: "join.invalidRelay",
  // COPY-GAP: no Desktop text for "Finishing ended and this machine is not in a Ring"; the
  // generic join failure is the nearest approved line.
  not_added: "join.failed",
};

// Errors a Retry can fix (the others need something else first).
const RETRYABLE = new Set(["offline", "busy", "not_registered", "other", "cancelled", "unavailable", "not_added"]);

export function createErrorKey(code: string): CopyKey {
  return CREATE_ERRORS[code] ?? "create.failed";
}

export function joinErrorKey(code: string): CopyKey {
  return JOIN_ERRORS[code] ?? "join.failed";
}

export function setupView(s: SetupState, status: Pick<RingStatus, "membership" | "finishingOffline"> | null): SetupView {
  if (status?.membership === "finishing") return { kind: "finishing", offline: status.finishingOffline };
  if (status?.membership === "member" || s.state === "created" || s.state === "installed") {
    return { kind: "done", text: s.state === "created" || s.state === "creating" ? "create.done" : "join.done" };
  }
  switch (s.state) {
    case "choices": case "left": return { kind: "choices", error: null, retry: false };
    case "creating": return { kind: "progress", text: "create.progress" };
    case "createFailed": return { kind: "choices", error: createErrorKey(s.code), retry: RETRYABLE.has(s.code) || !(s.code in CREATE_ERRORS) };
    case "starting": return { kind: "joining" };
    case "code": return { kind: "code", words: s.words, expiresAt: s.expiresAt };
    case "completing": return { kind: "completing" };
    // Finishing reported by an event or a start error, before `ring:status` says so.
    case "finishing": return { kind: "finishing", offline: status?.finishingOffline ?? false };
    case "ended":
      if (s.reason === "expired") return { kind: "ended", text: "join.expired", action: "new" };
      if (s.reason === "cancelled") return { kind: "ended", text: "join.cancelled", action: "new" };
      return { kind: "ended", text: joinErrorKey(s.code ?? "other"), action: RETRYABLE.has(s.code ?? "other") || !((s.code ?? "") in JOIN_ERRORS) ? "retry" : null };
  }
}

// The words in two rows of four.
export function wordGroups(words: string): [string[], string[]] {
  const w = words.split(/[\s-]+/).filter(Boolean);
  return [w.slice(0, 4), w.slice(4, 8)];
}

export function countdownLine(expiresAt: number, nowMs: number): string {
  return fmtc("join.countdown", countdown(expiresAt, nowMs));
}
