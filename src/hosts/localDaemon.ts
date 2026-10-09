import type { LocalHostInfo, LocalPersistentInfo } from "./localHost";
import { isUsableStatus } from "./registry";
import { fmt } from "./strings";
import type { HostStatus, TerminalInfo } from "./types";

// Settings → Hosts → This computer: "Keep terminals running after quit" (a Persistent Daemon
// for the Local Host, #25). Pure logic for LocalDaemonSettings; the user never sees "daemon".

// Shown only where the backend offers it: Linux and macOS, local Tabs in a Daemon.
export function persistentVisible(info: LocalHostInfo | null | undefined): info is LocalHostInfo & { persistent: LocalPersistentInfo } {
  return !!info && info.mode === "daemon" && !!info.persistent?.supported;
}

export interface Confirm {
  title: string;
  body: string;
  confirm: string;
}

// Switching restarts every local Terminal, so it is confirmed; with none there is nothing to
// confirm (null).
export function switchConfirm(target: boolean, n: number): Confirm | null {
  if (n === 0) return null;
  return target
    ? { title: fmt("local.persistent.onTitle"), body: fmt("local.persistent.onBody", { n }), confirm: fmt("local.persistent.onConfirm") }
    : { title: fmt("local.persistent.offTitle"), body: fmt("local.persistent.offBody", { n }), confirm: fmt("local.persistent.offConfirm") };
}

export function upgradeConfirm(n: number): Confirm {
  return { title: fmt("hosts.upgrade.confirmTitle"), body: fmt("hosts.upgrade.confirmBody", { host: "this computer", n }), confirm: fmt("hosts.upgrade.confirm") };
}

// `local_daemon_set_persistent` rejects with `other-app`, `timeout`, `confirm-again:<n>` (the
// count changed: ask again, see confirmAgainCount), `failed:<message>` (or anything else,
// shown as a failure).
export function switchErrorText(code: string, log: string | null | undefined): string {
  if (code === "other-app") return fmt("local.persistent.err.otherApp");
  if (code === "timeout") return fmt("local.persistent.err.timeout", { log: log || "the xshell log" });
  return fmt("local.persistent.err.failed", { error: code.startsWith("failed:") ? code.slice("failed:".length) : code });
}

// The Terminal count of a `confirm-again:<n>` rejection, else null.
export function confirmAgainCount(code: string): number | null {
  const m = /^confirm-again(?::(\d+))?$/.exec(code);
  return m ? Number(m[1] ?? 1) : null;
}

export function errorCode(e: unknown): string {
  if (typeof e === "string") return e;
  const m = (e as { message?: unknown } | null)?.message;
  return typeof m === "string" ? m : String(e);
}

// "Terminal setup 1.5.0 · keeps running", once connected.
export function localDaemonLine(status: HostStatus | undefined, running: LocalPersistentInfo["running"] | undefined): string | null {
  if (!status?.daemonVersion || !running) return null;
  return fmt("local.daemon.line", { v: status.daemonVersion, mode: fmt(running === "persistent" ? "local.mode.persistent" : "local.mode.guiBound") });
}

// What the section is doing. Switch and upgrade exclude each other.
export type Phase =
  | { kind: "idle" }
  | { kind: "confirm-switch"; target: boolean; terminals: number; confirm: Confirm }
  | { kind: "switching"; target: boolean; terminals: number }
  | { kind: "confirm-upgrade"; confirm: Confirm }
  | { kind: "upgrading" };

export interface State {
  phase: Phase;
  error: string | null;
}

export type Action =
  | { type: "toggle"; target: boolean; terminals: number }
  | { type: "confirm" }
  | { type: "cancel" }
  | { type: "switched" }
  | { type: "failed"; error: string }
  // The backend found more Terminals than were confirmed: ask again with that count.
  | { type: "confirm-again"; terminals: number }
  | { type: "upgrade"; terminals: number }
  | { type: "upgraded" };

