# One Daemon per user per Host, upgraded only when the user chooses

Desktops auto-install `xshelld` over SSH (VS Code-style), which would naturally lead to one Daemon per Desktop version. That would split a Host's Terminals into separate worlds and break ADR-0001, so there is exactly one Daemon per user per Host and Desktops of different versions share it. The protocol is versioned with additive changes and capability negotiation, so a newer Desktop normally keeps talking to an older running Daemon. A newer Desktop installs the new binary alongside and marks the Host as having an upgrade pending; the switch happens when the user triggers it (Terminals restart and resume their sessions) or at the Daemon's next natural restart. Only when protocol ranges do not overlap is the Host unusable until the user upgrades.

## Considered Options

- **Newest Desktop replaces the Daemon immediately.** Rejected: it kills agents mid-task on every Desktop because one of them updated.
- **Live handover of PTY file descriptors and scrollback to the new Daemon.** Deferred, not rejected: zero-interruption upgrades are possible on Unix (SCM_RIGHTS), but too complex for the first version.
