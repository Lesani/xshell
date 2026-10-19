# A Relay move never waits for the old Relay

Design superseded by [ADR-0015](0015-change-relays-by-leaving-and-pairing-again.md): the target enrollment design requires per-device leave and pairing into a new Ring, with no move-ring function. The automatic move described below remains the current implementation until that change lands.

Until now a Desktop that changed its Ring's Relay stayed on the old Relay until that Relay had
acknowledged the new Roster version, and refused another change meanwhile ("it never moves
without the acknowledgement"). The intent was that devices still connected to the old Relay
learn of the move. In practice the old Relay is often gone: a self-hoster deleted the
container, a user leaves the Hosted Relay to go air-gapped and will never reach it again, or
(the case that surfaced this, 2026-10-16) a Ring was created on an address that never
existed. The Desktop then dialed a dead Relay forever, showed "Can't reach the relay" next to
the *new* URL, kept the move across restarts and refused any other Relay change.

Decision (Mario, 2026-10-16):

- **The move takes effect at once.** After a Relay change the Connector connects to the Relay
  the head names, whatever the old Relay's state.
- **Telling the old Relay is an owed farewell, kept and retried.** For each old Relay the
  Desktop records a farewell: publish the versions it lacks up to the current head, so the
  devices still there learn where the Ring went. The farewell is persisted, retried in the
  background on its own short-lived connection (backoff up to a long interval, for as long as
  xshell runs, across restarts), and dropped once that Relay acknowledged it. Every Relay is
  treated alike, the Hosted Relay and a self-hosted one; leaving the Hosted Relay is the same
  farewell as leaving one's own.
- **Farewells always carry the newest head.** Moving A → B → C while A is unreachable leaves
  farewells for A and B, each publishing up to C, so devices still on A go straight to C. A
  further change is never refused because a farewell is owed.
- **No farewell to a Relay that never held the Ring.** The Desktop records which Relays have
  acknowledged a version of this Ring; nobody can be waiting on one that never did (a phone or
  a `xshelld pair` computer joins only through a Relay that holds the Ring). Older state
  without that record counts as held, except the placeholder `wss://relay.xshell.app`, which
  never existed: a farewell owed to it is dropped on load.
- **The UI says what is owed, by name.** The connection line is about the Ring's Relay only.
  An owed farewell has its own line naming the old Relay and saying that devices still using
  it will move once it is reachable again; no generic "Can't reach the relay".

Consequences: a device that only knew the old Relay and stays offline until that Relay is
gone for good must be paired again; that is the price of never being stuck. An older build
that reads the state this build wrote (`farewells`, `heldRelays`) cannot parse it and moves it
aside, so going back to it means starting Mobile access over. The Daemon and the
Mobile are unchanged: they follow the head they receive. This supersedes the "Relay moves"
paragraph of `xshell-protocol`'s Connector docs and the move rules in RELAY.md that depend on
it.
