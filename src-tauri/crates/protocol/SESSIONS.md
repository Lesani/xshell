# Pairing and sessions, version 1

How a device joins a **Ring** (**Pairing**) and how members then talk to each other end
to end through the **Relay**. The Relay's side (the pairing pipe and envelopes) is in
`RELAY.md`; this document is what both endpoints must agree on. Design: ADR-0004 and
ADR-0010. `ring::pairing` and `ring::noise` implement it sans-IO; `ring::relay::pair` and
`ring::relay::sessions` drive it over a Relay connection.

All byte strings below are ASCII unless said otherwise, `\n` is LF, `‖` is concatenation,
and `b64u` is the strict base64url of `RELAY.md` section 2. JSON is strict in the same way
(one object, no duplicate keys, integers at most 2^53−1); unknown fields are ignored.

## 1. The secret, the slot and the key

A Desktop makes a one-time secret and keeps it in memory only, for 10 minutes:

- for a **phone**, 32 random bytes in a QR payload (section 2);
- for a **computer**, which shows a code that the user types on the Desktop
  (`xshelld pair`), 10 random bytes (section 3).

Both sides derive from it:

```text
slot = b64u(SHA-256("xshell-pair-slot-v1\n" ‖ secret))        43 characters
psk  = SHA-256("xshell-pair-psk-v1\n" ‖ secret)               32 bytes
```

They meet on the Relay's pairing pipe at `{relayUrl}/v1/pair/{slot}` (`RELAY.md`
section 16). The Relay sees the slot, never the secret or the key.

## 2. QR payload

```text
"xsp1." ‖ b64u(JSON)
{"v":1,"ringId":"…","relayUrl":"wss://…","signKey":"<Desktop>","noiseKey":"<Desktop>",
 "secret":"<b64u 32 bytes>","expiresAt":<unix seconds>}
```

At most 2048 bytes, QR error correction level M. Every field is required; `relayUrl` is a
valid Relay URL, the keys are usable keys (`RELAY.md` section 2). `expiresAt` is shown to
the user only: the Desktop enforces the expiry with its own monotonic clock. The Desktop
also shows the same text for copying (an emulator has no camera).

## 3. Pair code

The 10 bytes (80 bits) in Crockford base32, 16 characters in groups of four:
`7KQ4-M2XW-9PJR-H3CT`. Input is normalized: case, hyphens and spaces are ignored, `O`
reads as `0`, and `I` and `L` as `1`; anything else outside the alphabet
`0123456789ABCDEFGHJKMNPQRSTVWXYZ` is refused.

The length is the defence. Under XXpsk3 an attacker who runs the Relay can pose as the
Desktop, record message 3 and try every code offline. The slot hash is itself a verifier,
so the expiry does not limit such an attacker: the work is 2^80 guesses, whatever the
expiry. The expiry only stops a late pairing between honest endpoints. A shorter code would
need a PAKE.

## 4. Pairing handshake

`Noise_XXpsk3_25519_ChaChaPoly_BLAKE2s`, the psk above at position 3, prologue
`"xshell-pair-v1\n" ‖ slot`. The joining device (guest) is the initiator, the Desktop the
responder. Each message is one `pair.msg` on the pipe (at most 8192 bytes).

1. G→D `e`, empty payload.
2. D→G `e, ee, s, es`, payload `{"v":1,"ringId":"…","name":"…"}`. Not authenticated yet:
   the guest trusts nothing in it until message 4. A **phone** aborts here unless the
   static key is the QR's `noiseKey`, before it says anything about itself.
3. G→D `s, se, psk`, payload
   `{"v":1,"role":"mobile"|"daemon","signKey":"…","name":"…","pop":"<b64u signature>"}`.
   `pop` is Ed25519 by `signKey` over `"xshell-pair-pop-v1\n" ‖ h`, where `h` is the
   handshake hash after message 2. The guest's `noiseKey` is its Noise static key, not a
   payload field.
4. D→G, the first transport message (nonce 0):
   `{"v":1,"ok":true,"ringId","relayUrl","version":N,"hash":"<b64u SHA-256 of token N>","signedBy":"<signKey>"}`
   or `{"v":1,"ok":false,"error":"expired|used|role|duplicate|full|publish_failed"}`.

**The Desktop, when message 3 arrives:**

1. Uses the secret up, before decrypting anything, so a replay or a second try finds it
   gone (`used`); past its 10 minutes, `expired`.
2. Decrypts message 3 (only the psk opens it), checks the static key and `signKey` as
   usable keys and verifies `pop`.
