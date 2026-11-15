import { fmtc, type CopyKey } from "./copy";
import type { Connection, MemberView } from "./types";
import type { AvailableHost, HostStatus } from "../hosts/types";

// Settings → Ring → Your devices, the pure part: labels, the presence chip, the version line,
// the Remove dialog and its errors.

// A machine (computer or headless) rather than a phone.
export function isMachine(m: Pick<MemberView, "kind" | "role">): boolean {
  if (m.kind === "mobile") return false;
  if (m.kind) return true;
  return m.role !== "mobile";
}

export function isPhone(m: Pick<MemberView, "kind" | "role">): boolean {
  return !isMachine(m);
}

export function isThisMachine(m: Pick<MemberView, "thisApp" | "thisComputer">): boolean {
  return m.thisApp || m.thisComputer;
}

export function kindKey(m: Pick<MemberView, "kind" | "role">): CopyKey {
  if (isPhone(m)) return "member.phone";
  if (m.kind === "daemon" || (!m.kind && m.role === "daemon")) return "member.headless";
  return "member.machine";
}

// Online when either key of the unit is; no chip for another device while this window has no
// connection of its own (it cannot know).
export function presenceChip(m: MemberView, connection: Connection): CopyKey | null {
  if (!isThisMachine(m) && connection !== "connected") return null;
  const online = m.presence.kind === "online" || m.daemonPresence?.kind === "online";
  return online ? "member.online" : "member.offline";
}

// The Host a machine row stands for: its Daemon's key equals the Host's `daemonKey`.
export function hostOf(m: Pick<MemberView, "daemonKey" | "signKey">, available: Pick<AvailableHost, "id" | "daemonKey">[]): string | null {
  const key = m.daemonKey ?? m.signKey;
  return available.find(h => h.daemonKey === key)?.id ?? null;
}

// The version line of a machine row (phones: none), from its Host's last status.
export function versionLine(m: MemberView, status: HostStatus | undefined): string | null {
  if (isPhone(m)) return null;
  const v = status?.daemonVersion;
  return v ? fmtc("member.version", { version: v }) : fmtc("member.versionUnknown");
}

export interface RemoveDialog {
  title: string;
  body: string;
  self: boolean;
}

export function removeDialog(m: MemberView, members: MemberView[]): RemoveDialog {
  const self = isThisMachine(m);
  if (isPhone(m)) return { title: fmtc("remove.phone.title"), body: fmtc("remove.phone.body", { name: m.name }), self };
  if (members.filter(isMachine).length === 1) return { title: fmtc("remove.last.title"), body: fmtc("remove.last.body"), self };
  if (self) return { title: fmtc("remove.self.title"), body: fmtc("remove.self.body"), self };
  return { title: fmtc("remove.other.title"), body: fmtc("remove.other.body", { name: m.name }), self };
}

const REMOVE_ERRORS: Record<string, CopyKey> = {
  offline: "remove.offline",
  conflict: "remove.changed",
  uncertain: "remove.uncertain",
  busy: "remove.busy",
  other_window: "remove.otherWindow",
  not_enabled: "remove.notMember",
};

export function removeErrorKey(code: string): CopyKey {
  return REMOVE_ERRORS[code] ?? "remove.failed";
}

// Command errors arrive as strings (or objects with a message).
export function errorText(e: unknown): string {
  if (typeof e === "string") return e;
  const m = (e as { message?: unknown })?.message;
  return typeof m === "string" ? m : String(e);
}
