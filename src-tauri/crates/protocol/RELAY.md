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
  can lie about both. Until the Noise sessions (#9) authenticate the peer inside the
  payload, a device must treat them as claims. Devices drop envelopes whose `from` is not in
  their own trusted head.
- **Before authentication the Relay reveals** a Ring's head version (in `challenge`) to
  anyone who knows the Ring id. The Ring id is a hash of a public key and is not secret
  within the Ring, but it is not guessable from outside.

## 2. Encodings

- **base64url** (`b64u`): RFC 4648 §5 without padding. Decoders refuse padding (`=`), the
  standard alphabet (`+`, `/`), whitespace, a length ≡ 1 (mod 4), and non-zero trailing bits.
  Every byte string has exactly one accepted spelling, so keys compare as strings.
- **JSON**: every frame and every Roster payload is one JSON object. Duplicate keys at any
  depth are refused (JavaScript's `JSON.parse` keeps the last one and cannot detect them; a
  Worker that cannot refuse them must at least never re-encode a Roster). Every integer, in
  frames and in Roster payloads (ids, versions, timestamps, generations), is a non-negative
  integer of at most 2^53−1; one past it is refused. Fields are camelCase. Unknown fields are ignored on receipt (and, in a Roster,
  covered by its signature and kept).
- **Keys**: `signKey` is an Ed25519 public key (32 bytes, b64u, 43 characters). It must be a
  valid point and not of small order. `noiseKey` is an X25519 public key (32 bytes, b64u) in
  canonical form (u < 2^255−19, high bit clear) and not of small order: the u-coordinates 0,
  1, the two order-8 points (`e0eb7a7c…b800` and `5f9c95bc…1157`) and p−1 are refused, and
  their non-canonical forms, p and p+1, fail the canonical check. Signatures are Ed25519, 64 bytes, b64u (86 characters), verified strictly
  (`verify_strict`: canonical `S`, no small-order `R` or key). WebCrypto's Ed25519 verify
  accepts the same signatures for every signature this crate produces; a Worker should also
  refuse small-order keys when it parses a Roster.
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
| `malformed` | prefix, base64url, JSON, duplicate key, or a missing or mistyped field |
| `invalid` | a structural rule of 4.2 (other than the signature) |
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

- A device dials `{relayUrl without trailing /}/v1/ring/{ringId}` as a WebSocket upgrade. The
  Worker checks `ringId` against `^[A-Za-z0-9_-]{16,128}$` (else HTTP 404) and routes to
  `idFromName(ringId)`.
- **Text frames only.** A binary frame gets `error{code:"unsupported"}` and close 4000.
- Each frame is one JSON object tagged by `"t"`.
- An unknown `t` from the Relay is ignored by devices. An unknown `t` from an authenticated
  client gets `error{code:"unknown_type"}` and the socket stays open, so extensions stay
  additive.

**Frame size caps** (raw text length, checked before or right after parsing):

| Frame (client to Relay) | Cap |
|---|---|
| `auth.chain` | 1 MiB |
| `env` | 96 KiB (and its decoded `payload` at most 64 KiB) |
| `roster.put` | 72 KiB |
| every other frame | 8 KiB |
| any Relay to client frame (`roster.chain` included) | 1 MiB |

A frame over its cap gets `error{code:"too_large"}` and close 1009. An `env` whose decoded
payload is over 65536 bytes but whose frame is within the cap gets `error{code:"too_large",
to}` and the socket stays open. Malformed JSON (or no string `t`, or duplicate keys) gets
`error{code:"bad_request"}` and close 4000. A known type with bad fields gets
`error{code:"bad_request"}`; before authentication it also closes with 4000.

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
- If `auth` does not arrive within 10 s of the challenge, the Relay sends
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
   included;
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

A Relay configured with the Push Gateway's public key verifies the token with
`verifyEntitlement(token, ringId, now, keys)` (any tier; a refusal's `detail` is the
gateway's code: `malformed`, `bad_signature`, `unknown_kid`, `ring_mismatch`, `expired`).
A Relay without it stores the latest well-formed token: `xet1.`, a payload part that
decodes to a JSON object, and a part that decodes to 64 bytes, 4 KiB at most. The stored
token is returned in `welcome.entitlement`. `crates/protocol` implements the same check
(`ring::entitlement::verify_entitlement`); `testdata/ring/entitlement.json` pins it.

**Hosted Relay: limited sessions.** The Hosted Relay routes envelopes for a Ring only while
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

## 14. Quotas (amendment A3, Hosted Relay)

- Per Ring per UTC day, at most 2,000,000 client frames processed by the Relay. Every client
  frame the Durable Object handles counts. Auto-answered pings never reach it and do not
  count.
- The 64 KiB per-message limit is the envelope payload limit of section 10. Control frames
  have their own caps (section 6).
- Over the daily quota the Relay answers `error{code:"quota"}`. A connection that keeps
  sending is closed with 4029. Nothing is billed.

## 15. Errors and close codes

`error` frames: `{"t":"error","code":"…","id"?:n,"to"?:"<signKey>","detail"?:"…"}`.

| Code | Close | Meaning |
|---|---|---|
| `bad_request` | 4000 if malformed or before auth | bad frame, or bad envelope |
| `unsupported` | 4000 | binary frame |
| `unknown_type` | no | unknown `t` after auth |
| `auth_timeout` | 4008 | no `auth` within 10 s |
| `no_roster` | 4003 | no Roster stored and no candidate |
| `not_member` | 4003 | key not in the resulting head |
| `bad_signature` | 4001 | challenge answer does not verify |
| `roster_invalid` | 4003 at auth, no otherwise | a Roster failed the rules (`detail`) |
| `roster_stale`, `roster_conflict` | no | `roster.put` refused |
| `removed` | 4004 | a new head no longer lists this device |
| `replaced` | 4009 | the same key connected again |
| `offline`, `unknown_recipient`, `too_large` | no (`too_large` on a frame: 1009) | envelope refused |
| `quota` | 4029 if it persists | Hosted daily quota |
| `rate_limited` | no | slow down |
| `entitlement_required` | no | `env` in a limited session (section 12) |
| `entitlement_invalid` | no | `entitlement.put` refused |
| `internal` | optional | Relay failure |

A close with 1000 follows a `bye`. Devices treat unknown codes as errors and keep going.

## 16. Reserved for later versions

- `{"t":"state","foreground":bool}`, sent by Mobiles, which would add `foreground` to their
  presence;
- `{"t":"push",…}`, for push forwarding;
- caps `foreground`, `push`, `bin`. Binary frames stay reserved.

## 17. Worker checklist

- One Durable Object per Ring (`idFromName(ringId)`), using WebSocket Hibernation.
- Storage: `roster:<version>` (the token string), `head` (version number), `presence:<signKey>`
  (record plus the generation it describes), `gen:<signKey>` (the generation counter, never
  deleted), `entitlement`, `stage/<candidateId>/<n>` (staged chunks, section 7), and the
  quota counter per UTC day.
- Attachment per socket: `{signKey, gen}` once authenticated; `{nonce, deadline,
  candidateId}` and the manifest `{frames, chunks, versions, bytes, stored}` before. Never
  the candidate itself.
- Before every side effect of an authenticated frame (`env`, `roster.put`,
  `entitlement.put`), check under the same storage transaction that the attachment's key is
  still in the head and its `gen` is still the key's current socket; otherwise answer
  `removed` (close 4004) or `replaced` (close 4009) and do nothing else.
- `webSocketMessage`: apply the size cap, parse strictly, dispatch by `t`. Before auth,
  allow only `auth.chain`, `auth`, and `ping` (which never arrives, being auto-answered).
- `webSocketClose` / `webSocketError`: mark the socket `dropped` unless a `bye` was seen,
  guarded by its generation (section 8).
- Use an alarm for the 10 s auth deadline and for sweeping stale `stage/` keys.
- Devices budget their reads per turn at the socket, below TLS, so records without
  plaintext cannot starve them; a Relay should do the same where its runtime allows. Optionally also sweep half-open sockets with
  `getWebSocketAutoResponseTimestamp`.
- Reuse the Push Gateway's `b64.ts` (`fromB64u` is strict) and the shape of `entitlement.ts`
  for Roster tokens.
- Log `bad_signature` together with the origin the Relay expected.

## 18. Test vectors and the contract suite

`crates/protocol/testdata/ring/` holds deterministic vectors (fixed seeds and timestamps;
Ed25519 is deterministic):

| File | Contents |
|---|---|
| `keys.json` | seeds (hex), the derived `signKey`/`noiseKey`, the Ring id, and `noiseKeyRejects` (small-order and non-canonical X25519 encodings that must be refused) |
| `roster-chain.json` | v1 → v2 (adds a Desktop, a Daemon, a Mobile) → v3 (signed by the second Desktop, removes the Mobile, moves the Relay): tokens, decoded payloads, hashes |
| `roster-reject.json` | `{name, trusted[], candidate, error}`: stale, forged and tampered signatures, Mobile/Daemon/non-member signers, gap, `prev` mismatch, fork, wrong Ring id, genesis not self-derived or by a Mobile, no Desktop, duplicate keys, unsafe integers, plain `ws://`, bad encodings |
| `auth.json` | an origin, Ring id, nonce, key, the signed message and signature; origins that must not verify; origin normalization pairs |
| `frames.json` | one of every v1 frame; `intBounds`: frames at 2^53−1 (accepted) and one past it (refused) |
| `entitlement.json` | a gateway key, its kid, and tokens with the expected verification result (ok, wrong tier, expired, other Ring, unknown kid, bad signature, malformed) |

To regenerate them, run
`cargo test -p xshell-protocol --all-features --test ring_vectors -- --ignored bless_ring_vectors`.
A normal test run fails when the files are stale.

The contract scenarios in `ring::relay::contract` (feature `test-relay`) are public functions
taking a `RelayTarget { url, tls, auth_timeout, gateway }`. This repository runs them against the test
Relay. `xshell-remote` runs the same functions against the Worker on workerd, which makes them
the definition of a conforming Relay. Every scenario uses fresh random keys, so it needs no
reset. `contract::SCENARIOS` lists the single-Relay scenarios.
`ring_moves_to_new_relay_with_full_chain` takes two Relays. `entitlement_slot_round_trips`
assumes a Relay without the gateway key. `contract::HOSTED_SCENARIOS` run against a Hosted
Relay; their target's `gateway` holds the gateway signing key the Relay trusts, so they can
mint tokens (`hosted_routing_ends_when_the_last_token_expires` is the routing cutoff test).
