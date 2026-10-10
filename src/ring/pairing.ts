import { fmt, type StringKey } from "./strings";
import type { PairingEvent, PhoneStart, Qr, RingStatus } from "./types";

// Settings → Mobile, pairing (#9), the pure part: the countdown, the pair code as typed, the
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

// ── The pair code (`xshelld pair`) ────────────────────────────────────────

// Crockford base32: 0-9 and A-Z without I, L, O and U.
const CODE = /^[0-9A-HJKMNP-TV-Z]{16}$/;
export const CODE_LEN = 16;

// The code as the Desktop sends it: case, hyphens and spaces ignored, O→0, I/L→1. Other
// characters stay, so the code reads as invalid.
export function normalizeCode(raw: string): string {
  return raw
    .toUpperCase()
    .replace(/[\s-]+/g, "")
    .replace(/O/g, "0")
    .replace(/[IL]/g, "1");
}

export function validCode(raw: string): boolean {
  return CODE.test(normalizeCode(raw));
}

// The field's text: the normalized code in groups of four, XXXX-XXXX-XXXX-XXXX.
export function formatCode(raw: string): string {
  return normalizeCode(raw).replace(/(.{4})(?=.)/g, "$1-");
}

// The inline error under the field: only once the code cannot become valid by typing on.
export function codeError(raw: string): string | null {
  const c = normalizeCode(raw);
  if (c === "" || validCode(c)) return null;
  if (c.length >= CODE_LEN || /[^0-9A-HJKMNP-TV-Z]/.test(c)) return fmt("mobile.pair.computer.invalid");
  return null;
}

// `mobile.pair.computer.desc` with ` --relay <url>` when the Ring is not on the Hosted Relay.
export function computerDesc(s: Pick<RingStatus, "relayUrl" | "hostedRelayUrl">): string {
  const relayArg = s.relayUrl && s.relayUrl !== s.hostedRelayUrl ? ` --relay ${s.relayUrl}` : "";
  return fmt("mobile.pair.computer.desc", { relayArg });
}

// ── Failures ──────────────────────────────────────────────────────────────

// The codes the Desktop reports (`PairingEvent::Failed`, and the start of a command's error).
const CODES = new Set([
  "expired", "used", "role", "duplicate", "full", "publish_failed", "not_found", "crypto", "pin",
  "relay", "timeout", "protocol", "cancelled", "other_window", "not_enabled", "invalid_code",
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
    case "not_found": return "mobile.pair.computer.notFound";
    case "invalid_code": return "mobile.pair.computer.invalid";
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
  | { state: "starting" }
  | { state: "waiting"; offer: PhoneStart }
  | { state: "paired"; name: string }
  | { state: "expired" }
  | { state: "failed"; code: string; error?: string };

export type PhoneAction =
  | { type: "start" }
  | { type: "started"; offer: PhoneStart }
  | { type: "startFailed"; error: string }
  | { type: "event"; event: PairingEvent }
  // The countdown reached zero.
  | { type: "tick"; nowMs: number }
  | { type: "cancel" };

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
      return s.state === "starting" || s.state === "waiting" ? s : { state: "starting" };
    case "started":
      return s.state === "starting" ? { state: "waiting", offer: a.offer } : s;
    case "startFailed":
      return s.state === "starting" ? { state: "failed", code: errorCode(a.error), error: a.error } : s;
    case "event": {
      if (a.event.flow !== "phone" || (s.state !== "starting" && s.state !== "waiting")) return s;
      return ended(a.event) ?? s;
    }
    case "tick":
      return s.state === "waiting" && secondsLeft(s.offer.expiresAt, a.nowMs) === 0 ? { state: "expired" } : s;
    case "cancel":
      return PHONE_IDLE;
  }
}

// Whether the panel offers "New code" (otherwise "Pair a phone", or nothing while it runs).
export function offersNewCode(s: PhoneState): boolean {
  return s.state === "expired" || s.state === "failed";
}

// ── The "Add a computer" section ──────────────────────────────────────────

export type ComputerState =
  | { state: "idle" }
  | { state: "connecting" }
  | { state: "paired"; name: string }
  | { state: "failed"; code: string; error?: string };

export type ComputerAction =
  | { type: "submit" }
  | { type: "submitFailed"; error: string }
  | { type: "event"; event: PairingEvent }
  // The code was edited: a finished attempt's line goes away.
  | { type: "edit" }
  | { type: "cancel" };

export const COMPUTER_IDLE: ComputerState = { state: "idle" };

export function computerReducer(s: ComputerState, a: ComputerAction): ComputerState {
  switch (a.type) {
    case "submit":
      return s.state === "connecting" ? s : { state: "connecting" };
    case "submitFailed":
      return s.state === "connecting" ? { state: "failed", code: errorCode(a.error), error: a.error } : s;
    case "event": {
      if (a.event.flow !== "computer" || s.state !== "connecting") return s;
      switch (a.event.state) {
        case "paired": return { state: "paired", name: a.event.name };
        case "expired": return { state: "failed", code: "expired" };
        case "failed": return a.event.code === "cancelled" ? s : { state: "failed", code: a.event.code };
        default: return s;
      }
    }
    case "edit":
      return s.state === "connecting" ? s : COMPUTER_IDLE;
    case "cancel":
      return COMPUTER_IDLE;
  }
}

export function canSubmit(code: string, s: ComputerState): boolean {
  return s.state !== "connecting" && validCode(code);
}

// The line under the code field, if any.
export function computerLine(s: ComputerState): string | null {
  switch (s.state) {
    case "connecting": return fmt("mobile.pair.computer.connecting");
    case "paired": return pairedLine(s.name);
    case "failed": return failureLine(s.code, s.error);
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
