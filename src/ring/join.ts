import { fmt } from "./strings";
import { countdown, secondsLeft } from "./pairing";
import type { JoinEvent, JoinPreview, JoinStarted } from "./types";

// Settings → Hosts → Ring, Computer adoption (#87), the pure part: Create ring, the Join ring
// code screen and its local consent, and the pairing window cards for offers that
// `xshelld pair` started on this computer. The Device Code words live only in the `code`
// state: consent and every later state drop them.

export type EndReason = "expired" | "declined" | "failed";

export type JoinUi =
  | { state: "idle" }
  | { state: "creating" }
  | { state: "created" }
  | { state: "createFailed"; error: string }
  | { state: "starting"; operation: string }
  | { state: "code"; operation: string; words: string; expiresAt: number }
  | { state: "consent"; operation: string; preview: JoinPreview; expiresAt: number }
  | { state: "confirming"; operation: string; preview: JoinPreview; expiresAt: number }
  | { state: "completing"; operation: string }
  // The screen was left: the backend offer is cancelled, but its operation is tracked until
  // a definitive result (a confirmed Join may still need reconciliation).
  | { state: "cancelled"; operation: string }
  | { state: "installed" }
  // `operation`: ended by the local countdown, still tracked until the backend's result.
  | { state: "ended"; reason: EndReason; error?: string; operation?: string }
  | { state: "reconcile"; error?: string };

export type JoinAction =
  | { type: "create" }
  | { type: "created" }
  | { type: "createFailed"; error: string }
  | { type: "start"; operation: string }
  | { type: "started"; started: JoinStarted }
  | { type: "startFailed"; operation: string; error: string }
  | { type: "event"; event: JoinEvent }
  | { type: "confirm"; operation: string }
  | { type: "confirmFailed"; operation: string; error: string }
  | { type: "declined"; operation: string }
  | { type: "cancel"; operation: string }
  // The countdown reached zero.
  | { type: "tick"; nowMs: number };

export const JOIN_IDLE: JoinUi = { state: "idle" };

// The operation the UI currently owns, if any.
export function joinOperation(s: JoinUi): string | null {
  switch (s.state) {
    case "starting": case "code": case "consent": case "confirming": case "completing": return s.operation;
    default: return null;
  }
}

// The operation whose events still matter: the active one, or one left/expired locally whose
// backend result has not arrived.
export function trackedOperation(s: JoinUi): string | null {
  if (s.state === "cancelled") return s.operation;
  if (s.state === "ended") return s.operation ?? null;
  return joinOperation(s);
}

// Whether a Join or Create runs (nothing else may start).
export function joinBusy(s: JoinUi): boolean {
  return s.state === "creating" || s.state === "cancelled" || joinOperation(s) !== null;
}

// Whether the Join screen (code, consent or an outcome) replaces Create ring / Join ring.
export function showsJoinPanel(s: JoinUi): boolean {
  return joinOperation(s) !== null || s.state === "ended" || s.state === "reconcile";
}

function canStart(s: JoinUi): boolean {
  return s.state === "idle" || (s.state === "ended" && !s.operation) || s.state === "createFailed" || s.state === "created";
}

// A left or locally expired operation: only a definitive backend result changes it.
function fromTrackedEvent(s: Extract<JoinUi, { state: "cancelled" | "ended" }>, e: JoinEvent): JoinUi {
  switch (e.state) {
    case "waiting": case "awaitingConfirmation": case "completing": return s;
    case "installed": return { state: "installed" };
    case "reconciliationRequired": return { state: "reconcile", error: e.error };
    default: return s.state === "cancelled" ? JOIN_IDLE : { state: "ended", reason: s.reason, error: s.error };
  }
}

function fromEvent(s: JoinUi, e: JoinEvent): JoinUi {
  switch (e.state) {
    case "waiting": return s;
    case "awaitingConfirmation":
      if (s.state === "confirming" || s.state === "completing") return s;
      return e.preview ? { state: "consent", operation: e.operation, preview: e.preview, expiresAt: e.preview.expiresAt } : s;
    case "completing": return { state: "completing", operation: e.operation };
    case "installed": return { state: "installed" };
    case "declined": return { state: "ended", reason: "declined" };
    case "expired": return { state: "ended", reason: "expired" };
    case "failed": return { state: "ended", reason: "failed", error: e.error };
    case "reconciliationRequired": return { state: "reconcile", error: e.error };
    case "cancelled": return JOIN_IDLE;
  }
}

