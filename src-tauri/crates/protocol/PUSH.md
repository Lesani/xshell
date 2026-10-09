# Push, version 1

How a Daemon wakes a **Mobile** through the **Push Gateway** with a payload that only that
Mobile can open, and what the Mobile checks. The Relay's side (foreground and the `push`
frame) is in `RELAY.md` sections 17 and 18, the gateway's API in xshell-remote's
`push/README.md`. Design: ADR-0006 and ADR-0011. `ring::push` implements the seal sans-IO;
the vectors are `testdata/ring/push.json`. Notation as in `SESSIONS.md`.

## 1. Registration

Over its session (`SESSIONS.md` section 5), a Mobile sends this to every Daemon whose
`hello` lists the capability `push`, on every session it opens:

```json
{"t":"push.register","id":7,"blob":"xpb1.…","sealKey":"<b64u X25519>","triggers":{"needsYou":true,"finished":true}}
→ {"t":"res","id":7,"ok":null}
{"t":"push.unregister","id":8}  → {"t":"res","id":8,"ok":null}
```

- `blob` is the push blob the Push Gateway returned at its registration: `xpb1.`, 8 to 4096
  bytes of `[A-Za-z0-9_.-]`, opaque to everyone but the gateway.
- `sealKey` is a separate X25519 key of the Mobile, used only to open pushes: a canonical
  key not of small order (`RELAY.md` section 2), never its session `noiseKey`. On iOS the
  Notification Service Extension holds only its secret, not the key that speaks for the
  device in sessions. Registering a new seal key rotates it.
- Both triggers are required. They are the Mobile's own (both on by default).
- Only a Mobile may register; the Daemon binds the registration to the session's Roster
  member, and anyone else gets `err "push.register is for a Mobile"`. A registration
  replaces the previous one and clears any dormant or paused state. The Daemon keeps it
  while its trusted head lists that Mobile with the same `signKey` and `noiseKey`.
- The Daemon pushes when an agent itself reports an Agent Status change to needs you or
  finished (a hook, or Codex's needs-you notification) and that trigger is on; never for a
  change made by input (an interrupt), never for working or ended. It sends nothing while
  any Mobile of the Ring is in the foreground (`RELAY.md` section 17; the Relay checks
  again), at most one push per Mobile per 10 s window (the first change at once, then the
  newest change still current at the window's end), and nothing late: a change that is no
  longer the Terminal's status when its turn comes is dropped.

## 2. Sealed payload

```text
pattern   Noise_X_25519_ChaChaPoly_BLAKE2s, one message:  -> e, es, s, ss
          initiator = the Daemon (its noiseKey), responder static = the Mobile's sealKey
prologue  "xshell-push-v1\n" ‖ ringId
plain     len (u16, big-endian) ‖ json ‖ zeros, padded to P ∈ {512, 1024, 1536, 2048},
          the smallest with 2 + len ≤ P
sealed    0x01 ‖ noise message        1 + 32 + 48 + P + 16 bytes (609 to 2145)
sealedPayload = b64u(sealed)          at most 2860 characters (the gateway takes 3072)
```

```json
{"v":1,"host":"<Daemon signKey>","terminal":"<uuid>","status":"needs-you"|"finished",
 "agent":"claude"|"codex","project":"<cwd>","title":"<at most 120 bytes>","at":<unix ms>,
 "seq":<n>,"needsYou":<n>}
```

- `host` is the Daemon's sign key; `project` the Terminal's working directory; `title`
  (optional) its title, cut to 120 bytes. If the JSON is longer than 2046 bytes, `project`
  is shortened from the left and starts with `…`, then `title` from the right.
- `needsYou` counts the Host's Terminals that need you now, so a replacement notification
  (the same `collapseId`) loses nothing.
- `seq` is strictly increasing per Daemon and Mobile, written to the Daemon's disk before
  the push is sent (it is at least the Daemon's clock in ms, so it keeps growing even if
  the Daemon's state is lost).
- `at` is when the status changed.
- `collapseId` (outside the seal, for APNs and FCM) is
  `b64u(SHA-256("xshell-push-collapse-v1\n" ‖ hostSignKey))`, its first 22 characters:
  stable per Host and no Ring key.

**Opening** (`push::open`, with the seal secret and the Ring id). The Mobile refuses:

- more than 3072 characters, or b64u that is not canonical;
- a first byte other than 1;
- a body that is not exactly one of the four bucket sizes, a small-order ephemeral key, or a
  failing handshake or AEAD tag;
- `len` past the body, non-zero padding, JSON that is not strict (`SESSIONS.md`,
  introduction), `v` other than 1, or a field of the wrong type.

`open` returns the Noise static key that sealed it. The Mobile then requires that its
trusted head lists `host` as a `daemon` member whose `noiseKey` is that key; otherwise it
drops the push.

## 3. Freshness

A sealed push opens again when it is replayed, so the Mobile also applies
`push::check_fresh`. It keeps the highest `seq` it accepted per Daemon where the
notification extension can read and write it (the App Group's shared storage on iOS), and
refuses a push whose `seq` is not above it, whose `at` is more than 24 h old, or whose `at`
is more than 5 min in the future. A push that passes raises the stored `seq`.

## 4. Limits

What the seal does not do (ADR-0011):

- The sender is authenticated relative to the recipient, not by a signature. Whoever holds a
  seal secret can open every push sealed to it and can also seal pushes to it that appear
  to come from any Daemon of the Ring. A Relay or the gateway cannot, and a removed Daemon's
  pushes stop opening as from a member once the head drops it.
- Nothing is forward-secret: the secret opens every recorded push sealed to it, and a new
  seal key does not protect pushes recorded before.
- The bucket size tells a coarse length class; the gateway and the providers see when a
  Mobile is woken.

## 5. Delivery outcomes

The Relay answers each push with `ok` or `push_failed` and a `detail` (`RELAY.md`
section 18). The Daemon:

- marks the registration **dormant**, on disk, until the Mobile registers again:
  `device_gone`, `blob_expired`, `blob_invalid`, `binding_moved`, `subscription_inactive`;
- **pauses** every Mobile until the next UTC midnight, in memory: `quota_exceeded`,
  `attempts_exceeded`;
- retries **once**, after 2 s, only `reconcile_pending` (the gateway refunded that delivery),
  if the push is still current then;
- drops `foreground` silently, logs `bad_request` and `payload_too_large` as errors,
  `unavailable` once per connection, and anything else as a warning;
- never retries a timeout (the outcome is unknown, and the gateway charged the attempt) and
  queues nothing while it is offline.
- sends nothing whose `seq` it could not write first, and checks every condition of
  section 1 once more right before it hands the frame to the Relay connection, so a change
  made meanwhile (the status, a new registration or head, a Mobile in front, the Daemon
  stopping) drops the push.

An answer applies only to the registration it was sent under: one that arrives after the
Mobile registered again changes nothing. A Mobile re-arms everything by registering again,
which it does on every session it opens.

