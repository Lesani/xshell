# Remote Hosts — design

Status: accepted, in implementation on `feat/remote-hosts`. Vocabulary is defined in [`CONTEXT.md`](../CONTEXT.md); the load-bearing decisions are [ADR-0001](adr/0001-remote-host-is-source-of-truth.md) (the Daemon owns a Remote Host's Terminals), [ADR-0002](adr/0002-reach-daemons-over-ssh.md) (transport is the system `ssh`) and [ADR-0003](adr/0003-one-daemon-per-host-deferred-upgrades.md) (one Daemon per user per Host, user-chosen upgrades).

## Goal

Agents running on another machine look and behave like local ones: the same sidebar, session lists, stats, git panel, file explorer and terminal tabs, with a small Host label as the only visible difference. Every Host-side feature has parity on a Remote Host.

## Shape

```
src-tauri/                      Cargo workspace root (target/ stays here)
├── Cargo.toml                  [workspace] members = [".", "crates/core", "crates/xshelld"]
├── src/                        Desktop (Tauri): commands, Local Host, Host connections
└── crates/
    ├── core/   (xshell-core)   Tauri-free: every Host-side feature, terminal command building,
    │                           protocol types + codec, RPC dispatch table
    └── xshelld/                the Daemon binary: socket server, Terminal registry, `connect` bridge
```

- `xshell-core` contains all logic that today lives in `lib.rs` except the Tauri glue. It must not depend on `tauri`, GTK or WebKit, so `xshelld` builds as a small static binary (`x86_64/aarch64-unknown-linux-musl`, `aarch64/x86_64-apple-darwin`).
- Functions that read the user's files take an explicit home directory (a small context value) instead of calling `dirs::home_dir()` deep inside, so tests can point them at fixture trees. The Tauri commands and the Daemon pass the real home.
- The Desktop's 38 non-terminal Tauri commands become thin wrappers over core functions. Behaviour of the Local Host is unchanged (ADR-0001: Local Host stays in-process and Desktop-owned).

## Protocol

One byte stream per connection (the `ssh` process's stdio on the Desktop; the Unix socket on the Daemon). Frames: `u32` big-endian length, then a `u8` kind, then the payload.

- kind `0` — JSON message (UTF-8). Every message has `"t"` (type). Requests carry `"id"`; responses echo it with `"ok"` or `"err"`. `id` is optional on every Desktop→Host message: without it the Host sends no response (the Desktop omits it on `term.input`/`term.resize`).
- kind `1` — Terminal output: 16-byte Terminal UUID followed by raw bytes. Output is never JSON-encoded.

Handshake: first message from each side is `hello { protocol: { min, max }, version, capabilities[] }`. The connection uses the highest common protocol version; no overlap → the Daemon replies with an error naming both ranges and closes. Changes are additive: unknown message types get an `err` response, unknown fields are ignored, and new features are gated on `capabilities`.

Messages (protocol 1):

| Direction | Type | Purpose |
|---|---|---|
| D→H | `call { id, method, params }` | Any Host-side command by its Tauri name and the same JSON params the frontend sends today (camelCase). One dispatch table in core serves both. |
| D→H | `term.open { id, spec }` | Start a Terminal from a launch spec (agent, session id, cwd, shell mode/id/command, flags, initial size, display metadata). The Desktop picks the Terminal UUID. |
| D→H | `term.attach { id, terminal }` | Subscribe; Daemon replies, then sends the replay buffer as kind-1 frames, then live output. |
| D→H | `term.detach`, `term.input`, `term.resize`, `term.close`, `term.update { meta }` | `term.update` records late-bound metadata (e.g. a session id linked after start) so restore resumes the right session. |
| H→D | `terminals { list }` | Full Terminal list; sent after `hello` and whenever it changes. The Desktop reconciles Tabs against it (ADR-0001). |
| H→D | `term.exit { terminal, code }` | Terminal ended. Sent to every connection, only after the process exited **and** its last output was read, so no output follows it. An attach to an ended Terminal gets reply, replay, then `term.exit`. |
| D→H | `daemon.upgrade` | Persist, end every Terminal, exit (ADR-0003). The next `connect` starts the new binary, which restores. |

## Daemon (`xshelld`)

- `xshelld connect`: connect to the per-user socket (`$XDG_RUNTIME_DIR/xshell/daemon.sock`, else `~/.xshell/run/daemon.sock`, mode 0700 directory); if absent, start `xshelld serve` detached (new session, stdio to a log file under `~/.xshell/log/`) and retry; then copy stdio ↔ socket until either side closes. `connect` itself holds no state.
- `xshelld serve`: single instance (lock file next to the socket). Holds the Terminal registry and serves any number of connections.
- `xshelld --version` prints the build version and protocol range, machine-readable.
- Each Terminal: a PTY from the same command-building code the Local Host uses, a reader thread feeding a per-Terminal **replay buffer** (ring of raw bytes, 2 MiB default) and every attached connection. Replay is trimmed forward to just after a newline and preceded by a terminal reset, so it never starts mid-escape. After an attach the Daemon forces a redraw by nudging the PTY size (rows−1, then back), because full-screen TUIs only repaint on `SIGWINCH`.
- Size follows the last connection that sent `term.input` or `term.resize` for that Terminal.
- Launch specs are persisted to `~/.xshell/daemon/terminals.json` on every change. On `serve` start, each persisted Terminal is relaunched with the agent's resume flag (raw shells start fresh in the same cwd) under the same UUID. A Terminal whose relaunch fails is dropped from the list.
- A Terminal whose process ends stays listed (with `exitCode`) until `term.close`, like a local Tab showing "[Session ended]", and is relaunched on restart.
- Closing (`term.close`) SIGHUPs the session leader's and the foreground job's process groups, then SIGKILLs each group still alive after a grace period; it removes the Terminal from the list and the state file. Connections dropping never ends anything. A missing cwd is an error on the Daemon (no silent fallback to `$HOME`).
- SIGTERM/SIGINT/SIGHUP end every Terminal and keep the state file. After a crash, `serve` ends the processes left over from the previous run (tracked by leader pid and start time) before relaunching, so an agent never runs twice.
- Dropped files (`save_dropped_file`) go to a per-user private dir: `$XDG_RUNTIME_DIR/xshell/tmp`, else `~/.xshell/tmp`.
- Idle exit: with zero Terminals and zero connections for 1 hour, `serve` exits.
- Image paste/drop (`save_dropped_file`) runs on the Host like any other call, so the path typed into the agent exists where the agent runs.
- Release builds of `xshelld` use the `release-daemon` profile (`panic = "unwind"`), so a panicking `call` becomes an `err` instead of killing the Daemon.

## Desktop

### Host connections (Rust)

- Hosts are configured in the settings store: `{ id, name, sshTarget, color?, daemonCommand? }`.
- One connection task per Host: spawns `ssh -T [-o BatchMode=yes] <sshTarget> <daemon command> connect`, performs the handshake, and multiplexes requests, Terminal output and events. The ssh program and arguments are built by one function so tests can substitute a direct local transport.
- Reconnect with exponential backoff (1 s → 60 s cap, reset on success, immediate retry on resume from sleep / network change when detectable). Status is published as a Tauri event: `connected | reconnecting | offline | upgrade-pending`, plus the last error text (stderr of ssh).
- **Install** (no `daemonCommand`): run `uname -sm` and `~/.xshell/server/<version>/xshelld --version` over ssh. If missing or incompatible, obtain the binary for that OS/arch — first a matching `xshelld-<target>` shipped next to the Desktop executable (dev and fork builds), else the GitHub Release asset for the Desktop's version (repo configurable at build time, default `MertPROJ/xshell`) — and upload it with `ssh <target> 'mkdir -p … && cat > … && chmod +x …'`. The Desktop downloads; the Host needs no internet.
- **Upgrade** (ADR-0003): if the running Daemon's version is older than the Desktop's and protocols overlap, the Desktop stages its version and reports `upgrade-pending`; the UI offers "Upgrade now", which sends `daemon.upgrade` and reconnects. With no overlap the Host is unusable until upgraded, and the UI says so.
- Tauri commands: `host_call(host, method, params)`, `host_term_open/attach/detach/input/resize/close/update`, `host_upgrade(host)`, `hosts_status()`. Terminal output for attached Tabs is delivered over the same `Channel` mechanism local Terminals use, so `TerminalTab` stays transport-agnostic.

### Frontend

- A single `hostInvoke(host, command, args)` replaces direct `invoke` for Host-side commands: Local Host → `invoke(command, args)`; Remote Host → `invoke("host_call", …)`. Desktop-only commands (`open_url`, `reveal_in_explorer`, `read_image_base64`, settings, updater) keep calling `invoke` directly; `reveal_in_explorer` is hidden for remote paths.
- **Project key**: a Project is identified by `(host, path)`. Local Projects keep the bare path as their key so existing `project_paths`, `sidebar_layout` and `project_icons` need no migration; remote Projects use a qualified key, with `toProjectKey`/`parseProjectKey` helpers as the only place that format is known.
- **Tabs for Remote Hosts are not persisted in `open_tabs`.** They come from the Daemon's `terminals` list. The Desktop caches the last list per Host (settings store) so that at startup, and while offline, those Tabs show immediately in a reconnecting state with their last output, input disabled. On every `terminals` message: attach new ones (open Tabs), close Tabs whose Terminal is gone, keep the rest. Group/split layout stays Desktop-local; a Terminal opened elsewhere appears as a standalone Tab.
- Sidebar: remote Projects sit wherever the user puts them, with a Host label; offline Hosts show cached session lists and stats marked stale. The Project picker lists discovered Projects from every connected Host, grouped by Host. New Terminals on an offline Host are refused with a clear message.
- Account-wide widgets: rate limits show the freshest reading across connected Hosts (same account); the cost summary sums connected Hosts.
- Settings → Hosts: add/edit/remove, test connection, status with the last error, "Upgrade now", detected agent CLIs per Host.

## Testing

- `cargo test` in core: session/project parsing against fixture home trees, protocol codec round-trips and malformed-frame handling, replay-buffer trimming, launch-spec persistence and restore arguments.
- `cargo test` in xshelld: integration tests that run `serve` in-process on a temp socket and drive it through the client codec — open/attach/replay, two connections on one Terminal, size-follows-last-input, close ends it everywhere, disconnect ends nothing, restart restores with resume flags (a fake `claude` script records its argv).
- Desktop Rust: host connection tests using the substitutable transport against a real `xshelld` built in the workspace; install path tested against a temp "remote home".
- CI end-to-end: start `sshd` on the runner with a generated key and connect to `localhost` through the real `ssh` path, including auto-install from the workspace-built binary.
- Vitest: project-key helpers, `hostInvoke` routing, Tab reconciliation against `terminals` lists, offline cache behaviour.
- `ci.yml` on every PR: `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test --workspace`, `tsc`, `vitest run`, musl build of `xshelld`.

## Out of scope (first version)

Windows as a Remote Host; Local Host through a Daemon; live Daemon handover on upgrade; Host discovery beyond SSH config aliases; home directories shared between Hosts (NFS) without `XDG_RUNTIME_DIR`, where two Daemons would share one socket and state file.