3. Checks the role against the flow it started: a QR adds a `mobile`, a code a `daemon`
   (else `role`).
4. Refuses keys that are members under other keys or another role (`duplicate`) and a full
   Roster (64 members, `full`). The same keys with the same role again are not an error:
   the answer names the head that lists them.
5. Signs version n+1 with the new member, commits it, publishes it and waits for the
   Relay's `ok` (else `publish_failed`), and only then sends message 4.

**The guest, after message 4,** connects to the Ring endpoint as a member, without staging
anything, reads `roster.get since 0` to the end, verifies the chain from genesis for that
`ringId`, and requires that version N hashes to `hash` and that its head lists the guest.
So a Relay that hides or forks the chain fails the pin.

## 5. Session envelopes

Members talk through pairwise `Noise_IK_25519_ChaChaPoly_BLAKE2s` sessions; each message is
the payload of one Relay `env` (at most 65536 bytes). Every payload starts with a header:

```text
ver (u8) = 1 | kind (u8) | sid (16 bytes) | n (u64, big-endian)       26 bytes
kind: 1 = HS1, 2 = HS2, 3 = DATA
```

`sid` is random per handshake, chosen by the initiator. HS1 and HS2 carry `n = 0`; in DATA,
`n` is the AEAD nonce. A receiver drops a payload whose header does not parse.

**Prologue**, binding the Relay's `from` and `to`, the Ring and the session:

```text
"xshell-noise-v1\n" ‖ ringId ‖ "\n" ‖ initiatorSignKey ‖ "\n" ‖ responderSignKey ‖ "\n" ‖ sid
```

(keys in b64u, `sid` as its 16 raw bytes). A Relay that lies about `from` makes the
handshake fail.

**HS1** (initiator → responder) payload: `{"ts":<ms>}`. The initiator sends
`max(now, last + 1)`. The responder keeps the highest accepted `ts` per peer in memory and
drops an HS1 with `ts` not above it, so a replayed HS1 never replaces a live session (as in
WireGuard). If a peer's clock moves back, its HS1s are dropped until the responder restarts;
this is accepted.

**HS2** (responder → initiator) payload: `{"ok":true}`, or `{"ok":false,"error":"forbidden"}`
for a peer whose role may not open sessions. An HS1 from a static key that the responder's
head does not list under the Relay's `from` is dropped without an answer, so a stranger
learns nothing.

**Who opens:** Desktops and Mobiles initiate; a Daemon only responds. The responder finds the
member by its Noise static key in its trusted head, requires that member's `signKey` to be
the Relay's `from`, and maps the role: `desktop` → a Desktop connection, `mobile` → a Mobile
connection (the Daemon enforces what a Mobile may do), `daemon` → `forbidden`. One session
per peer: a newer valid HS1 replaces the old session.

**DATA** plaintext: `inner (u8) ‖ body`, sealed with the session's transport key and nonce
`n`:

| inner | body |
|---|---|
| 0 | stream bytes |
| 1 | close, a UTF-8 reason (at most 256 bytes) |
| 2, 3 | reserved (ping, rekey); ignored |
| other | an error |

All of it is inside the AEAD, so the Relay cannot forge a close. A body is at most
`65536 − 26 − 16 − 1 = 65493` bytes. The session carries one ordered byte stream, and the
xshell protocol's frame codec runs on it unchanged; a frame larger than one message is
simply split across messages.

**Strict order.** The receiver requires `n` to be exactly the next value (0, 1, 2, … per
direction). A replayed, reordered, dropped or altered message ends the session; the stream
above never sees a gap. A DATA payload with another `sid` is dropped.

**Rekey:** none in v1. After 2^48 messages in one direction the session ends and the
initiator opens a new one.

## 6. When a session ends

Either side ends a session, and its stream fails, when:

- it adopts a head (from the Relay, from a local `ring.join`, or a new Relay connection)
  that no longer lists the peer with the same `signKey`, `noiseKey` and role;
- the Relay reports the peer offline;
- the Relay refuses an envelope to the peer (any `error` with `to` naming it: `offline`,
  `unknown_recipient`, `too_large`, `quota`, `entitlement_required`, …), since one of its
  messages may be lost; a limited (unentitled) session ends every session;
- its Relay connection leaves `Connected`, since envelopes may have been lost;
- a message fails to decrypt or is not the next one;
- either side sends close, or the connection above it ends.

An initiator that gets no HS2 within 5 s tries again with a new `sid`, for up to 15 s: a
Daemon may not yet have the head that lists a device paired a moment ago (a Relay client
drops envelopes from senders not in its head).
