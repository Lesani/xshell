# Desktop Ring adoption

Status: agreed product design, not implemented. Decisions were resolved with Mario on 2026-10-19. Domain terms live in `../CONTEXT.md`; architectural decisions are recorded in ADR-0014 and ADR-0015.

## Goal and delivery order

An unpaired xshell on Windows, Linux, or macOS can join an existing Ring as a full computer Desktop, rather than merely enrolling its Daemon. The adopted Desktop can approve further devices without the original Desktop remaining online. Word-code adoption comes first; QR presentation and scanning follow using the same credential and adoption flow.

Current installations are development devices and will all be updated. Backward compatibility with old membership rules is not a delivery gate; public release is a separate, later gate. Preserve local data and normal local use. Broader attack prevention and denial-of-wallet work are deferred; the adoption cooldown below is in scope.

## Hosts settings

- Put Ring creation, joining, device approval, membership management, and an optional custom Relay URL in Hosts settings.
- Unpaired Desktops offer explicit Create ring and Join ring actions. Do not automatically create a Ring. Local-only use needs neither action.
- Create ring enrolls this Computer Device as the first full member.
- Join ring opens the code screen. Use the custom Relay URL if set, otherwise the Hosted Relay.
- Do not discover other Relays or introduce special self-hoster provisioning. A code offered on a different Relay produces the normal device-not-found result.
- An enrolled device uses its Ring's Relay. There is no move-ring function and no per-device URL override for an enrolled Ring. To change Relays, explicitly leave and register each device in a new Ring.

## Computer Device membership

A Computer Device groups its Desktop and Local Host Daemon. Pairing enrolls both without a second step; removal revokes both through one visible computer entry. Activity from either identity keeps the whole computer active. Phones and standalone headless Hosts track their own activity.

An adopted Desktop discovers all existing Ring Hosts and can access them through the Relay while their Daemons are online, without configuring SSH again. SSH credentials and desktop-specific Sidebar Layout are not copied. A GUI-bound Daemon remains reachable only while xshell runs; persistent Daemon support is unchanged.

Ring revocation does not revoke independently configured SSH credentials.

## Device Code and consent

- Eight independently random words from one fixed, versioned, curated 4,096-word English list: 96 bits of randomness. The list is shared across platforms and UI languages.
- Ignore capitalization; accept spaces or hyphens between words.
- A GUI code exists only while the join settings screen remains open, for at most ten minutes. Closing it or switching away to another xshell tab/window invalidates it. OS minimization and lock detection are outside scope.
- After expiry, show Generate new code; do not renew automatically. Reopening the join screen can generate a fresh code.
- Entering the code on an enrolled Desktop requests adoption. The joining Desktop displays the proposed Ring and approving device and requires explicit local confirmation before access is granted.
- Declining invalidates the code and offers a fresh one; it does not count as a failed attempt. Successful adoption consumes its one-time code.
- A future device-join QR contains only the same words plus a marker identifying a join-ring device code. No separate credential or Relay URL. Scanning uses the same consent, lifetime, and cooldown rules as manual entry.
- Preserve standalone headless `xshelld pair`, using the same word format. Its code exists only while the command runs, for at most ten minutes, and is invalidated on exit. It enrolls only the Daemon. The settings-screen lifetime rule applies to GUI pairing.

## Relay-enforced adoption cooldown

- Count failures per Ring across its approving devices, in a rolling one-minute window.
- The third failure starts a three-minute Ring-wide cooldown. Accept no new adoption attempts during that cooldown.
- Successful attempts do not count; rapid valid multi-device enrollment is not otherwise limited by this failure policy.
- An unknown or expired code, or failed cryptographic verification, counts as failure. User rejection, cancellation, and network outages do not.

## Membership authority

The Relay maintains and enforces the latest signed Roster, rejects unauthorized attachment, and does not forward unauthorized traffic to devices. Devices independently verify membership-change authorization and reject stale versions. A compromised Relay must not be able to insert an attacker's key and grant access.

- Computer Desktops have full membership authority.
- Mobiles may approve enrollment, including full computer Desktop enrollment, explicitly revoke active devices, and change inactivity policy, each after fresh biometric confirmation. Existing restrictions on shells, files, and git remain.
- Daemons may remove themselves or devices expired under the signed Ring policy. They cannot enroll devices, change policy, or remove other active members. Computer Device grouping still applies to self-removal.
- Revocation disconnects both computer identities' existing Relay sessions immediately and rejects reconnection. Peers close sessions when they receive the signed revocation. Local Terminals are not stopped.