export const initialState: State = { phase: { kind: "idle" }, error: null };

export function reduce(s: State, a: Action): State {
  switch (a.type) {
    case "toggle": {
      if (s.phase.kind !== "idle") return s;
      const confirm = switchConfirm(a.target, a.terminals);
      return { error: null, phase: confirm ? { kind: "confirm-switch", target: a.target, terminals: a.terminals, confirm } : { kind: "switching", target: a.target, terminals: a.terminals } };
    }
    case "confirm-again": {
      if (s.phase.kind !== "switching") return s;
      const n = Math.max(a.terminals, s.phase.terminals + 1, 1);
      return { error: null, phase: { kind: "confirm-switch", target: s.phase.target, terminals: n, confirm: switchConfirm(s.phase.target, n)! } };
    }
    case "confirm":
      if (s.phase.kind === "confirm-switch") return { ...s, phase: { kind: "switching", target: s.phase.target, terminals: s.phase.terminals } };
      if (s.phase.kind === "confirm-upgrade") return { ...s, phase: { kind: "upgrading" } };
      return s;
    case "cancel":
      return s.phase.kind === "confirm-switch" || s.phase.kind === "confirm-upgrade" ? { ...s, phase: { kind: "idle" } } : s;
    case "switched":
      return s.phase.kind === "switching" ? { error: null, phase: { kind: "idle" } } : s;
    case "failed":
      return s.phase.kind === "switching" || s.phase.kind === "upgrading" ? { error: a.error, phase: { kind: "idle" } } : s;
    case "upgrade":
      if (s.phase.kind !== "idle") return s;
      return { error: null, phase: { kind: "confirm-upgrade", confirm: upgradeConfirm(a.terminals) } };
    case "upgraded":
      return s.phase.kind === "upgrading" ? { ...s, phase: { kind: "idle" } } : s;
  }
}

// A switch to start, with the Terminal count the user confirmed: set when a "switching"
// phase begins.
export const startsSwitch = (prev: State, next: State): { target: boolean; confirmed: number } | null =>
  next.phase.kind === "switching" && prev.phase.kind !== "switching" ? { target: next.phase.target, confirmed: next.phase.terminals } : null;
export const startsUpgrade = (prev: State, next: State): boolean =>
  next.phase.kind === "upgrading" && prev.phase.kind !== "upgrading";

export interface Controls {
  // The stored value: kept while busy or disabled, never shown as off just for being disabled.
  checked: boolean;
  disabled: boolean;
  // "Switching…" while a switch runs, and while no usable connection gives the Terminal list.
  hint: string | null;
  showUpgrade: boolean;
  upgradeDisabled: boolean;
  upgradeBusy: boolean;
}

export function controls(s: State, p: LocalPersistentInfo, status: HostStatus | undefined, live: TerminalInfo[] | null | undefined): Controls {
  const hostUpgrading = status?.phase === "upgrading";
  // The list counts only from a usable connection: it was sent before that connection was
  // reported usable, so it is the current one's. The backend checks it again anyway.
  const unknown = live == null || !isUsableStatus(status);
  const busy = s.phase.kind !== "idle";
  const upgradeBusy = s.phase.kind === "upgrading" || hostUpgrading;
  const canUpgrade = p.enabled && (status?.status === "upgrade-pending" || (status?.status === "incompatible" && status.incompatibleReason === "daemon-older"));
  return {
    checked: p.enabled,
    disabled: busy || unknown || hostUpgrading,
    hint: s.phase.kind === "switching" || (unknown && !busy) ? fmt("local.persistent.switching") : null,
    showUpgrade: canUpgrade || upgradeBusy,
    upgradeDisabled: busy || hostUpgrading,
    upgradeBusy,
  };
}

// The `mounted` flag's effect: set on setup, cleared on cleanup, so StrictMode's
// setup → cleanup → setup leaves it set.
export function trackMounted(ref: { current: boolean }): () => void {
  ref.current = true;
  return () => { ref.current = false; };
}
