# Features negotiate; the session transport does not break

From the declared starting point on, devices of different versions must keep working together: a Terminal on an older or newer machine can always be seen, interacted with and remotely upgraded. A feature one side lacks (file transfer, say) is not offered, or answers with a message; it never hides the session. The one exception is an incompatible change of the base transport, reserved for cases such as a security flaw, and expected to be exceptionally rare: once the Hosted Relay carries paying customers, such a break drops sessions across the world.

Requiring one release on all devices would have removed much version-handling code, and was rejected for that reason.

Until the starting point is declared (after the Windows stage of the "Ring just works" work), nothing has to stay compatible: the only Ring is the developer's, and it may be re-formed at will. So the pre-existing Ring format is not migrated, and the Upgrade Ring step is removed rather than made reliable.
