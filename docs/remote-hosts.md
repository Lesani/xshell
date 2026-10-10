# Remote Hosts — design

Status: accepted, in implementation on `feat/remote-hosts`. Vocabulary is defined in [`CONTEXT.md`](../CONTEXT.md); the load-bearing decisions are [ADR-0001](adr/0001-remote-host-is-source-of-truth.md) (the Daemon owns a Remote Host's Terminals), [ADR-0002](adr/0002-reach-daemons-over-ssh.md) (transport is the system `ssh`) and [ADR-0003](adr/0003-one-daemon-per-host-deferred-upgrades.md) (one Daemon per user per Host, user-chosen upgrades).

## Goal

Agents running on another machine look and behave like local ones: the same sidebar, session lists, stats, git panel, file explorer and terminal tabs, with a small Host label as the only visible difference. Every Host-side feature has parity on a Remote Host.

## Shape

```
src-tauri/                      Cargo workspace root (target/ stays here)
├── Cargo.toml                  [workspace] members = [".", "crates/core", "crates/protocol", "crates/xshelld", "crates/hostlink"]
├── src/                        Desktop (Tauri): commands, Local Host, Host connections
└── crates/
    ├── protocol/ (xshell-protocol)  wire protocol: frame codec, messages, negotiation; no PTY/SQLite/Tauri
    ├── core/   (xshell-core)   Tauri-free: every Host-side feature, terminal command building,
    │                           RPC dispatch table
    └── xshelld/                the Daemon binary: socket server, Terminal registry, `connect` bridge
```

- `xshell-core` contains all logic that today lives in `lib.rs` except the Tauri glue. It must not depend on `tauri`, GTK or WebKit, so `xshelld` builds as a small static binary (`x86_64/aarch64-unknown-linux-musl`, `aarch64/x86_64-apple-darwin`).
- Functions that read the user's files take an explicit home directory (a small context value) instead of calling `dirs::home_dir()` deep inside, so tests can point them at fixture trees. The Tauri commands and the Daemon pass the real home.
- The Desktop's 38 non-terminal Tauri commands become thin wrappers over core functions. Behaviour of the Local Host is unchanged (ADR-0001: Local Host stays in-process and Desktop-owned).

## Protocol

One byte stream per connection (the `ssh` process's stdio on the Desktop; the Unix socket on the Daemon). Frames: `u32` big-endian length, then a `u8` kind, then the payload.

- kind `0` — JSON message (UTF-8). Every message has `"t"` (type). Requests carry `"id"`; responses echo it with `"ok"` or `"err"`. `id` is optional on every Desktop→Host message: without it the Host sends no response (the Desktop omits it on `term.input`/`term.resize`).
- kind `1` — Terminal output: 16-byte Terminal UUID followed by raw bytes. Output is never JSON-encoded.

Handshake: first message from each side is `hello { protocol: { min, max }, version, capabilities[] }`. The connection uses the highest common protocol version; no overlap → the Daemon replies with an error naming both ranges and closes. Changes are additive: unknown message types get an `err` response, unknown fields are ignored, and new features are gated on `capabilities`. A Mobile performs the same `hello` inside its Noise session; its `capabilities` list only what that app handles, and the Daemon does not use them yet.

Messages (protocol 1):

| Direction | Type | Purpose |
|---|---|---|
| D→H | `call { id, method, params }` | Any Host-side command by its Tauri name and the same JSON params the frontend sends today (camelCase). One dispatch table in core serves both. |
| D→H | `term.open { id, spec }` | Start a Terminal from a launch spec (agent, session id, cwd, shell mode/id/command, flags, initial size, display metadata). The Desktop picks the Terminal UUID. Optional `firstMessage` (capability `term.first-message`, Unix Hosts only): see "First messages" below. |
| D→H | `term.attach { id, terminal }` | Subscribe; Daemon replies `{ exitCode, cols, rows }` (`cols`/`rows`: the size applied to the PTY now), then sends the replay buffer as kind-1 frames, then live output. |
| D→H | `term.detach`, `term.input`, `term.resize`, `term.close`, `term.update { meta }` | `term.update` records late-bound metadata (e.g. a session id linked after start) so restore resumes the right session. |
| H→D | `terminals { list }` | Full Terminal list; sent after `hello` and whenever it changes. The Desktop reconciles Tabs against it (ADR-0001). Each entry: `terminal`, `spec`, `meta`, `createdAtMs`, `pid`, `exitCode`, and the optional entry fields below. On a Mobile connection the list holds only direct agent Terminals (no shell, shell command, shell id or launch prefix). |
| H→M | `term.size { terminal, cols, rows }` | The size applied to the Terminal's PTY changed (never for the redraw nudge). Sent only to a Mobile attached to the Terminal, decided by the connection's role. Capability `term.mobile`. |
| H→D | `term.exit { terminal, code }` | Terminal ended. Sent to every connection that is told about the Terminal (a Mobile only for a direct agent), only after the process exited **and** its last output was read, so no output follows it. An attach to an ended Terminal gets reply, replay, then `term.exit`. |
| D→H | `term.relaunch { id, terminal, skipPermissions }` | Relaunch the Terminal with the agent's skip-permissions flag on or off. Gated on the `term.relaunch` capability. Replies `{ pid, relaunched }`; `relaunched` is false when the value already applies. Attached connections see the old output, a reset, then the new process's output, and no `term.exit`; the `terminals` list shows the same UUID with the new pid and spec. If the old process has ended but the new one cannot start, the Terminal stays listed as ended with its spec unchanged (`term.exit`, then `terminals`, then the `err`). |
| D→H | `daemon.upgrade` | Persist, end every Terminal, exit (ADR-0003). The next `connect` starts the new binary, which restores. |
| D→H | `ring.identity { id }` | The Host's Ring identity: `{ signKey, noiseKey, name, ring }`, where `ring` is `null` or `{ ringId, version, relayUrl, state, error? }` (`state`: `connecting`, `connected`, `waiting`, `stopped`). Creates the Host's device keys in `~/.xshell/daemon/ring/` on first use; the private keys never leave the Host. Desktop only; gated on the `ring` capability. |
| D→H | `ring.join { id, rosters, expect? }` | Join or follow a Ring: the whole Roster chain from version 1, as tokens. The chain must verify and its head must list this Host as a `daemon` (else `not a member of this Roster`); a chain of the Ring already joined must extend the stored one (else `roster refused: stale` or `roster refused: prev_mismatch`), and a chain of another Ring replaces the membership. With `expect: { ringId \| null, version? }` (capability `ring.cjoin`) the join is conditional: refused with `membership changed`, nothing stored, unless the membership `ring.identity` would report now is that Ring (at that head version, when given) or, for `null`, none; checked atomically with the commit. Replies `{ version }`. The Host then keeps a Relay connection, reconnects with the backoff, and says `bye` with `quit`, `idle` or `upgrade` when it exits. Desktop only; gated on the `ring` capability. |
| M→H | `push.register { id, blob, sealKey, triggers }`, `push.unregister { id }` | A Mobile's push registration, over its Relay session: the Push Gateway's blob, the X25519 key pushes are sealed to (never its session key) and `{ needsYou, finished }`. Replaces its previous registration; replies `null`. Anyone but a Mobile gets `push.register is for a Mobile`. Gated on the `push` capability; see `crates/protocol/PUSH.md`. |
| M/D→H | `session.subscribe { id, terminal, limit? }` | Follow an agent Terminal's conversation (capability `session.stream`). Replies with its newest page `{ gen, session, items, before? }` (at most `limit` entries, default 50, at most 200), then sends `session.append`s for that connection. Replaces the connection's earlier subscription to that Terminal. Refused like any message about a Terminal (`unknown terminal …`, `forbidden for mobile: …`), with `no session stream for this agent` for anything but a direct Claude or Codex, and with `too many session subscriptions` past 8 per connection. |
| M/D→H | `session.page { id, terminal, gen, before, limit? }` | An older page of the subscription: the entries before the cursor `before` (from a page or a reset), same shape. `session changed` when `gen` is no longer the subscription's, `not subscribed` without one. |
| M/D→H | `session.unsubscribe { id, terminal }` | End the subscription; replies `null` (also when there was none). No `session.append` for it follows the reply. |
| H→D | `session.append { terminal, gen, reset, session, items, before? }` | New entries of a subscribed conversation, in order. With `reset: true` the subscription moved to generation `gen` (another session linked, the session file replaced, truncated or newly there): `items` is its newest page and replaces everything shown, `before` continues it. |

Optional `terminals` entry fields (absent when they do not apply and from older Daemons; a value a peer cannot read decodes as absent and never fails the list):

- `agentStatus`: `working`, `needs-you`, `finished` or `ended`, as the agent's hooks report it. Capability `agent.status`. Absent for shells, agents without hooks and before the first report; a Relaunch starts without one, and it is not persisted across a Daemon restart.
- `statusAtMs`: when `agentStatus` last changed, in Unix milliseconds of the Daemon's clock. Present exactly when `agentStatus` is. Strictly increasing per Terminal, also across a Relaunch (if the clock stands still or goes back, the next value is the previous one plus 1); a Mobile compares it to the value it last saw to tell a new turn.
- `lastLine`: `{ from: "user" | "agent", text }`, the newest text message of the agent's session, on one line (runs of whitespace and control characters collapsed to one space) and at most 200 characters. Capability `agent.last-line`. Only for Claude and Codex run directly (no shell, shell command or launch prefix) with a session id, read from the end of the session file (`~/.claude/projects/<Project>/<id>.jsonl`, the Codex rollout ending in `-<id>.jsonl` under `~/.codex/sessions`). The file must be a regular file whose real path stays inside that storage; otherwise there is no line. The Daemon reads it off-thread when the Terminal opens, restores or relaunches, when a `term.update` links another session, and on every accepted agent report or status change, then once more a second later for a transcript written after the hook. Every entry reserves room for these fields in the `terminals` list budget, so filling them in never grows the list past it.

### Session streams (capability `session.stream`)

The Chat View reads an agent Terminal's conversation through a subscription. The connection names a Terminal, never a file: the Daemon reads the same session file as `lastLine` (a Claude or Codex agent run directly, on a valid session id; the file must be a regular file whose real path stays inside the agent's session storage, checked on every open), and only for a Terminal the connection's role may act on.

- **Entries**: `{ id, atMs?, kind, … }`, oldest first. `kind` is `user` or `agent` (`text`, Markdown as written, at most 32 Ki characters), `tool-call` (`call`, `name`, a one-line `summary` of at most 200 characters such as a command or path, `input` at most 2 Ki characters), or `tool-result` (`call`, `text` at most 4 Ki characters, `error`). `truncated: true` marks a cut text; `call` pairs a result with its call. Claude: text and tool blocks of user and assistant entries; thinking, `isMeta`, `isSidechain` and non-message entries are left out, injected wrappers (`<system-reminder>`, `<local-command-…>`, `<task-notification>`) too, a slash command shows as `/name args` and an image as `[image]`. Codex: `event_msg` user and agent messages and the `response_item` tool calls and outputs (a non-zero exit code is an `error`). A peer drops an entry it cannot read (an unknown `kind`) and keeps the rest. Caps are constants in `xshell-protocol`.
- **Ids and cursors**: `id` is `"<gen>:<offset>"` (`"<gen>:<offset>.<n>"` for further entries of one session line), unique within a generation and the same in pages and appends. `before` is an opaque byte cursor. A session line's entries are never split between messages.
- **Generations**: each subscription has a `gen`, renewed by every reset. Pages and appends carry it; a `session.page` for an older one is refused, so a stale page never mixes into a new conversation.
- **Bounds**: a page or append message is at most 256 KiB, serialized (one line alone always fits, its entries cut further if needed); a page scans at most 16 MiB of the file, an append read too; lines longer than 8 MiB are skipped, also when a read resumes inside one. Appends wait while the connection has more than 1 MiB queued, so a slow phone is never disconnected for them. Each connection may have 8 subscriptions and 16 queued session requests, and at most 256 requests are queued over all connections (beyond: `too many session requests`).
- **Live updates**: the Daemon checks each subscribed file every second (agents write during a turn, hooks fire only at its end) and at once on an agent report, an Agent Status change or a `term.update` relink. An older page is refused with `session changed` (and a reset follows) once the file no longer holds what the generation read. Results are checked again just before they are queued (the connection, the Terminal instance, the role's access, the session, agent and working directory), so nothing of a session the Terminal has left is sent after the change; a Terminal that is gone or no longer visible ends the subscription without a message (the `terminals` list says why). A Codex Terminal has no session until a Desktop links it: `session` is `null` and the first linked page comes as a reset.

### First messages (capability `term.first-message`)

A Mobile starts a new chat with its first prompt in `term.open`'s `firstMessage`. The Daemon puts it at the end of the agent's argv, after every other argument: `claude [--dangerously-skip-permissions] [--session-id <id>] [--settings <hooks>] -- <message>`, or `codex [flag] [-c <hook overrides>…] -- <message>`. The agent is executed directly, never through a shell.

- **Only a new direct chat**: Claude Code or Codex with no shell, shell command, shell id or launch prefix; Claude Code with no session id or one with no session file yet (`--session-id`), Codex with no session id. Anything else is refused with `a first message needs a new chat` or `a first message needs Claude Code or Codex, run directly`, and nothing starts. The check applies to every role, after a Mobile's usual checks (a known Project, else `forbidden for mobile: …`) and before any other.
- **The message**: at most 16 KiB of UTF-8 (`FIRST_MESSAGE_MAX_BYTES`), not blank, no NUL. A message without whitespace gets one trailing space, because Claude Code still runs a subcommand named by the first word after `--` (`claude -- update` updates) and no subcommand name contains whitespace.
- **Never persisted**: the message is not part of the launch spec. It is not in the state file or the `terminals` list, and a restore or Relaunch never sends it again.
- **Visible to local users**: while the agent runs, its argv, and so the message, can be read by other users of the Host (`ps`, `/proc/<pid>/cmdline`). On a Host shared with other users, a Mobile's first message is as visible as any command line.
- **Unix only**: on Windows a direct agent runs through `cmd.exe /C`, which would parse the message as a command line, so a Windows Daemon does not advertise the capability and refuses `firstMessage` with `a first message is not supported on Windows hosts`.

A peer that sends `firstMessage` to a Daemon without the capability gets a plain new chat (unknown fields are ignored), so the Mobile only offers it to Hosts that advertise `term.first-message`.

## Daemon (`xshelld`)

- `xshelld connect`: connect to the per-user socket (`$XDG_RUNTIME_DIR/xshell/daemon.sock`, else `~/.xshell/run/daemon.sock`, mode 0700 directory); if absent, start `xshelld serve` detached (new session, stdio to a log file under `~/.xshell/log/`) and retry; then copy stdio ↔ socket until either side closes. `connect` itself holds no state.
- `xshelld serve`: single instance (lock file next to the socket). Holds the Terminal registry and serves any number of connections.
- `xshelld --version` prints the build version and protocol range, machine-readable.
- Each Terminal: a PTY from the same command-building code the Local Host uses, a reader thread feeding a per-Terminal **replay buffer** (ring of raw bytes, 2 MiB default) and every attached connection. Replay is trimmed forward to just after a newline and preceded by a terminal reset, so it never starts mid-escape. After an attach the Daemon forces a redraw by nudging the PTY size (rows−1, then back), because full-screen TUIs only repaint on `SIGWINCH`.
- Size follows the last connection that interacted with that Terminal: a Desktop by `term.input` or `term.resize`, a Mobile only by `term.input`. A Mobile's `term.resize` records the size of its Terminal View and applies it only once that Mobile types (at once while it already holds the size); a Mobile that never sized a view of the Terminal (a reply from its Chat View) types without taking the size. When a Mobile that holds the size detaches or disconnects, the size goes back to the Desktop that held it most recently and is still attached with a recorded size; otherwise it stays.
- Mobile connections (capability `term.mobile`):
  - Terminal output is paced per connection: at most one frame per Terminal per second, and per 100 ms for 3 s after the Mobile's own `term.input`. Every other message goes at once. An attach's `res` and replay go at once; `term.exit` and an attach's `res` take the output of their Terminal queued before them along, so nothing crosses them; `term.size` waits for the output of its Terminal queued before it, and that Terminal's output queued after it waits for the next interval (one output frame per Terminal per interval). Held output counts against the connection's caps; on overflow, if anyone else is attached, the queued output of that Terminal is replaced by a fresh replay tail instead of the redraw nudge (a queued `term.size` stays). `term.detach` drops the Terminal's output still queued for that Mobile.
  - A Mobile's replay (on attach and after a Relaunch) is the last 256 KiB of the buffer at most, trimmed like the full one.
  - A Mobile's attach nudges the PTY only when no other connection is attached to the Terminal: the nudge would reflow every attached screen.
- Launch specs are persisted to `~/.xshell/daemon/terminals.json` on every change. On `serve` start, each persisted Terminal is relaunched with the agent's resume flag (raw shells start fresh in the same cwd) under the same UUID. A Terminal whose relaunch fails is dropped from the list.
- A Terminal whose process ends stays listed (with `exitCode`) until `term.close`, like a local Tab showing "[Session ended]", and is relaunched on restart.
- `term.relaunch` ends the process the way `term.close` does (SIGHUP, then SIGKILL after the grace period) and starts the changed spec under the same UUID once it has exited and its output is drained, resuming the agent's session. Only agent Terminals whose agent has a skip-permissions flag (`claude`, `codex`) and a session to resume can be relaunched; raw shells, ended or closing Terminals and a Terminal already relaunching are refused. The requested value is persisted as soon as the request is accepted, so a restart in the middle restores with it. A `term.close` during a relaunch wins.
- Closing (`term.close`) SIGHUPs the session leader's and the foreground job's process groups, then SIGKILLs each group still alive after a grace period; it removes the Terminal from the list and the state file. Connections dropping never ends anything. A missing cwd is an error on the Daemon (no silent fallback to `$HOME`).
- SIGTERM/SIGINT/SIGHUP end every Terminal and keep the state file. After a crash, `serve` ends the processes left over from the previous run (tracked by leader pid and start time) before relaunching, so an agent never runs twice. Only processes confirmed to be that run's (leader pid plus start time; on Linux signalled through pidfds) are signalled; if leftovers cannot be ended or confirmed gone, the Terminal is not relaunched but listed as exited (`exitCode` -1) with its record kept until `term.close`.
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
- Tauri commands: `host_call(host, method, params)`, `host_term_open/attach/detach/input/resize/close/update/relaunch`, `host_upgrade(host)`, `hosts_status()`. The status carries the Daemon's `daemonCapabilities`; `host_term_relaunch` is refused without sending anything when they lack `term.relaunch`. Terminal output for attached Tabs is delivered over the same `Channel` mechanism local Terminals use, so `TerminalTab` stays transport-agnostic.

### Frontend

- A single `hostInvoke(host, command, args)` replaces direct `invoke` for Host-side commands: Local Host → `invoke(command, args)`; Remote Host → `invoke("host_call", …)`. Desktop-only commands (`open_url`, `reveal_in_explorer`, `read_image_base64`, settings, updater) keep calling `invoke` directly; `reveal_in_explorer` is hidden for remote paths.
- **Project key**: a Project is identified by `(host, path)`. Local Projects keep the bare path as their key so existing `project_paths`, `sidebar_layout` and `project_icons` need no migration; remote Projects use a qualified key, with `toProjectKey`/`parseProjectKey` helpers as the only place that format is known.
- **Tabs for Remote Hosts are not persisted in `open_tabs`.** They come from the Daemon's `terminals` list. The Desktop caches the last list per Host (settings store) so that at startup, and while offline, those Tabs show immediately in a reconnecting state with their last output, input disabled. On every `terminals` message: attach new ones (open Tabs), close Tabs whose Terminal is gone, keep the rest. Group/split layout stays Desktop-local; a Terminal opened elsewhere appears as a standalone Tab.
- Sidebar: remote Projects sit wherever the user puts them, with a Host label; offline Hosts show cached session lists and stats marked stale. The Project picker lists discovered Projects from every connected Host, grouped by Host. New Terminals on an offline Host are refused with a clear message.
- Account-wide widgets: rate limits show the freshest reading across connected Hosts (same account); the cost summary sums connected Hosts.
- Settings → Hosts: add/edit/remove, test connection, status with the last error, "Upgrade now", detected agent CLIs per Host.

### Joining the Ring (#21)

With Mobile access on, every Host joins the Ring by itself after each connect (an install, an upgrade and a reconnect all end in one): the Desktop asks its Daemon for `ring.identity`, adds it to the Roster as a `daemon` member named after the Host's Settings → Hosts name, and sends `ring.join`; Desktop ↔ Daemon traffic stays on SSH, the Relay carries presence. Hosts that connect together are added in one Roster version, and the Daemons follow a new Relay URL or a renamed Host the same way. A Daemon without the `ring` capability is never asked (Settings → Mobile says to update it). A Remote Host already paired with another Desktop's devices is left there until the user chooses "Pair with this desktop"; joins are conditional (`ring.join` with `expect`, capability `ring.cjoin`), so two Desktops never take a Host from each other. A reinstalled Host's new key replaces its old member; a Host pointed at another machine keeps the old member until it is removed (#22). `xshelld pair`, for machines no Desktop reaches over SSH, comes with the pairing handshake (#9).

## Testing

- `cargo test` in xshell-protocol: codec round-trips, malformed-frame handling, message decoding and version negotiation.
- `cargo test` in core: session/project parsing against fixture home trees, replay-buffer trimming, launch-spec persistence and restore arguments.
- `cargo test` in xshelld: integration tests that run `serve` in-process on a temp socket and drive it through the client codec — open/attach/replay, two connections on one Terminal, size-follows-last-input, close ends it everywhere, disconnect ends nothing, restart restores with resume flags (a fake `claude` script records its argv).
- Desktop Rust: host connection tests using the substitutable transport against a real `xshelld` built in the workspace; install path tested against a temp "remote home".
- CI end-to-end: start `sshd` on the runner with a generated key and connect to `localhost` through the real `ssh` path, including auto-install from the workspace-built binary.
- Vitest: project-key helpers, `hostInvoke` routing, Tab reconciliation against `terminals` lists, offline cache behaviour.
- `ci.yml` on every PR: `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test --workspace`, `tsc`, `vitest run`, musl build of `xshelld`.

## Out of scope (first version)

Windows as a Remote Host; Local Host through a Daemon; live Daemon handover on upgrade; Host discovery beyond SSH config aliases; home directories shared between Hosts (NFS) without `XDG_RUNTIME_DIR`, where two Daemons would share one socket and state file.
