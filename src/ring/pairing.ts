import { fmt, type StringKey } from "./strings";
import type { PairingEvent, PhoneStart, Qr, RingStatus } from "./types";

// Settings → Hosts → Ring, pairing (#9, #87), the pure part: the countdown, the eight words as typed, the
// two panels' state machines and the failure lines.

// ── Countdown ─────────────────────────────────────────────────────────────

// Whole seconds until `expiresAt` (unix seconds), never negative.
export function secondsLeft(expiresAt: number, nowMs: number): number {
  return Math.max(0, Math.ceil(expiresAt - nowMs / 1000));
}

// `{m}:{ss}`, e.g. 9:05.
export function countdown(expiresAt: number, nowMs: number): { m: string; ss: string } {
  const s = secondsLeft(expiresAt, nowMs);
  return { m: String(Math.floor(s / 60)), ss: String(s % 60).padStart(2, "0") };
}

export function expiresLine(expiresAt: number, nowMs: number): string {
  return fmt("mobile.pair.phone.expires", countdown(expiresAt, nowMs));
}

// ── The eight-word Device Code (Add computer) ─────────────────────────────

export const WORD_COUNT = 8;

// The words as the Desktop sends them: lowercase, split on spaces and hyphens, joined by `-`.
export function normalizeWords(raw: string): string {
  return raw.toLowerCase().split(/[\s-]+/).filter(Boolean).join("-");
}

function words(raw: string): string[] {
  return normalizeWords(raw).split("-").filter(Boolean);
}

// Exactly eight words of letters a–z. (Whether each word is in the list is the Desktop's check.)
export function validWords(raw: string): boolean {
  const w = words(raw);
  return w.length === WORD_COUNT && w.every(x => /^[a-z]+$/.test(x));
}

// The inline error under the field: only once the input cannot become valid by typing on.
export function wordsError(raw: string): string | null {
  const w = words(raw);
  if (w.length === 0 || validWords(raw)) return null;
  if (w.length > WORD_COUNT || w.some(x => !/^[a-z]+$/.test(x))) return fmt("mobile.pair.computer.invalid");
  return null;
}

// ── Failures ──────────────────────────────────────────────────────────────

// The codes the Desktop reports (`PairingEvent::Failed`, and the start of a command's error).
const CODES = new Set([
  "expired", "used", "role", "duplicate", "full", "publish_failed", "not_found", "crypto", "pin",
  "relay", "timeout", "protocol", "cancelled", "busy", "other_window", "not_enabled", "invalid_code",
  "not_approver", "reconciliation_required",
]);

// The failure code a command error starts with, or "other".
export function errorCode(error: string): string {
  const m = /^([a-z_]+)\b/.exec(error);
  return m && CODES.has(m[1]) ? m[1] : "other";
}

export function failureKey(code: string): StringKey {
  switch (code) {
    case "role": return "mobile.pair.err.role";
    case "full": return "mobile.pair.err.full";
    case "invalid_code": return "mobile.pair.computer.invalid";
    case "not_approver": return "mobile.pair.err.notApprover";
    case "busy": return "mobile.pair.err.busy";
    case "reconciliation_required": return "mobile.pair.err.reconcile";
    case "expired": return "mobile.pair.expired";
    case "publish_failed": return "mobile.pair.err.publishFailed";
    default: return "mobile.pair.failed";
  }
}

// The line a failure shows. `error` is the command's text, if the failure came from one.
export function failureLine(code: string, error?: string): string {
  return fmt(failureKey(code), { error: error ?? code });
}

export function pairedLine(name: string): string {
  return fmt("mobile.pair.paired", { name });
}

// ── The "Pair a phone" panel ──────────────────────────────────────────────

export type PhoneState =
  | { state: "idle" }
  | { state: "starting"; operation: string }
  | { state: "waiting"; offer: PhoneStart }
  | { state: "paired"; name: string }
  | { state: "expired" }
  | { state: "failed"; code: string; error?: string };

export type PhoneAction =
  | { type: "start"; operation: string }
  | { type: "started"; offer: PhoneStart }
  | { type: "startFailed"; operation: string; error: string }
  | { type: "event"; event: PairingEvent }
  // The countdown reached zero.
  | { type: "tick"; nowMs: number }
  | { type: "cancel"; operation: string };

export const PHONE_IDLE: PhoneState = { state: "idle" };

