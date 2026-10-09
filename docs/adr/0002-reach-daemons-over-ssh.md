# Desktops reach Daemons over the system ssh, never a network port

The Daemon listens only on a Unix socket on its Host. A Desktop runs the system `ssh <host> xshelld connect`, which bridges the SSH session's stdio to that socket (starting the Daemon if it is not running), and speaks the xshell protocol over it. Authentication, encryption, host-key trust, jump hosts and `~/.ssh/config` aliases are therefore all SSH's — xshell ships no auth or TLS code and opens no ports. "Connect whenever online" is a reconnect loop with backoff around that ssh process.

## Considered Options

- **Daemon listens on TCP (WebSocket + TLS + pairing token, optionally Tailscale identity).** Rejected: it means owning a security surface (certificates, token pairing, revocation) and exposing a port on every Host, for no capability SSH lacks. Tailnet users are covered anyway through Tailscale SSH.
- **Embedded Rust SSH client instead of the system `ssh`.** Rejected: it would ignore the user's `~/.ssh/config`, agent, hardware keys and ProxyJump, which is exactly what makes "add the Host as the alias you already use" work.
