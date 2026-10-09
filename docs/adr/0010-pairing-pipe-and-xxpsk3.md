# Pairing meets on an unauthenticated pipe on the Relay and runs Noise XXpsk3

A device that is not paired yet is in no Roster, so the Relay will not let it authenticate or send an envelope, and `xshelld pair` on a new computer knows neither a Ring nor a Desktop key nor even the Relay's Ring. Pairing therefore meets on a small rendezvous the Relay adds next to the Ring endpoint: the pairing pipe `/v1/pair/{slot}`, with no Ring, no authentication and no subscription check, where the slot is a hash of the one-time secret the Desktop shows (a QR code for a phone, a 16-character code that the user types on the Desktop for a computer). The pipe joins two sockets once and forwards opaque messages; it is not a security boundary. One handshake serves both flows, `Noise_XXpsk3_25519_ChaChaPoly_BLAKE2s` with the secret as pre-shared key, the joining device as initiator and the Desktop as responder; the phone also pins the Desktop's Noise key from the QR code. The Desktop uses the secret up on the first join, enforces its 10 minutes itself, adds the device in a new signed Roster version and publishes it before it answers, and the device then fetches the chain and pins that version. Live sessions stay `Noise_IK_25519_ChaChaPoly_BLAKE2s` as ADR-0004 says. RELAY.md section 16 and `crates/protocol/SESSIONS.md` hold the wire format; the Worker implements the pipe in `xshell-remote`.

## Considered Options

- **A temporary "pairing member" in the Roster, derived from the secret.** Rejected: if the Desktop crashed before it removed that member, a leaked QR code would stay a permanent `mobile` credential.
- **IKpsk2 (the joiner knows the Desktop's key in advance).** Rejected: it fits the QR flow but not `xshelld pair`, which has nothing but the code; two handshakes would double the code to review.
- **A short code (6 to 8 characters) with a PAKE such as SPAKE2 or CPace.** Rejected for now: no audited PAKE in the dependency set. Without one, a malicious Relay posing as the other side can record message 3 and guess the code offline, and the slot hash is itself a verifier, so the expiry does not bound that attack: 80 bits of code (2^80 guesses) does.

## Consequences

- The pipe is free on the Hosted Relay, so a Ring can pair the phone that later buys its subscription.
- The Relay bounds abuse itself: a per-address rate limit before the upgrade, caps on outstanding slots per address prefix and per Relay (a slot's reservation is released as soon as it is used up or expires), a short tombstone for used-up slots, two sockets per slot.
- A pairing secret lives only in the Desktop's memory; an app restart invalidates every offer.
