// The strings kept from the earlier Ring section (phone pairing, the Relay and connection lines,
// the quota notice), verbatim. The "Ring just works" copy is in ./copy.ts. `{param}`
// placeholders are filled by fmt().

export const S = {
  "mobile.relay.title": "Relay",
  "mobile.relay.hosted": "Hosted relay",
  "mobile.relay.url.placeholder": "wss://relay.example.com",
  "mobile.relay.save": "Use this relay",
  "mobile.relay.err.invalid": "Enter a wss:// URL. Use ws:// only for a loopback address, such as localhost.",
  "mobile.conn.connected": "Connected to {relay}",
  "mobile.conn.connecting": "Connecting to {relay}…",
  "mobile.conn.retrying": "Can't reach {relay}. Retrying in {s}s",
  "mobile.conn.limited": "Connected. To control agents through the hosted relay, purchase a Hosted subscription on one of your paired phones.",
  "mobile.conn.stopped": "This app is no longer one of your paired devices.",
  "mobile.local.tooOld": "Local terminal setup doesn't support mobile access. Update xshell on this computer, then reopen it.",
  "mobile.conn.otherWindow": "Another xshell window manages the connection to your devices.",
  "mobile.quota.hosted": "Your devices have reached the daily message limit on the hosted relay. The limit resets at {time}; replies and agent output won't reach your phone until then.",
  "mobile.quota.ownRelay": "Your devices have reached the daily message limit on your own relay. The limit resets at {time}; replies and agent output won't reach your phone until then.",
  "mobile.problem.unreadable": "Couldn't read your mobile access settings. Mobile access is unavailable: {error}",
  "mobile.pair.phone": "Pair a phone",
  "mobile.pair.phone.desc": "Scan this QR code with the phone app. It works once, for 10 minutes. Can't scan? Copy the pairing text into the phone app.",
  "mobile.pair.phone.expires": "Code expires in {m}:{ss}",
  "mobile.pair.phone.copy": "Copy pairing text",
  "mobile.pair.phone.cancel": "Cancel pairing",
  "mobile.pair.phone.new": "New code",
  "mobile.pair.expired": "This code expired. Select New code to try again.",
  "mobile.pair.waiting": "Waiting for your phone…",
  "mobile.pair.paired": "Added {name}",
  "mobile.pair.failed": "Couldn't pair the device: {error}",
  "mobile.pair.err.busy": "Another pairing is already in progress. Wait for it to finish.",
  "mobile.pair.err.role": "That device can't be added as a computer.",
  "mobile.pair.err.full": "You've reached the limit of 64 devices. Remove a device, then try again.",
  "mobile.pair.connecting": "Connecting to the relay. You can start pairing now. xshell will add the device once it connects.",
  "mobile.pair.needsRelay": "Pairing needs a connection to the relay. xshell keeps trying.",
  "mobile.pair.needsMember": "This app is no longer one of your paired devices, so it can't add devices.",
  "mobile.pair.err.publishFailed": "xshell couldn't reach the relay in time to add the device. Try again.",
  "ring.unsupported": "Creating or joining a Ring is not available on this platform yet.",
} as const;

export type StringKey = keyof typeof S;

// `{name}` interpolation. Unknown placeholders are left as-is.
export function fmt(key: StringKey, params?: Record<string, string | number>): string {
  const s: string = S[key];
  if (!params) return s;
  return s.replace(/\{(\w+)\}/g, (m, name: string) => (name in params ? String(params[name]) : m));
}
