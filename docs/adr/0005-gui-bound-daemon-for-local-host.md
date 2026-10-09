# The Local Host runs its Terminals in a GUI-bound Daemon

For a Mobile to reach agents started at a machine's own xshell, that machine's Terminals must live in a Daemon rather than inside the Desktop process. The Desktop therefore starts a GUI-bound Daemon (a child `xshelld serve`) and runs its Local Host Tabs through it with Remote Host semantics (ADR-0001): Daemon-owned, visible to every Desktop of the Ring, restored with resume flags on the next start. Quitting the Desktop ends the GUI-bound Daemon and its Terminals, so agents still never keep running (and spending) after quit unless the user opts in to a Persistent Daemon. A machine without a running Daemon is simply offline to Mobiles; nothing over the Relay ever starts one. This revises ADR-0001's "Local Host stays in-process and Desktop-owned" while keeping its reason.

## Consequences

- The GUI-bound Daemon is required on every platform the Desktop supports, Windows included, so `xshelld` needs a Windows port of its core: a named pipe ACL'd to the user instead of the Unix socket, ConPTY through `portable-pty`, and a Job Object with kill-on-close instead of process groups, which also ties every agent to the GUI's lifetime by OS guarantee. The Persistent Daemon (detached, surviving crashes and reboots) remains Linux and macOS only until a later version.
