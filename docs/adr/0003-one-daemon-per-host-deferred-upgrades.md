# One Daemon per user per Host, upgraded only when the user chooses

Desktops auto-install `xshelld` over SSH (VS Code-style), which would naturally lead to one Daemon per Desktop version. That would split a Host's Terminals into separate worlds and break ADR-0001, so there is exactly one Daemon per user per Host and Desktops of different versions share it. The protocol is versioned with additive changes and capability negotiation, so a newer Desktop normally keeps talking to an older running Daemon. A newer Desktop installs the new binary alongside and marks the Host as having an upgrade pending; the switch happens when the user triggers it (Terminals restart and resume their sessions) or at the Daemon's next natural restart. Only when protocol ranges do not overlap is the Host unusable until the user upgrades.

## Note: builds of one version (#49)

The same applies between builds of one version: a Desktop whose build differs from the running Daemon's offers "Replace with this Desktop's build" and never switches by itself. Builds are installed side by side (`~/.xshell/server/<version>+<build>`). An upgrade or a replacement writes the Host-wide record `~/.xshell/server/selected`, naming the build to start, just before it asks the Daemon to exit; installing never writes it. `xshelld connect` (and a Persistent Local Daemon) starts that build when no Daemon runs, unless it is of an older version than the connector itself, so a newer version still takes over at the next natural restart. The one-Daemon lock and socket do not depend on the build. A connector from before #49 ignores the record; a Desktop that then still sees another build says so and lets the user replace again.

## Considered Options

- **Newest Desktop replaces the Daemon immediately.** Rejected: it kills agents mid-task on every Desktop because one of them updated.
- **Live handover of PTY file descriptors and scrollback to the new Daemon.** Deferred, not rejected: zero-interruption upgrades are possible on Unix (SCM_RIGHTS), but too complex for the first version.
