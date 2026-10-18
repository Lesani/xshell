# Windows SSH bridge rough design (BINDING)

Issue: Lesani/xshell#57. ADR-0013 is binding.

## Scope and user model

A Desktop adds a Windows Remote Host via an SSH alias and an explicit Daemon
command. Windows xshell must remain open. Native Windows agents run under its
existing GUI-bound Daemon and can be viewed and controlled from another Desktop.
No WSL, no new persistent service, no automatic Windows installation, no change
to protocol or agent lifetime. Do not claim arbitrary external terminals can be
adopted: control concerns Daemon-owned Terminals.

## Architecture

Add a Windows-only implementation behind `xshelld connect`, attaching through
`xshell_core::pipe` to the same resolved per-user endpoint used by the Desktop.
Reuse its security/ownership checks, bounded connection behavior and shutdown.
Keep Unix code unchanged except necessary platform-neutral comments/tests.
Bridge raw stdin/stdout with no PowerShell pipeline in deployment. Handle EOF
and either-direction failure by prompt cancellation and process completion;
pipes lack half-close. Audit blocked stdin cancellation, partial writes, output
flush behavior, and inherited handles. Missing daemon uses NOT_RUNNING_EXIT and
NOT_RUNNING_MESSAGE, without spawning anything.

## Storage, protocol, frontend

No new storage or protocol. No frontend feature needed: existing advanced Daemon
command bypasses Unix auto-install. Document Windows command quoting and the
Desktop-open requirement. Update contradictory docs and CLI tests narrowly.
Do not widen automatic replacement/upgrade behavior for managed commands.

## Tests

Windows integration: daemon absent; exact binary bytes both ways (including NUL
and non-ASCII); handshake + RPC; stdin EOF; daemon-side close while stdin stays
open; disconnect leaves daemon/Terminal alive; no hanging worker/process. Existing
Windows GUI-bound, security, shutdown tests and Unix bridge tests must pass.
Real Forge Windows OpenSSH: handshake and RPC via executable command directly,
open a native test shell/Pi Terminal, detach/reconnect, confirm same terminal and
process, close the test terminal. Native Windows build/test required.

## Work order and ownership

Single implementation seam: daemon Windows SSH bridge + scoped tests/docs, one
writer owns all affected source. Parent owns design, planning review, integration,
real-device build/deployment, private homelab config, publication. Planner is
read-only and supplies exact file changes, ordered steps, test names, risks,
code snippets only for cancellation and tricky Windows I/O. Writer must not
modify core pipe security/transport without a concrete need and parent review.
No changes to user configs, private host names or credentials in this public repo.

## Environment

Feature worktree based on origin/dev a887d35, feature branch
feat/windows-ssh-bridge. No upstream PR. Parent squash-merges into dev after
review/validation; feature commits use Co-Authored-By trailers. Read CONTEXT.md,
CONTRIBUTING.md, relevant ADRs, daemon cli/connect/paths and core pipe APIs.
Use relative paths, preserve others' work, no global toolchain changes. A native
Windows test host is available for parent-run validation; children do not mutate it.
