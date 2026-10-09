import { fmt, type StringKey } from "./strings";
import { newHostId } from "./projectKey";
import type { HostConfig, HostErrorHint } from "./types";
import type { AgentId } from "../agents";

// Host form validation — mirrors the Rust `config::validate` rules so the user sees the
// problem before Save.

export interface HostFormValues { name: string; sshTarget: string; color?: string; daemonCommand: string; launchPrefixes?: Partial<Record<AgentId, string>> }
export type HostFormErrors = Partial<Record<"name" | "sshTarget" | "daemonCommand" | `launchPrefix.${AgentId}`, string>>;

// Whether `s` splits into shell words: every quote is closed and no backslash is left
// dangling, the cases where the Rust side's `shlex::split` gives up.
export function shellWordsOk(s: string): boolean {
  let quote: "'" | '"' | null = null;
  for (let i = 0; i < s.length; i++) {
    const c = s[i];
    if (quote === "'") { if (c === "'") quote = null; continue; }
    if (c === "\\") { if (++i >= s.length) return false; continue; }
    if (quote === '"') { if (c === '"') quote = null; continue; }
    if (c === "'" || c === '"') quote = c;
  }
  return quote === null;
}

// eslint-disable-next-line no-control-regex
const CONTROL_OR_SPACE = /[\s\u0000-\u001f\u007f]/;

export function validateHostForm(v: HostFormValues): HostFormErrors {
  const e: HostFormErrors = {};
  if (!v.name.trim()) e.name = fmt("hosts.form.err.nameRequired");
  const t = v.sshTarget.trim();
  if (!t) e.sshTarget = fmt("hosts.form.err.targetRequired");
  else if (t.length > 255 || CONTROL_OR_SPACE.test(t) || t.startsWith("-")) e.sshTarget = fmt("hosts.form.err.targetInvalid");
  if (v.daemonCommand.length > 0 && (!v.daemonCommand.trim() || /[\r\n]/.test(v.daemonCommand))) e.daemonCommand = fmt("hosts.form.err.commandInvalid");
  for (const [agent, prefix] of Object.entries(v.launchPrefixes ?? {}) as [AgentId, string | undefined][]) {
    if (prefix && (/[\r\n]/.test(prefix) || !shellWordsOk(prefix))) e[`launchPrefix.${agent}`] = fmt("hosts.form.err.prefixInvalid");
  }
  return e;
}

// Form values → config. A new Host gets a fresh id; blank optional fields are omitted.
export function toHostConfig(v: HostFormValues, id?: string): HostConfig {
  const cfg: HostConfig = { id: id ?? newHostId(), name: v.name.trim(), sshTarget: v.sshTarget.trim() };
  if (v.color) cfg.color = v.color;
  const cmd = v.daemonCommand.trim();
  if (cmd) cfg.daemonCommand = cmd;
  const prefixes = Object.entries(v.launchPrefixes ?? {}).map(([a, p]) => [a, p?.trim() ?? ""] as const).filter(([, p]) => p);
  if (prefixes.length) cfg.launchPrefixes = Object.fromEntries(prefixes);
  return cfg;
}

const HINT_KEYS: Record<HostErrorHint, StringKey> = {
  "host-key": "hosts.hint.hostKey",
  "permission-denied": "hosts.hint.permissionDenied",
  "unresolved": "hosts.hint.unresolved",
  "unreachable": "hosts.hint.unreachable",
  "ssh-missing": "hosts.hint.sshMissing",
  "unsupported-platform": "hosts.hint.unsupportedPlatform",
  "binary-unavailable": "hosts.hint.binaryUnavailable",
  "daemon-command-failed": "hosts.hint.daemonCommandFailed",
  "xshell-not-running": "hosts.hint.xshellNotRunning",
};

export function hintText(hint: HostErrorHint | null | undefined, target: string): string | null {
  if (!hint) return null;
  return fmt(HINT_KEYS[hint], { target });
}