## Inactivity expiry

- Signed, Ring-wide policy configured in Hosts, defaulting to 30 days; no Relay environment override.
- Support explicit Never expire as well as finite durations.
- Computers and biometric-confirmed Mobiles may change the policy. Daemons enforce it, not edit it.
- Authorized clients may trust Relay activity records. A compromised Relay can cause false expiry and deny availability, but cannot grant unauthorized access.
- A connected identity keeps its Computer Device active; expire it only when neither identity has been connected within the timeout.
- Expired and revoked enrollments never revive, including when the timeout is increased or disabled. Restoring access requires fresh pairing.
- Shortening the timeout applies immediately after showing affected devices and requiring confirmation. No additional grace period.
- No last-device exemption. If all devices expire, create a new Ring and pair again. Local Terminals and data remain intact.

## Leaving and changing Rings

An enrolled Computer Device must explicitly leave before joining another Ring. Never silently replace membership or merge Rings.

Leaving stops local participation by both identities and preserves local Terminals and settings. Persist a Pending Leave obligation to register removal with the old Relay, show it in the Desktop window, and retry across restarts. A Pending Leave never blocks enrollment in a new Ring. This is departure cleanup, not the legacy Farewell that advertises a moved Ring.

## Implementation seams to investigate

These are engineering tasks, not permission to weaken the decisions above:

- `src-tauri/crates/protocol/src/ring/roster.rs` and `chain.rs`: the current chain accepts only computer Desktop signers and requires a computer Desktop member. Introduce scoped change authorization, computer grouping, signed policy, and terminal expiry/revocation semantics without letting Daemons enroll devices or remove active peers.
- Protocol Pairing and Relay client: share the word-code credential between GUI, CLI, and future QR; support the joining Desktop's confirmation and both computer identities; ensure interrupted adoption does not grant unconfirmed access.
- `src-tauri/crates/hostlink/src/ring/desktop.rs`: adopt an existing Ring rather than generating one, manage Pending Leaves independently of new membership, remove automatic Relay moves, and discover Ring Hosts.
- `src/components/HostsSettings.tsx`, current `MobileSettings.tsx`, and `src/ring/`: place controls in Hosts, scope code lifetime to the relevant xshell screen/window, and surface expiry, consent, cooldown, and cleanup states.
- Relay and Mobile implementation live in the separate xshell-remote project; the local checkout currently contains its repository scaffold, not the implementation. Locate the active implementation before planning file ownership or claiming server/mobile behavior is tested.
- Relay protocol must enforce grouped membership, cooldowns, activity, expiry, and immediate disconnection while leaving enrollment authority with authorized devices.
- Preserve existing data across the development-fleet upgrade, or explicitly document any required re-pairing; no old-protocol compatibility layer is required.

## Verification obligations

Tests must cover:

1. Desktop-to-desktop adoption of an existing Ring on Windows, Linux, and macOS; both identities enrolled, full approval authority, and no dependency on the original Desktop being online.
2. Local confirmation before access, rejection invalidation, one-time consumption, expiry, explicit regeneration, GUI tab/window cancellation, and CLI exit cancellation.
3. The fixed word-list size, uniqueness, versioning, random selection, normalization, and common credential format; future QR is an alternate representation only.
4. Three failures within a rolling minute, three-minute cooldown across approving devices, unlimited successful attempts outside cooldown, and correct nonfailure classification.
5. Independent membership verification, rejection of unauthorized keys and stale membership, scoped Daemon changes, Mobile authority, and biometric gates.
6. Discovery of Ring Hosts without copied SSH credentials or layout; explicit paired-computer revocation disconnects both identities without stopping local Terminals.
7. Either computer identity maintaining activity, expiry of both as a unit, standalone Host/Mobile activity, Never expire, immediate confirmed policy reductions, and no resurrection or last-device exemption.
8. Offline leave followed by joining a new Ring, durable visible cleanup across restarts, and no implicit merge or Relay migration.
9. Compromised Relay activity reports affecting availability but not permitting unauthorized enrollment; race handling for concurrent membership changes and delayed cleanup from a prior enrollment.
