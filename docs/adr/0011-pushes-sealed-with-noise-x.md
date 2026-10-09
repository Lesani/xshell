# Pushes are sealed with Noise X to a per-Mobile seal key, and the Relay enforces foreground

A push travels Daemon → Relay → Push Gateway → APNs or FCM → the phone, and on iOS it is opened by the Notification Service Extension, a separate process that wakes for a few seconds. Only the phone may read it, the phone must know which Daemon sent it, and no push may wake a phone whose user is looking at the app. The Daemon seals each payload with the one-way handshake `Noise_X_25519_ChaChaPoly_BLAKE2s` from its own Noise static key to a **seal key**: a separate X25519 key the Mobile registers over its session (`push.register`, with the gateway's push blob and its triggers), never its session key, so the extension holds only a key that opens pushes and not the one that speaks for the device. The Daemon's static key travels encrypted inside the message; the Mobile maps it to a `daemon` member of its Roster. The prologue binds the Ring id, the plaintext is padded to one of four sizes, and the extension opens it in Rust through UniFFI (ADR-0007), because CryptoKit has no BLAKE2s. snow, already in the tree for the sessions, supports the pattern: no new dependency. The Daemon decides when to push and holds pushes back while a Mobile is in the foreground; the Relay checks foreground again when it forwards, because a Mobile can come to the front while a push is on its way. Foreground is a lease: a Mobile counts only while its socket was active in the last 75 s, so a suspended iPhone with a half-open socket stops blocking pushes on its own. `crates/protocol/PUSH.md` and RELAY.md sections 17 and 18 hold the wire format.

## Considered Options

- **Seal to the Mobile's session Noise key.** Rejected: the extension would need the key that impersonates the device in every session.
- **Sign each push with the Daemon's Ed25519 key, then encrypt.** Rejected for v1: it is a second construction to specify and to run in the extension. It would stop one thing the seal does not: a thief of one phone's seal secret, who can already read that phone's pushes, forging pushes to that phone. Registering a new seal key ends that, and the forged text reaches only the notification, never a Terminal.
- **CryptoKit in Swift (HPKE with X25519 and AES-GCM).** Rejected: a second implementation of the seal on one platform, against the shared Rust core of ADR-0007.
- **Foreground enforced by the Daemon alone.** Rejected: its view of presence can be seconds old, which is exactly when a user opens the app.

## Consequences

The limits are honest ones, and PUSH.md states them:

- The sender is authenticated relative to the recipient, not by a signature. Whoever holds a seal secret can open every push sealed to it and can also forge pushes to that recipient that appear to come from any Daemon of the Ring. A Relay, the gateway or a stranger cannot.
- Nothing is forward-secret: a seal secret opens every recorded push sealed to it, and registering a new seal key does not protect pushes recorded before.
- A recorded push opens again. The Mobile refuses replays by a `seq` that grows per Daemon and Mobile and by the push's time (`at`, at most 24 h old and 5 min ahead); the extension keeps the highest `seq` in shared storage.
- The padding leaves a coarse length class visible; the gateway and the providers learn when a Mobile is woken, never what about.
- A phone whose app is in front gets no push; once it goes quiet for 75 s without saying so, pushes resume.
