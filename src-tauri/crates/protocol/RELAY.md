# The Relay protocol, version 1

A **Relay** joins the devices of one **Ring** (Desktops, Daemons and Mobiles) and carries
opaque, end-to-end encrypted envelopes between them. It authenticates devices against the
Ring's signed **Roster**, keeps the Roster's versions, reports presence, and holds the
Ring's entitlement token. It never sees plaintext. This document is complete enough to
implement a Relay (the Worker in `xshell-remote`) from it; `xshell-protocol` implements the
device side (`ring::relay::RingClient`) and a test Relay (`ring::relay::test_relay`).

Terms are the glossary's (`CONTEXT.md`). Design: ADR-0004, ADR-0008, ADR-0009 and the
PRD amendments A2–A4.

## 1. Security boundary

What the protocol guarantees, and what it does not:

- **The Relay cannot forge membership.** Every Roster version is signed. Version 1 is signed
  by the Ring's creator, and the Ring id is a hash of that creator's key. Every later version
  is signed by a Desktop of the version before it and hashes that version in `prev`. Devices
  verify every version before they trust it. The Relay verifies too, so that it does not
  route for non-members, but devices never rely on the Relay's check.
- **No rollback below a device's head.** A device keeps the newest head it verified and
  refuses older versions and forks of known versions.