// The end of the current offer, from the Desktop's events. A `cancelled` failure is the old
// offer that "New code" replaced, so it never ends the new one.
function ended(event: PairingEvent): PhoneState | null {
  switch (event.state) {
    case "paired": return { state: "paired", name: event.name };
    case "expired": return { state: "expired" };
    case "failed": return event.code === "cancelled" ? null : { state: "failed", code: event.code };
    default: return null;
  }
}

export function phoneReducer(s: PhoneState, a: PhoneAction): PhoneState {
  switch (a.type) {
    case "start":
      return s.state === "starting" || s.state === "waiting" ? s : { state: "starting", operation: a.operation };
    case "started":
      return s.state === "starting" && s.operation === a.offer.operation ? { state: "waiting", offer: a.offer } : s;
    case "startFailed":
      return s.state === "starting" && s.operation === a.operation ? { state: "failed", code: errorCode(a.error), error: a.error } : s;
    case "event": {
      if (a.event.flow !== "phone" || (s.state !== "starting" && s.state !== "waiting")) return s;
      const operation = s.state === "starting" ? s.operation : s.offer.operation;
      if (a.event.operation !== operation) return s;
      return ended(a.event) ?? s;
    }
    case "tick":
      return s.state === "waiting" && secondsLeft(s.offer.expiresAt, a.nowMs) === 0 ? { state: "expired" } : s;
    case "cancel":
      return (s.state === "starting" && s.operation === a.operation) || (s.state === "waiting" && s.offer.operation === a.operation) ? PHONE_IDLE : s;
  }
}

// Whether the panel offers "New code" (otherwise "Pair a phone", or nothing while it runs).
export function offersNewCode(s: PhoneState): boolean {
  return s.state === "expired" || s.state === "failed";
}

// ── The "Add computer" section ────────────────────────────────────────────

// One request (`ring_approve_device_code`) and its reply; nothing arrives by event.
export type ComputerState =
  | { state: "idle" }
  | { state: "connecting"; operation: string }
  | { state: "paired"; names: string[] }
  | { state: "failed"; code: string; error?: string };

export type ComputerAction =
  | { type: "submit"; operation: string }
  | { type: "approved"; operation: string; names: string[] }
  | { type: "submitFailed"; operation: string; error: string }
  // The words were edited: a finished attempt's line goes away.
  | { type: "edit" };

export const COMPUTER_IDLE: ComputerState = { state: "idle" };

export function computerReducer(s: ComputerState, a: ComputerAction): ComputerState {
  switch (a.type) {
    case "submit":
      return s.state === "connecting" ? s : { state: "connecting", operation: a.operation };
    case "approved":
      return s.state === "connecting" && s.operation === a.operation ? { state: "paired", names: a.names } : s;
    case "submitFailed":
      return s.state === "connecting" && s.operation === a.operation ? { state: "failed", code: errorCode(a.error), error: a.error } : s;
    case "edit":
      return s.state === "connecting" ? s : COMPUTER_IDLE;
  }
}

export function canSubmit(raw: string, s: ComputerState): boolean {
  return s.state !== "connecting" && validWords(raw);
}

// The backend's text after its `code: ` prefix.
function detail(error: string | undefined, code: string): string {
  if (!error) return code;
  const m = /^[a-z_]+: (.*)$/s.exec(error);
  return m ? m[1] : error;
}

// The line under the words field, if any.
export function computerLine(s: ComputerState): string | null {
  switch (s.state) {
    case "connecting": return fmt("mobile.pair.computer.connecting");
    case "paired": return fmt("mobile.pair.computer.added", { names: s.names.join(", ") });
    case "failed": return fmt(failureKey(s.code), { error: detail(s.error, s.code) });
    default: return null;
  }
}

// ── The QR code as SVG rectangles ─────────────────────────────────────────

export interface QrRect { x: number; y: number; w: number }

// One rectangle per horizontal run of dark modules (row `y`, from `x`, `w` wide).
export function qrRects(qr: Qr): QrRect[] {
  const out: QrRect[] = [];
  qr.rows.forEach((row, y) => {
    let x = 0;
    while (x < row.length) {
      if (row[x] !== "1") { x++; continue; }
      const start = x;
      while (x < row.length && row[x] === "1") x++;
      out.push({ x: start, y, w: x - start });
    }
  });
  return out;
}
