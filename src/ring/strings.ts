// Every user-facing string of Settings → Mobile (#8, #21): the binding copy tables and the
// adopted review copy, verbatim. `{param}` placeholders are filled by fmt().

export const S = {
  "settings.nav.mobile": "Mobile",
  "mobile.section.title": "Mobile access",
  "mobile.section.desc": "Watch and control coding agents on your computers from your phone. Traffic is end-to-end encrypted, so the relay can't read it.",
  "mobile.enable": "Enable mobile access",
  "mobile.enabling": "Enabling…",
  "mobile.enable.failed": "Couldn't enable mobile access: {error}",
  "mobile.relay.title": "Relay",
  "mobile.relay.hosted": "Hosted relay",
  "mobile.relay.hosted.desc": "Run by the xshell project. Requires a Hosted subscription purchased in the phone app.",
  "mobile.relay.custom": "Self-hosted relay",
  "mobile.relay.url.label": "Relay URL",
  "mobile.relay.url.placeholder": "wss://relay.example.com",
  "mobile.relay.url.help": "Changing the relay moves all your paired devices to the new URL.",
  "mobile.relay.save": "Use this relay",
  "mobile.relay.err.invalid": "Enter a wss:// URL. Use ws:// only for a loopback address, such as localhost.",
  "mobile.relay.err.saveFailed": "Couldn't change the relay: {error}",
  "mobile.conn.connected": "Connected to the relay",
  "mobile.conn.connecting": "Connecting to the relay…",
  "mobile.conn.retrying": "Can't reach the relay. Retrying in {s}s",
  "mobile.conn.limited": "Connected. To control agents through the hosted relay, purchase a Hosted subscription on one of your paired phones.",
  "mobile.conn.stopped": "This app is no longer one of your paired devices.",
  "mobile.members.title": "Your devices",
  "mobile.member.thisApp": "This app",
  "mobile.member.thisComputer": "This computer",
  "mobile.role.desktop": "Desktop",
  "mobile.role.daemon": "Host",
  "mobile.role.mobile": "Phone",
  "mobile.status.online": "Connected",
  "mobile.status.closed": "xshell closed",
  "mobile.status.unreachable": "Unreachable",
  "mobile.status.never": "Never connected",
  "mobile.status.unknown": "Unknown",
  "mobile.local.inProcess": "Terminals on this computer run inside the app and aren't reachable from your phone.",
  "mobile.local.tooOld": "Local terminal setup doesn't support mobile access. Update xshell on this computer, then reopen it.",
  "mobile.conn.moving": "Telling your paired devices about the new relay…",
  "mobile.conn.moveFailed": "Some paired devices may still use the old relay. xshell keeps trying to reach them.",
  "mobile.conn.otherWindow": "Another xshell window manages the connection to your devices.",
  "mobile.problem.recovered": "Couldn't read your mobile access settings. Saved the old file at {path}. Enable mobile access again, then pair your devices again.",
  "mobile.problem.unreadable": "Couldn't read your mobile access settings. Mobile access is unavailable: {error}",
  "mobile.host.tooOld": "Update xshelld on {name} in Settings → Hosts to make it reachable from your phone.",
  "mobile.host.otherRing": "{name} is paired with another set of devices. Pairing here will end their mobile access to it.",
  "mobile.host.claim": "Pair with this desktop",
  "mobile.host.full": "{name} can't join your devices. You've reached the limit of 64. Remove a device, then try again.",
  "mobile.host.failed": "Couldn't pair {name}: {error}",
} as const;

export type StringKey = keyof typeof S;

// `{name}` interpolation. Unknown placeholders are left as-is.
export function fmt(key: StringKey, params?: Record<string, string | number>): string {
  const s: string = S[key];
  if (!params) return s;
  return s.replace(/\{(\w+)\}/g, (m, name: string) => (name in params ? String(params[name]) : m));
}