- **No freshness proof.** A Relay can hide a newer version from a device, and a removed or
  compromised Desktop together with a malicious Relay can show a fork to a lagging device.
  The `prev` hash makes such a fork detectable when two devices compare heads (over Noise,
  #9), but this protocol alone does not detect it.
- **`from` and presence are Relay assertions.** The Relay stamps an envelope's `from` with
  the key the socket authenticated as, and reports presence as it sees it. A malicious Relay
  can lie about both. Devices drop envelopes whose `from` is not in their own trusted head.
  Inside an envelope, the Noise sessions (`SESSIONS.md`) authenticate the peer end to end:
  each handshake binds both sign keys, so a lie about `from` makes the handshake fail, and
  the Relay can drop, delay or refuse session messages but not read, alter, replay or
  inject them. Presence remains a claim; a device uses it only to end sessions early.
- **Before authentication the Relay reveals** a Ring's head version (in `challenge`) to
  anyone who knows the Ring id. The Ring id is a hash of a public key and is not secret
  within the Ring, but it is not guessable from outside.

## 2. Encodings

- **base64url** (`b64u`): RFC 4648 §5 without padding. Decoders refuse padding (`=`), the
  standard alphabet (`+`, `/`), whitespace, a length ≡ 1 (mod 4), and non-zero trailing bits.
  Every byte string has exactly one accepted spelling, so keys compare as strings.
- **JSON**: every frame and every Roster payload is one JSON object in UTF-8. Fields are
  camelCase. Unknown fields are ignored on receipt (and, in a Roster, covered by its
  signature and kept). These are refused anywhere in the text, in ignored fields too:
  - invalid UTF-8;
  - a duplicate key at any depth;
  - a `\u` escape that is a lone UTF-16 surrogate (an escaped pair such as `\ud83d\ude00`
    is fine);
  - a number outside the finite range of an IEEE 754 double, such as `1e999`;
  - more than 127 levels of nested objects and arrays, the top-level object included.

  JavaScript's `JSON.parse` keeps the last of two duplicate keys and reads `1e999` as
  `Infinity`, so a Worker needs a parser of its own for frames and Roster payloads.
- **Integers**: every integer field, in frames and in Roster payloads (ids, versions,
  timestamps, generations), is written as a plain non-negative integer (digits only: no sign,
  fraction or exponent) of at most 2^53−1. So `-0`, `-1`, `1.0` and `1e2` are refused, and
  so is 2^53. The refusal is reported as follows (section 4.4 for Rosters, section 6 for
  frames):

| Integer field holds | In a Roster | In a frame |
|---|---|---|
| `0` to `9007199254740991` | accepted | accepted |
| `9007199254740992` to `18446744073709551615` | `invalid` | bad fields (`bad_request`) |
| a sign, fraction or exponent, or more than `18446744073709551615` | `malformed` | bad fields (`bad_request`) |

  A Roster's `v` is read as a 32-bit unsigned integer: above 4294967295 it is `malformed`,
  and any value other than 1 is `invalid`. A field the reader ignores may hold any finite
  number. `testdata/ring/roster-reject.json` and the `lexemes` of `frames.json` pin these
  rules.
- **Keys**: `signKey` is an Ed25519 public key (32 bytes, b64u, 43 characters). It is
  decoded as curve25519-dalek decompresses a point: the low 255 bits are y, read modulo p
  (so a non-canonical y ≥ p is accepted and means y − p), and the top bit selects x. The
  result must be a point on the curve and not of small order (8·A is not the identity).
  Keys still compare as strings, so two encodings of one point are two keys. Every
  non-canonical encoding decodes to a point with y below 19, whose private key nobody
  knows, so no such key can sign. `noiseKey` is an X25519 public key (32 bytes, b64u) in
  canonical form (u < 2^255−19, high bit clear) and not of small order: the u-coordinates 0,
  1, the two order-8 points (`e0eb7a7c…b800` and `5f9c95bc…1157`) and p−1 are refused, and
  their non-canonical forms, p and p+1, fail the canonical check.
- **Signatures** are Ed25519, 64 bytes, b64u (86 characters). Devices and Relays verify them
  strictly, exactly as ed25519-dalek 2.2's `verify_strict` does:
  1. `S` is canonical: S < L, the group order;
  2. `R` decodes to a point (as a key does) that is not of small order;
  3. the key is not of small order;
  4. with k = SHA-512(R as sent ‖ the key as sent ‖ message) mod L, the canonical encoding
     of sB − kA equals R as sent (the cofactorless equation; so a non-canonical R fails).

  This is normative for Relays, not only for devices: a Relay that stored a malleated
  (S + L) copy of a genuine token, or a token signed with a small-order R, would hold a
  head that every device refuses. WebCrypto is not enough on its own. On workerd
  2026-09-21, `crypto.subtle` imports any 32 bytes as an Ed25519 key, refuses S ≥ L, and
  accepts both a small-order R and a small-order key. A Worker therefore decodes keys and R
  itself and refuses small orders before it calls `verify`. `testdata/ring/ed25519.json`
  pins key decoding and these signatures.
- **Domain separation**: each signed message starts with its own context string:

| Purpose | Context (ASCII, `\n` is LF) |
|---|---|
| Ring id derivation | `xshell-ring-v1\n` |
| Roster signature | `xshell-roster-v1\n` |
| Challenge answer | `xshell-relay-auth-v1\n` |
| Push Gateway entitlement (not signed by devices) | `xshell-entitlement-v1\n` |

## 3. Ring id

```text
ringId = b64u(SHA-256(ASCII("xshell-ring-v1\n") || creatorSignKey32))
```

That is 43 characters of `[A-Za-z0-9_-]`. Relays and the Push Gateway accept any id that
matches `^[A-Za-z0-9_-]{16,128}$`; whether an id was derived from its creator is a check on
the genesis Roster (section 4.3).

## 4. The Roster

### 4.1 Token

```text
token     = "xro1." + b64u(payloadJSON) + "." + b64u(signature)
signature = Ed25519(signedBy, ASCII("xshell-roster-v1\n") || ASCII(b64u(payloadJSON)))
```

The signature covers the *encoded* payload string, exactly as the Push Gateway's
entitlement tokens do, so the Worker can reuse `verifyEntitlement`'s shape and nobody needs
canonical JSON. Relays and devices store and forward the exact token string and never
re-encode it. A token is at most 65536 bytes.

### 4.2 Payload

```json
{
  "v": 1,
  "ringId": "<43 chars>",
  "version": 2,
  "prev": "<b64u SHA-256 of the previous token>",
  "relayUrl": "wss://relay.example.com",
  "signedBy": "<signKey>",
  "issuedAt": 1767225660,
  "members": [
    {"name": "laptop", "role": "desktop", "signKey": "<signKey>", "noiseKey": "<noiseKey>", "addedAt": 1767225600}
  ]
}
```

Every field above is required (`prev` is `null` in version 1). Structural rules, checked on
every version on its own:

- `v` is 1. `version` is at least 1. `issuedAt` and `addedAt` are Unix seconds.
- `prev`, when not `null`, decodes to 32 bytes.
- `relayUrl` is a valid Relay URL (section 5).
- `members` has 1 to 64 entries. `signKey`s are unique, and `noiseKey`s are unique.
- `role` is `desktop`, `daemon` or `mobile`. At least one member is a `desktop`.
- `name` is 1 to 64 bytes of UTF-8 with no control characters (Unicode `Cc`) and none of
  U+200B–U+200F, U+202A–U+202E, U+2060–U+2069, U+FEFF.
- The signature verifies under `signedBy`.

`signedBy`, `signKey` and `noiseKey` are typed fields, like the integers: a value that is not
a usable key by section 2 (a small-order or off-curve `signKey`, a small-order or
non-canonical `noiseKey`) makes the token `malformed`, not `invalid`.

### 4.3 Chain rules

**Genesis (version 1):** `version == 1` and `prev == null`; `ringId == derive(signedBy)`;
`signedBy` is a member of version 1 itself, with role `desktop`.

**Successor (version n+1 after version n):**

1. `ringId` is unchanged, else `ring_mismatch`.
2. `version > n`, else `stale`; `version == n + 1`, else `gap`.
3. `prev == b64u(SHA-256(ASCII(token_n)))`, else `prev_mismatch`.
4. `signedBy` is a member of version n, else `signer_not_member`, with role `desktop`, else
   `signer_not_desktop`. Mobiles and Daemons never sign. The signer need not remain a member
   of version n+1.

Moving a Ring to another Relay is an ordinary new version with a new `relayUrl`. A chain is
at most 4096 versions long.

**Accepting into a stored chain** (devices and Relays alike): a version byte-identical to a
stored one is skipped. A stored version number with different bytes is refused: `prev_mismatch`
at the head (a fork), `stale` below it. Anything newer must be a valid successor of the head.

### 4.4 Refusal details

A refused Roster is reported with one of these `detail` codes, in this order of checking:

| Detail | Meaning |
|---|---|
| `too_large` | token over 64 KiB, chain over 4096 versions, or candidate over 16 MiB |
| `malformed` | prefix, base64url, UTF-8, JSON (section 2), duplicate key, or a missing or mistyped field (an unusable key, or an integer written with a sign, fraction or exponent, included) |
| `invalid` | a structural rule of 4.2 (other than the signature), or an integer above 2^53−1 that fits 64 bits |
| `bad_signature` | the signature does not verify under `signedBy` |
| `not_genesis` | a chain that does not start at version 1 with `prev: null` |
| `ring_mismatch` | wrong `ringId` (not derived from the genesis signer, or changed) |
| `stale`, `gap`, `prev_mismatch` | successor rules 2 and 3 |
| `signer_not_member`, `signer_not_desktop` | genesis or successor rule 4 |

## 5. Relay URL and origin

A `relayUrl` is `wss://host[:port][/path]`, or `ws://` only to a loopback host (`localhost`,
`127.0.0.0/8`, `::1`). It is at most 512 bytes of printable ASCII, with no userinfo, query,
fragment, backslash, percent escape, empty or dot path segment. The host is a DNS name
(labels of `[A-Za-z0-9-]`, 1 to 63 bytes, not starting or ending with `-`, no trailing dot),
a dotted-quad IPv4 address in canonical form, or a bracketed IPv6 address without an embedded
IPv4 part. A port, if given, is 1 to 65535 without leading zeros. These limits exist so that
every accepted URL normalizes the same way in Rust and in a Worker's `new URL()`.
`testdata/ring/urls.json` pins which URLs are accepted, their origins and their Ring
endpoints.

**Origin** (what a device signs): lowercase scheme and host, the default port (wss 443,
ws 80) dropped, no path, IPv6 in brackets in canonical (RFC 5952) form:

```text
WSS://Relay.Example.COM:443/x/  ->  wss://relay.example.com
wss://relay.example.com:8443    ->  wss://relay.example.com:8443
ws://[0:0:0:0:0:0:0:1]:80       ->  ws://[::1]
```

The Relay compares against its configured `RELAY_ORIGIN`. When that is unset it uses the
request's origin with `https` mapped to `wss` and `http` to `ws`. A self-hoster behind a
proxy or tunnel must set `RELAY_ORIGIN`, or every login fails with `bad_signature`; log the
expected origin when that happens.

## 6. Transport

- A device dials `{relayUrl without trailing /}/v1/ring/{ringId}` as a WebSocket upgrade. A
  `relayUrl` may carry a path, so the Relay serves every request path that **ends** in
  `/v1/ring/{ringId}`, whatever comes before it; a Relay behind a path prefix need not know
  the prefix. `ringId` is the last path segment and must match `^[A-Za-z0-9_-]{16,128}$`.
  The Worker routes it to `idFromName(ringId)`.
- HTTP answers: such a path without a WebSocket upgrade gets 426; `GET /healthz` gets 200;
  every other request, a bad `ringId` included, gets 404. The pairing pipe,
  `…/v1/pair/{slot}`, is the one other endpoint (section 16).
- **Text frames only.** A binary frame gets `error{code:"unsupported"}` and close 4000.
- Each frame is one JSON object tagged by `"t"`.
- An unknown `t` from the Relay is ignored by devices. An unknown `t` from an authenticated
  client gets `error{code:"unknown_type"}` and the socket stays open, so extensions stay
  additive.

**Frame size caps** (the frame's length in UTF-8 bytes, checked before or right after
parsing; every byte limit in this document counts UTF-8 bytes):

| Frame (client to Relay) | Cap |
|---|---|
| `auth.chain` | 1 MiB |
| `env` | 96 KiB (and its decoded `payload` at most 64 KiB) |
| `roster.put` | 72 KiB |
| every other frame | 8 KiB |
| any Relay to client frame (`roster.chain` included) | 1 MiB |

A frame over its cap gets `error{code:"too_large"}` and close 1009. A Worker may send 1009:
workerd accepts `close(1009)` (checked on workerd 2026-09-21), and browsers forbid only
*calling* `close()` with it, not receiving it. The runtime's own limit on an inbound message
is far above every cap (32 MiB on workerd), so the Relay checks the caps itself; a message
over the runtime's limit may end the socket without an `error` frame. An `env` whose decoded
payload is over 65536 bytes but whose frame is within the cap gets `error{code:"too_large",
to}` and the socket stays open. Malformed JSON (section 2: no single strict object, no string
`t`, duplicate keys, a lone surrogate, a non-finite number, too deep) gets
`error{code:"bad_request"}` and close 4000. A known type with bad fields (an integer field
that breaks section 2 included) gets `error{code:"bad_request"}`; before authentication it
also closes with 4000. `auth` or `auth.chain` after authentication gets
`error{code:"bad_request"}`, and the socket stays open.

## 7. Handshake

```text
Relay  -> {"t":"challenge","v":1,"nonce":"<b64u 32 random bytes>","rosterVersion":N,"caps":[]}
client -> {"t":"auth.chain","rosters":["xro1…", …]}        (zero or more, optional)
client -> {"t":"auth","signKey":"<signKey>","sig":"<b64u 64 bytes>","caps":[]}
Relay  -> {"t":"welcome","you":"<signKey>","rosterVersion":N,"presence":[…],"entitlement":"xet1…"|null,"caps":[]}
```

- `rosterVersion` in the challenge is the Relay's stored head version for this Ring, or 0
  when it has none.
- Before `auth` the client may only send `auth.chain`, `auth` and `ping`. Anything else gets
  `error{code:"bad_request"}` and close 4000.
- If `auth` does not arrive within the **auth timeout** of the challenge (10 s by default; a
  Relay may configure another, and a contract target names it), the Relay sends
  `error{code:"auth_timeout"}` and closes with 4008.
- The signed message is
  `ASCII("xshell-relay-auth-v1\n" + origin + "\n" + ringId + "\n" + nonce + "\n" + signKey)`,
  with `nonce` and `signKey` exactly as they appear on the wire.

**Staged chain (`auth.chain`).** A device whose head is newer than the Relay's
`rosterVersion` sends the versions the Relay lacks, oldest first, in as many `auth.chain`
frames as needed (each frame at most 1 MiB). For a Relay with no Roster that is the whole
chain from version 1. Whole chains, not only a creator-signed version 1, are accepted so
that a Ring can move to a new Relay. The Relay keeps the tokens as an uncommitted
**candidate**. Each token must parse (else `error{code:"roster_invalid",detail}`, close 4003).
The candidate holds at most 4096 versions and 16 MiB of tokens in all (else
`roster_invalid` with detail `too_large`, close 4003).

**Where the candidate lives.** A Worker's socket can hibernate between `auth.chain` and
`auth`, so the candidate is not kept in memory or in the socket attachment:

- An `auth.chain` frame with an empty `rosters` array is `error{code:"bad_request"}`,
  close 4000.
- The attachment holds `{nonce, deadline, candidateId}` and the candidate's **manifest**
  `{frames, chunks, versions, bytes, stored}`: frames received, physical chunks stored,
  tokens and their bytes, and serialized chunk bytes. (`{signKey, gen}` after `auth`.)
  `candidateId` is random per socket.
- Each frame's tokens are stored as **physical chunks**: JSON arrays of whole tokens, each
  array at most 96 KiB serialized (under the Durable Object's 128 KiB value limit; a token
  is at most 64 KiB, so every chunk holds at least one). Chunk `n` is stored under
  `stage/<candidateId>/<n>`, n zero-padded so keys sort, with indices contiguous from 0
  across all frames.
- Caps per candidate: 4096 versions, 16 MiB of token bytes, 4096 physical chunks and
  16 MiB + 64 KiB of serialized chunk bytes. Beyond any of them: `roster_invalid` with detail
  `too_large`, close 4003.
- At `auth` the Relay reads `stage/<candidateId>/` in key order and deletes those keys,
  whatever the outcome. Reconstruction checks that the keys are exactly indices 0 to
  `chunks − 1`, each chunk is at most 96 KiB and a non-empty array of tokens, and the totals
  of versions, token bytes and serialized bytes equal the manifest. Any mismatch (a chunk
  expired, lost or altered) means the candidate cannot be trusted to be whole: the answer is
  `error{code:"roster_invalid",detail:"invalid"}` and close 4003, and nothing is committed.
- Stage keys carry their creation time. An alarm deletes chunks older than the auth deadline
  plus a margin (the test Relay uses 1 s), so sockets that never authenticate leave nothing
  behind.

**Processing `auth`**, in this order:

1. **The resulting head.** If the Relay stores no Roster for the Ring: an empty candidate is
   `error{code:"no_roster"}`; otherwise the candidate must be a valid chain from genesis
   (4.3) whose `ringId` is the one in the URL. If the Relay stores a Roster: the candidate
   (possibly empty) must be a valid extension of the stored chain by the accepting rule of
   4.3. A failure is `error{code:"roster_invalid",detail}`. Each of these closes with 4003.
2. **Membership.** `signKey` must be a member of the resulting head, else
   `error{code:"not_member"}`, close 4003.
3. **Signature.** `sig` must verify over the message above, with the Relay's origin, else
   `error{code:"bad_signature"}`, close 4001.
4. **Commit.** Only now does the Relay store the candidate's new versions. It then
   broadcasts each new version as `{"t":"roster",…}` to the other connected members and
   disconnects members the new head no longer lists (section 11).
5. **Replace.** An existing socket of the same `signKey` gets `error{code:"replaced"}` and
   close 4009. That is not a goodbye, and its later close or error does not change presence
   (section 8).
6. **Welcome.** `presence` lists every member of the head (the new device included,
   online). On the Hosted Relay `limited` may be `true` (section 12).

Before `welcome` a device accepts only `challenge`, `welcome`, `error`, `pong` and unknown
types; any other frame fails the connect. Between `welcome` and the end of its Roster sync it
keeps session frames (`env`, `presence`, `roster`, `entitlement`) for later, at most 256
frames and 1 MiB, and fails the connect beyond that.

So a stranger can never seed a Ring with an old genesis, and a member added in a version the
Relay has not seen can still connect by supplying that version itself.

## 8. Presence

Per member the Relay stores and persists (a Durable Object can be evicted at any time):

```json
{"signKey":"…","online":true,"lastSeen":1767225600,"lastReason":null}
```

- `lastSeen` is the time of the last connect or disconnect, `null` if never seen.
- `lastReason` is the last `bye` reason, the reserved value `"dropped"` when a socket closed
  without one, and `null` while online or never seen.
- Every change is pushed to the other connected members as `{"t":"presence",…record}`.

**Generations.** Each authenticated socket gets a generation from a per-`signKey` counter
(`gen:<signKey>`) that is persisted on its own and never reset, not even when the member is
removed from the Roster and its presence record forgotten. A member removed and added back
therefore gets a generation its old socket never had. The Worker keeps `{signKey, gen}` in
`serializeAttachment`, so the socket stays authenticated across hibernation. A close, error or
`bye` changes presence only when the socket's generation is the record's current one and
the record is online. Terminal transitions are idempotent: once one has happened, later ones
for that generation do nothing. A replaced socket's late `bye`, close or error therefore
leaves the new socket online.

**After a restart.** A Relay that restarts or is redeployed can lose its sockets without any
close or error event. When it wakes, every presence record that says `online` but has no
live authenticated socket of the record's generation becomes `dropped`, with `lastSeen` set
to the time of the wake, and the change is pushed like any other. (The test Relay never
restarts, so the contract suite does not cover this.)

How a device reads a record (amendment A4, Host Status):

| Record | Device shows |
|---|---|
| `online` | online |
| not online, `lastSeen == null` | never connected |
| not online, `lastReason == "dropped"` | unreachable |
| not online, any other reason | xshell closed |

## 9. Goodbye

```text
client -> {"t":"bye","reason":"quit"}
```

`reason` matches `^[a-z][a-z0-9_.-]{0,31}$` and is never `dropped`. Daemons send `quit`
(GUI quit), `idle` or `upgrade`. The Relay records it (section 8), broadcasts the presence
change and closes with 1000. A device waits briefly for that close (2 s), counted from when the goodbye was requested,
not from when it reached the socket: a device whose outbound data is stuck behind a peer that
stopped reading still closes on time.

## 10. Envelopes

```text
client -> {"t":"env","to":"<signKey>","payload":"<b64u>"}
Relay  -> {"t":"env","from":"<authenticated signKey>","payload":"<b64u>"}   (to the addressee only)
```

The Relay stamps `from` itself and ignores any `from` the client sent. It never inspects
`payload` beyond its encoding and size (at most 65536 bytes decoded; compute the decoded
length from the encoded length before decoding). Refusals come back to the sender as
`{"t":"error","code":…,"to":"<signKey>"}`, with the socket left open:

| Code | When |
|---|---|
| `too_large` | decoded payload over 64 KiB |
| `bad_request` | payload not canonical b64u, or `to` is the sender |
| `unknown_recipient` | `to` is not a member of this Ring's head |
| `offline` | `to` has no socket |
| `quota`, `rate_limited` | Hosted limits (section 14) |

## 11. Roster frames

```text
client -> {"t":"roster.put","id":7,"roster":"xro1…"}
Relay  -> {"t":"ok","id":7}
       or {"t":"error","id":7,"code":"roster_stale"|"roster_conflict"|"roster_invalid","detail":"…"}
```

Any member may upload; the chain rules decide. A token byte-identical to the head is `ok`.
`stale` maps to `roster_stale`, `prev_mismatch` to `roster_conflict`, and every other detail to
`roster_invalid`. On success the Relay stores the version (one storage key per version; each
is at most 64 KiB, under the Durable Object's 128 KiB value limit) and then:

1. broadcasts `{"t":"roster","roster":"xro1…"}` to every connected member, the uploader
   included. The uploader may get the broadcast before or after its `ok`, and a client
   handles both orders (the test Relay's `broadcast_before_ok` tests the other one);
2. sends each member the new head no longer lists `error{code:"removed"}`, closes it with
   4004, and forgets its presence record.

```text
client -> {"t":"roster.get","id":8,"since":3}
Relay  -> {"t":"roster.chain","id":8,"rosters":["xro1…v4", …],"more":true}
```

`rosters` holds the stored versions after `since`, oldest first, as many as fit in 1 MiB
(at least one). `more` says whether versions remain; the device asks again with `since` set
to the last version it received. A device that sees a `roster` broadcast skip a version
(`gap`) does the same.

Devices verify every version they receive. A version that fails is reported locally and
ignored; the device keeps its trusted head.

## 12. Entitlement slot (amendment A2)

```text
client -> {"t":"entitlement.put","id":9,"token":"xet1…"}
Relay  -> {"t":"ok","id":9}  or  {"t":"error","id":9,"code":"entitlement_invalid"}
Relay  -> {"t":"entitlement","token":"xet1…"}     (to every connected member)
```

One switch: a Relay is **Hosted** if and only if it is configured with the Push Gateway's
public keys (the Worker's `GATEWAY_PUBLIC_KEYS`, the test Relay's `hosted`). Then it:

- verifies each `entitlement.put` with `verifyEntitlement(token, ringId, now, keys)` (any
  tier; a refusal's `detail` is the gateway's code: `malformed`, `bad_signature`,
  `unknown_kid`, `ring_mismatch`, `expired`);
- limits sessions without a valid Hosted token (below);
- applies the daily quota (section 14);
- returns the stored token in `welcome.entitlement` only while it still verifies (any
  tier), else `null`.

A Relay that is not Hosted verifies nothing: it stores the latest well-formed token (`xet1.`,
a payload part that decodes to a strict JSON object by section 2, and a part that decodes to
64 bytes, 4 KiB at most) and returns it in `welcome.entitlement` exactly as stored.
`crates/protocol` implements the gateway's check (`ring::entitlement::verify_entitlement`);
`testdata/ring/entitlement.json` pins it.

The Worker verifies with the Push Gateway's own `verifyEntitlement`, which reads the payload
with `JSON.parse`: it accepts duplicate keys and numbers like `1.0` that `crates/protocol`
refuses. This divergence is accepted, because only the gateway signs these tokens and it never
writes such payloads.

**Hosted Relay: limited sessions.** A Hosted Relay routes envelopes for a Ring only while
it stores a valid `tier: "hosted"` token (signature, kid, `ringId`, and `now < expiresAt`, no
grace). Without one, an authenticated member still gets a session, so that a new Ring can
install its first token:

- `welcome` carries `"limited": true`, and `entitlement` is `null` unless the stored token
  still verifies (a Push-tier token, say).
- Allowed: `entitlement.put`, `roster.get`, `roster.put`, `bye`, `ping`.
- `env` is refused with `{"t":"error","code":"entitlement_required","to":…}`. The socket
  stays open.
- When a valid Hosted token is stored, the Relay broadcasts
  `{"t":"entitlement","token":"xet1…"}` (with `"limited": true` if the new token does not
  lift the limit) and every socket of the Ring routes again.
- Validity is checked at each `env`, so the first `env` after the last token's `expiresAt`
  is refused, on existing sockets and new ones alike. A device learns of the limit from
  `welcome.limited`, an `entitlement` frame's `limited`, or the `entitlement_required` error.

`limited` is omitted when false, so a self-hosted Relay's frames do not change.

## 13. Keepalive

The device sends exactly the bytes `{"t":"ping"}` every 30 s, before and after
authentication. The Relay answers exactly `{"t":"pong"}`. On Cloudflare that is
`setWebSocketAutoResponse(new WebSocketRequestResponsePair('{"t":"ping"}', '{"t":"pong"}'))`, so
pings never wake the Durable Object. A device that receives no complete frame for 75 s (two
missed pongs plus slack) treats the connection as dead.

## 14. Quotas (amendment A3)

A Hosted Relay applies a daily quota of client frames per Ring, 2,000,000 per UTC day by
default. Any other Relay applies one only when it is configured with one (the Worker's
`QUOTA_FRAMES_PER_DAY`, the test Relay's `quota_frames_per_day`).

- **What counts:** each client frame on an authenticated socket that passes the size cap
  and the JSON parse (section 6). Frames before `auth` do not count. The exact bytes
  `{"t":"ping"}` never count: on Cloudflare they are auto-answered and never reach the
  Durable Object (section 13). A ping written any other way (`{"t": "ping"}`) does count.
  `bye` is never counted or refused.
- **Over the quota:** once the Ring's count for the day has reached the quota, a frame is not
  processed. The Relay answers `error{code:"quota"}`, with the frame's `id` (`roster.put`,
  `roster.get`, `entitlement.put`) or `to` (`env`) when it decoded with one, and the socket
  stays open. After 32 refusals in a row on one socket (`wire::QUOTA_REFUSALS_BEFORE_CLOSE`;
  a processed frame starts the count again), the Relay closes it with 4029. Nothing is
  billed.
- **Accuracy:** the counter may lose up to 64 counted frames each time the Relay is evicted
  or restarted (a Worker writes it behind, every 64 frames and at each alarm).
- `rate_limited` is reserved: no v1 Relay sends it.
- The 64 KiB per-message limit is the envelope payload limit of section 10. Control frames
  have their own caps (section 6).

## 15. Errors and close codes

`error` frames: `{"t":"error","code":"…","id"?:n,"to"?:"<signKey>","detail"?:"…"}`.

| Code | Close | Meaning |
|---|---|---|
| `bad_request` | 4000 if malformed or before auth | bad frame, bad envelope, or `auth`/`auth.chain` after auth |
| `unsupported` | 4000 | binary frame |
| `unknown_type` | no | unknown `t` after auth |
| `auth_timeout` | 4008 | no `auth` within the auth timeout (10 s by default) |
| `no_roster` | 4003 | no Roster stored and no candidate |
| `not_member` | 4003 | key not in the resulting head |
| `bad_signature` | 4001 | challenge answer does not verify |
| `roster_invalid` | 4003 at auth, no otherwise | a Roster failed the rules (`detail`) |
| `roster_stale`, `roster_conflict` | no | `roster.put` refused |
| `removed` | 4004 | a new head no longer lists this device |
| `replaced` | 4009 | the same key connected again |
| `offline`, `unknown_recipient`, `too_large` | no (`too_large` on a frame: 1009) | envelope refused |
| `quota` | 4029 after 32 in a row | daily quota (section 14) |
| `rate_limited` | no | reserved; not sent in v1 |
| `entitlement_required` | no | `env` in a limited session (section 12) |
| `entitlement_invalid` | no | `entitlement.put` refused |
| `internal` | optional | Relay failure |
| `pair_busy` | 4010 | pairing pipe: a third socket, or a slot already used (section 16) |
| `pair_expired` | 4008 | pairing pipe: the slot's time is up |
| `too_many` | 4000 | pairing pipe: more than eight `pair.msg` from one socket |

A close with 1000 follows a `bye`. Devices treat unknown codes as errors and keep going.

## 16. Pairing pipe

A device that is not paired yet is no member, so it cannot authenticate (section 7) or
send an envelope. Pairing therefore meets on a rendezvous of its own, with no Ring, no
authentication, no presence and no quota or entitlement (pairing must work before anyone
buys a subscription): capability `pair`. The pipe only joins two sockets and forwards
opaque messages between them. **It is not a security boundary**: security comes from the
pre-shared key, the Desktop's single-use and expiry bookkeeping, and the joining device
pinning the chain (`SESSIONS.md`). The rules below keep the Relay tidy and bound abuse.

- **Path:** every request path that ends in `/v1/pair/{slot}`, with `slot` matching
  `^[A-Za-z0-9_-]{43}$`, as a WebSocket upgrade (426 without one). The Worker routes it to
  `idFromName("pair:" + slot)`. `slot` is `b64u(SHA-256("xshell-pair-slot-v1\n" ‖ secret))`.
- **Sequence:**

  ```text
  first socket        <- {"t":"pair.wait","v":1}
  second socket       -> both get {"t":"pair.peer"}
  third and later     <- error{code:"pair_busy"}, close 4010
  either side         -> {"t":"pair.msg","payload":"<b64u, at most 8192 bytes decoded>"}
                         forwarded unchanged to the other side
  ```

- **Single use.** Once two sockets have met, the slot is used up: every later open gets
  `pair_busy`, close 4010, even after both left, for as long as the slot's tombstone lasts
  (below). A socket refused for a protocol violation (`bad_request`, `unsupported`,
  `too_large`, `too_many`, under the limits per socket below) *burns* its slot: the slot is
  then used up just the same, met or not. A slot whose only socket just closed before anyone
  came is not burnt and may be opened again.
- **Lifetime.** A slot expires 600 s after its first open: a socket still on it gets
  `error{code:"pair_expired"}`, close 4008, and so does every later open while the tombstone
  lasts, unless the slot was used up or burnt: such a slot answers `pair_busy` for its whole
  tombstone, checked before the lifetime. Only a slot that expired without being used up
  answers `pair_expired`.
- **Limits per socket:** text frames of at most 16 KiB (else `too_large`, close 1009); at
  most 8 `pair.msg` (else `too_many`, close 4000); a `pair.msg` before the other side came,
  or whose payload is not canonical b64u of at most 8192 bytes, is `bad_request`, close
  4000. `{"t":"ping"}` works as in section 13. Any other `t`, and a binary frame, is
  `bad_request` (`unsupported` for binary), close 4000.
- **Peers.** When one side closes, the Relay closes the other with 1000.
- **Client address.** The rate limit and the slot cap below count opens by client address.
  The Relay takes it from the first of these that applies:
  1. on Cloudflare, `CF-Connecting-IP`, and only for a request that really came through
     Cloudflare's edge (a Worker sees `request.cf` with a `colo`). Cloudflare sets that header
     itself; on workerd or in the Docker image a client can send any value, so it is ignored
     there;
  2. a request header that a trusted reverse proxy sets, when the Relay is configured with its
     name (the Worker's `CLIENT_IP_HEADER`, the test Relay's `client_ip_header`): the last
     entry of its list (the entry the nearest proxy added, as in `X-Forwarded-For`). When such
     a header is configured, the Relay uses nothing else;
  3. the socket's peer address, when the runtime gives one (the test Relay; a Worker gets
     none);
  4. none: all such opens count against one shared bucket. So does an open whose value is
     not an IPv4 or IPv6 address (an IPv6 address may be in brackets).

  An IPv4-mapped IPv6 address (`::ffff:a.b.c.d`) counts as its IPv4 address. The rate limit
  counts an IPv4 address alone and an IPv6 address by its /64; the slot cap counts an IPv4
  address by its /24 and an IPv6 address by its /64 (the address's *prefix*). One subscriber
  usually holds a whole IPv6 /64, so rotating addresses inside it gains nothing.
- **Abuse limits** (the Relay's own; a device needs none of them), all before the upgrade:
  - at most 10 slot opens per client address per minute (`wire::PAIR_OPENS_PER_MINUTE`),
    else HTTP 429. An open refused with 429 is not counted;
  - an open of a *new* slot (one the Relay holds no state for: neither outstanding nor a
    tombstone) takes a reservation, counted against the prefix of its client address: at
    most 20 slots outstanding per prefix (`wire::PAIR_MAX_SLOTS_PER_PREFIX`) and at most 1000
    per Relay (`wire::PAIR_MAX_SLOTS`), else HTTP 503. The rate limit is checked first. An
    open of a slot that is outstanding or has a tombstone takes no reservation and is not
    capped: the slot itself answers it (`pair.peer`, `pair_busy` or `pair_expired`).
- **Reservations.** A slot is *outstanding*, and holds its reservation, from its first open
  until one of these releases it at once:
  - two sockets met on it, or a socket on it was refused for a protocol violation (it is
    used up or burnt);
  - it expired (600 s after its first open);
  - its first open was refused: the upgrade failed or never reached the slot.

  A slot whose only socket left before anyone came stays outstanding until it expires. A
  Relay that reserves in one place and admits in another (the Worker: a coordinating object
  and the slot's own) may also let a reservation the slot never confirmed lapse after a short
  time. One prefix thus holds at most 20 slots. This is the limit of the mitigation: an
  attacker with 50 prefixes (50 IPv4 /24s or IPv6 /64s, which a single cloud account or IPv6
  allocation can supply) can still fill the Relay's 1000 and keep new pairings refused with
  503 while they keep their slots outstanding.
- **Tombstones.** A used-up or burnt slot keeps a tombstone (that it was used) for 60 s
  (`wire::PAIR_TOMBSTONE`) from the meeting (or the refusal) or from its last socket leaving,
  whichever is later; a slot that expired unused keeps one for 60 s from its expiry. Opens in that time get
  `pair_busy` or `pair_expired`. A tombstone holds no reservation. Then the Relay forgets the
  slot, and a later open of it starts a new slot: the endpoints enforce single use and the
  lifetime themselves (`SESSIONS.md`).
- **Sockets.** A slot's Durable Object holds at most two sockets.

The test Relay implements all of this (the rate limit only when configured with
`pair_opens_per_minute`; the client address from the socket, or from `client_ip_header` when
configured); `TestRelayOptions::lax_pairing` turns single use and the lifetime off, to prove
that the endpoints enforce them themselves. The contract scenarios `pair_pipe_*`,
`pair_prefix_cap_refuses_21st`, `pair_reservation_released_on_meeting` and
`pair_refusal_burns_slot` pin it (section 19).

## 17. Reserved for later versions

- `{"t":"state","foreground":bool}`, sent by Mobiles, which would add `foreground` to their
  presence;
- `{"t":"push",…}`, for push forwarding;
- caps `foreground`, `push`, `bin`. Binary frames stay reserved.

## 18. Worker checklist

- One Durable Object per Ring (`idFromName(ringId)`), using WebSocket Hibernation.
- Storage: `roster:<version>` (the token string), `head` (version number), `presence:<signKey>`
  (record plus the generation it describes), `gen:<signKey>` (the generation counter, never
  deleted), `entitlement`, `stage/<candidateId>/<n>` (staged chunks, section 7), the
  quota counter per UTC day, and `ring` (the Ring id, written at the first request: a
  Durable Object cannot reliably learn the name it was created from).
- On wake, rebuild the socket index from `getWebSockets()` and the attachments, and mark
  `dropped` every online presence record without a live socket of its generation
  (section 8).
- Attachment per socket: `{signKey, gen}` once authenticated; `{nonce, deadline,
  candidateId}` and the manifest `{frames, chunks, versions, bytes, stored}` before. Never
  the candidate itself.
- Before every side effect of an authenticated frame (`env`, `roster.put`,
  `entitlement.put`), check under the same storage transaction that the attachment's key is
  still in the head and its `gen` is still the key's current socket; otherwise answer
  `removed` (close 4004) or `replaced` (close 4009) and do nothing else.
- `webSocketMessage`: apply the size cap, parse strictly (section 2; `JSON.parse` is not
  enough), apply the quota (section 14), dispatch by `t`. Before auth, allow only
  `auth.chain`, `auth`, and `ping` (which arrives only when not written exactly, the exact
  bytes being auto-answered).
- Verify every signature strictly (section 2): decode keys and `R` with y taken mod p,
  refuse `S ≥ L` and small-order `R` and keys, and only then call WebCrypto.
- `webSocketClose` / `webSocketError`: mark the socket `dropped` unless a `bye` was seen,
  guarded by its generation (section 8).
- Use an alarm for the auth deadline and for sweeping stale `stage/` keys.
- Devices budget their reads per turn at the socket, below TLS, so records without
  plaintext cannot starve them; a Relay should do the same where its runtime allows. Optionally also sweep half-open sockets with
  `getWebSocketAutoResponseTimestamp`.
- Reuse the Push Gateway's `b64.ts` (its `fromB64u` must be strict as section 2 says,
  non-zero trailing bits included) and the shape of `entitlement.ts` for Roster tokens.
- Log `bad_signature` together with the origin the Relay expected.
- The pairing pipe (section 16): one Durable Object per slot (`idFromName("pair:" + slot)`),
  storage `firstOpen` and `used`, an alarm that closes sockets at expiry and deletes the
  state when its tombstone runs out, and in front of it the per-address rate limit and the
  slot caps (per prefix and per Relay), whose reservations the slot releases as soon as it
  is used up or burnt, expires or refuses its first open. The client address comes from
  `CF-Connecting-IP` only when `request.cf` is present, else from `CLIENT_IP_HEADER`.

## 19. Test vectors and the contract suite

`crates/protocol/testdata/ring/` holds deterministic vectors (fixed seeds and timestamps;
Ed25519 is deterministic):

| File | Contents |
|---|---|
| `keys.json` | seeds (hex), the derived `signKey`/`noiseKey`, the Ring id, and `noiseKeyRejects` (small-order and non-canonical X25519 encodings that must be refused) |
| `ed25519.json` | `signKeys` (`{key, ok}`: small-order points, every y ≥ p, an off-curve key) and `signatures` (`{key, message, sig, ok}`: valid, S + L, small-order R, small-order key), as `verify_strict` reads them |
| `roster-chain.json` | v1 → v2 (adds a Desktop, a Daemon, a Mobile) → v3 (signed by the second Desktop, removes the Mobile, moves the Relay): tokens, decoded payloads, hashes |
| `roster-reject.json` | `cases` (`{name, trusted[], candidate, error}`): stale, forged and tampered signatures, a malleated and a small-order-R signature, Mobile/Daemon/non-member signers, gap, `prev` mismatch, fork, wrong Ring id, genesis not self-derived or by a Mobile, no Desktop, duplicate keys, unusable member keys, integer lexemes, non-finite numbers, lone surrogates, invalid UTF-8, nesting depth, plain `ws://`, bad encodings; `accepted` (`{name, candidate}`): unusual but valid genesis tokens (floats and big numbers in unknown fields, depth 127, an escaped surrogate pair, a non-canonical member key) |
| `auth.json` | an origin, Ring id, nonce, key, the signed message and signature; origins that must not verify; origin normalization pairs |
| `urls.json` | `accept` (`{url, origin, endpoint}` for a fixed Ring id) and `reject` (`{url, why}`) Relay URLs |
| `frames.json` | one of every v1 frame; `intBounds`: frames at 2^53−1 (accepted) and one past it (refused); `lexemes`: client frames and whether they decode (`ok`), are `malformed`, or have bad fields (`invalid`) |
| `entitlement.json` | a gateway key, its kid, and tokens with the expected verification result (ok, wrong tier, expired, other Ring, unknown kid, bad signature, malformed) |

To regenerate them, run
`cargo test -p xshell-protocol --all-features --test ring_vectors -- --ignored bless_ring_vectors`.
A normal test run fails when the files are stale.

The contract scenarios in `ring::relay::contract` (feature `test-relay`) are public functions
taking a `RelayTarget { url, tls, auth_timeout, gateway, quota_frames_per_day,
pair_opens_per_minute, pair_ttl, client_ip_header }`. This repository runs them against the test
Relay. `xshell-remote` runs the same functions against the Worker on workerd, which makes them
the definition of a conforming Relay. Every scenario uses fresh random keys, so it needs no
reset. `contract::SCENARIOS` lists the single-Relay scenarios.
`ring_moves_to_new_relay_with_full_chain` takes two Relays. `entitlement_slot_round_trips`
assumes a Relay without the gateway key. `contract::HOSTED_SCENARIOS` run against a Hosted
Relay; their target's `gateway` holds the gateway signing key the Relay trusts, so they can
mint tokens (`hosted_routing_ends_when_the_last_token_expires` is the routing cutoff test).
`contract::QUOTA_SCENARIOS` (`quota_refuses_then_closes`) run against a Relay with a small
daily quota (at most 1000 frames), which their target's `quota_frames_per_day` names.
`SCENARIOS` includes the pairing pipe's `pair_pipe_joins_two`, `pair_pipe_refuses_third`,
`pair_pipe_caps_messages` and `pair_pipe_closes_peer`. `contract::PAIR_TTL_SCENARIOS`
(`pair_pipe_expires`, `pair_pipe_used_slot_stays_busy`) need a Relay with a slot lifetime of at most 5 s (`pair_ttl`), and
`contract::PAIR_RATE_SCENARIOS` (`pair_pipe_rate_limited`) one with a rate limit of at most
20 opens a minute (`pair_opens_per_minute`). `contract::PAIR_CAP_SCENARIOS`
(`pair_prefix_cap_refuses_21st`, `pair_reservation_released_on_meeting`,
`pair_refusal_burns_slot`) need a Relay with
the default per-prefix cap of 20. When the Relay trusts a client address header, the
target's `client_ip_header` names it, and these scenarios send addresses of their own in it,
each in fresh random prefixes; without one, they open from the runner's address, and each
needs a Relay on which nothing else from that prefix is outstanding.

**Contract runners raise the rate limit.** The pairing scenarios open many more slots from
one machine than the default 10 opens a minute allows. A runner therefore configures every
Relay it runs pairing scenarios on with a raised limit (at least 100 opens a minute; the
`xshell-remote` runner uses 10000), except the Relay for `PAIR_RATE_SCENARIOS`. The test
Relay has no rate limit unless configured with one.
