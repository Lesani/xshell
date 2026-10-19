# Change relays by leaving and pairing again

The desktop-adoption design deliberately has no move-ring function: to use another Relay, each device must explicitly leave its old Ring and register in a new Ring, rather than migrating the existing Ring or automatically moving other devices. This replaces ADR-0012's automatic relay-move design with an explicit per-device trust transition; a configured custom Relay URL selects where an unpaired device creates or joins a Ring, never an override for an enrolled device's Ring.

Leaving takes effect locally without waiting for the old Relay. A durable Pending Leave remains visible in the Desktop window and retries registration of the departure with that Relay across restarts, without blocking enrollment in another Ring; this obligation removes old membership, unlike the legacy Farewell that advertised a Ring move.

This is the agreed target behavior; the existing automatic relay-move implementation has not yet been removed.