export function joinReducer(s: JoinUi, a: JoinAction): JoinUi {
  switch (a.type) {
    case "create":
      return canStart(s) ? { state: "creating" } : s;
    case "created":
      return s.state === "creating" ? { state: "created" } : s;
    case "createFailed":
      if (s.state !== "creating") return s;
      // Durable uncertainty is never a plain failure: no new Create or Join.
      return a.error.startsWith("reconciliation_required") ? { state: "reconcile", error: a.error } : { state: "createFailed", error: a.error };
    case "start":
      return canStart(s) ? { state: "starting", operation: a.operation } : s;
    case "started":
      return s.state === "starting" && s.operation === a.started.operation
        ? { state: "code", operation: a.started.operation, words: a.started.words, expiresAt: a.started.expiresAt }
        : s;
    case "startFailed": {
      if (s.state === "cancelled" && s.operation === a.operation) return a.error.startsWith("reconciliation_required") ? { state: "reconcile", error: a.error } : JOIN_IDLE;
      if (s.state !== "starting" || s.operation !== a.operation) return s;
      if (a.error.startsWith("reconciliation_required")) return { state: "reconcile", error: a.error };
      if (a.error.startsWith("cancelled")) return JOIN_IDLE;
      return { state: "ended", reason: "failed", error: a.error };
    }
    case "event": {
      // CLI-started offers have their own cards; another operation's events are stale.
      if (a.event.origin !== "gui" || trackedOperation(s) !== a.event.operation) return s;
      if (s.state === "cancelled" || s.state === "ended") return fromTrackedEvent(s, a.event);
      return fromEvent(s, a.event);
    }
    case "confirm":
      return s.state === "consent" && s.operation === a.operation ? { ...s, state: "confirming" } : s;
    case "confirmFailed":
      return (s.state === "confirming" || s.state === "consent") && s.operation === a.operation ? { state: "ended", reason: "failed", error: a.error } : s;
    case "declined":
      return (s.state === "consent" || s.state === "confirming") && s.operation === a.operation ? { state: "ended", reason: "declined" } : s;
    case "cancel":
      return joinOperation(s) === a.operation ? { state: "cancelled", operation: a.operation } : s;
    case "tick":
      return (s.state === "code" || s.state === "consent") && secondsLeft(s.expiresAt, a.nowMs) === 0 ? { state: "ended", reason: "expired", operation: s.operation } : s;
  }
}

// "Generate new code" only after a definitive outcome; never while completing or reconciling.
export function offersNewCode(s: JoinUi): boolean {
  return s.state === "ended" && !s.operation;
}

// The words in two rows of four.
export function wordGroups(words: string): [string[], string[]] {
  const w = words.split(/[\s-]+/).filter(Boolean);
  return [w.slice(0, 4), w.slice(4, 8)];
}

export function codeExpiresLine(expiresAt: number, nowMs: number): string {
  return fmt("ring.codeExpires", countdown(expiresAt, nowMs));
}

// The consent card's body.
export function consentBody(p: JoinPreview, fallbackApprover: string): string {
  return fmt("ring.consentBody", { approver: p.approver ?? fallbackApprover, computer: p.computer, ring: p.ringShort });
}

// The line an outcome shows, if any.
export function outcomeLine(s: JoinUi): { text: string; error: boolean } | null {
  switch (s.state) {
    case "created": return { text: fmt("ring.created"), error: false };
    case "createFailed": return { text: createFailedLine(s.error), error: true };
    case "installed": return { text: fmt("ring.joined"), error: false };
    case "ended":
      if (s.reason === "declined") return { text: fmt("ring.declined"), error: false };
      if (s.reason === "expired") return { text: fmt("ring.expired"), error: true };
      return { text: fmt("ring.joinFailed", { error: s.error ?? "" }), error: true };
    case "reconcile": return { text: fmt("ring.reconciliationRequired"), error: true };
    default: return null;
  }
}

export function createFailedLine(error: string): string {
  if (error.startsWith("not_registered")) return fmt("ring.hostNotReady");
  if (error.startsWith("unavailable: platform")) return fmt("ring.unsupported");
  return fmt("ring.createFailed", { error });
}

// ── Pairing window cards (offers `xshelld pair` started on this computer) ──

export type CliCards = Record<string, JoinEvent>;

// The latest event per CLI operation. Never carries words: the terminal shows them.
export function cliReducer(cards: CliCards, e: JoinEvent): CliCards {
  if (e.origin !== "cli") return cards;
  return { ...cards, [e.operation]: e };
}

export function cliCardsFrom(events: JoinEvent[]): CliCards {
  return events.reduce(cliReducer, {} as CliCards);
}

// The status line of a CLI card.
export function cliLine(e: JoinEvent): { text: string; error: boolean } | null {
  switch (e.state) {
    case "waiting": return { text: fmt("ring.cliWaiting"), error: false };
    case "awaitingConfirmation": return null;
    case "completing": return { text: fmt("ring.completing"), error: false };
    case "installed": return { text: fmt("ring.joined"), error: false };
    case "declined": return { text: fmt("ring.declined"), error: false };
    case "expired": return { text: fmt("ring.expired"), error: true };
    case "failed": return { text: fmt("ring.joinFailed", { error: e.error ?? "" }), error: true };
    case "reconciliationRequired": return { text: fmt("ring.reconciliationRequired"), error: true };
    case "cancelled": return null;
  }
}
