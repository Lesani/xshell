# Project Shell and Pi harness (binding design, 2026-10-18)

Requested by Mario; issues #59 (Shell) and #60 (Pi). Develop on Framework;
validate native Windows on Forge and publish the matching Framework Desktop.

## User model and UX

- New Chat is a project-scoped new-session entry point. With Default agent = Ask,
  always show the choice dialog, even when zero or one harness is installed.
  List installed harnesses for the selected Host and an always-available **Shell**.
- An explicitly configured installed default agent may still start immediately.
  Keep Shell accessible in existing project/tab shell menus. Do not persist Shell
  as an AgentId or invent an AI session for it. No new default setting is needed.
- Shell opens a raw Terminal owned by the Project's Host in exactly its cwd. For
  a Remote Host, reuse the SSH transport, never spawn local ssh or a shell in the
  Desktop's directory. Choose defaults from the Host OS, not Framework's OS; use
  Windows PowerShell on Windows and the existing shell presets elsewhere.
- Pi is a real AgentId, label Pi, CLI binary `pi`, a distinct simple pi mark.
  Detection, chooser/defaults, Settings, project/session rows, context and file
  tree recognize it through the existing registry and Host invocation patterns.

## Pi launch and persistence

- Pi 1.3.0's documented `--session-id <id>` creates an exact project session ID if
  absent, or opens that exact ID if present. Allocate UUIDs for new Pi chats as
  for Claude, and consistently use this option on restore/resume. Do not use
  `--resume` (it opens a selector), `--continue` (ambiguous latest), or rely on
  delayed title association to guess among concurrent new Pi chats. Pi permits
  project-scoped custom IDs: discovery, Desktop open-state/title matching and
  Daemon deduplication distinguish (Host, cwd, Pi, id), not just raw session ID.
  Keep existing agents' semantics and protocol wire fields unchanged.
- Existing project cwd, launch prefixes, npm .cmd-safe native Windows direct
  execution, restored Terminal deduplication and remote reconciliation apply.
- Pi bypassFlag is false. Do not equate Pi project trust `--approve` with a
  permission-bypass switch. No global Pi files/configuration or extensions are
  modified. Native Pi inherits the user's Headroom/provider setup unchanged.

## Pi read-only integration

- New core `pi` module owns parsing, discovery and conservative conventional
  context-resource listing. Default storage is `~/.pi/agent/sessions/` grouped
  into directories; cwd comes from the session header, never decoding filenames.
  Initial support covers default storage; custom Pi session-dir configuration
  is not claimed. Files are read-only; reject symlink traversal like other
  confined session reads, tolerate malformed/truncated/unknown records.
- Parse version-3 JSONL: session header id/cwd, session_info names, user text or
  text blocks, assistant model/text/tool calls, recorded usage and timestamp.
  Follow id/parentId from the last linked entry to get the active branch for
  current message/title/model/context metadata; abandoned text must not become
  current conversation. Linked session_info entries are indexed and advance the
  leaf just like other entries; their names are global metadata (Pi /name), and
  a blank name clears a previous name. Legacy unlinked metadata only sets name.
  Verified against Pi v1.3.0 session-manager.ts: _buildIndex, _appendEntry and
  appendSessionInfo, not an assumption that metadata cannot move the leaf.
  Source: https://github.com/earendil-works/pi/blob/v1.3.0/packages/coding-agent/src/core/session-manager.ts
- Lifetime usage counts recorded assistant/tool-result/usage/compaction/branch
  summary usage exactly once per entry across historical branches; `reasoning`
  is already included in output. Read recorded costs only; no price estimates
  or invented context-window limits. Unknown limits stay unknown/zero.
- Expose `list_pi_projects` and `get_pi_context` through core dispatch, local
  Tauri commands and Desktop Host method registries. Merge Pi sessions into
  get_sessions/get_all_recent_sessions and existing stats aggregation. Pi-only
  Projects must be discoverable and resumable. Preserve older Host degradation.
- Context listing: conventional global/project instructions, skills, prompts,
  extensions as named file resources using existing ContextSection shape.
  No execution, credential/config-value exposure, or claim that this is Pi's
  fully resolved dynamic package/extension loadout. Do not present Claude
  context as Pi context merely because Claude is absent.
- **Mobile remains unchanged**: Pi is not added to Mobile launch allowlists,
  stream/reply/first-message support or hook/status/push claims. Shell remains
  denied to Mobile. Existing Claude/Codex status behavior is untouched.

## Architecture, ownership and sequence

1. Parent records/commits this design; fine plan and independent review precede
   writers. Two issue feature branches, squash into dev one per issue.
2. Shell UI owner: project-shell worktree, App New Chat resolution, picker,
   relevant default-setting explanatory copy and focused UI/planning tests.
   Existing planNewShell is the process seam; avoid backend edits if unneeded.
3. Pi backend owner: isolated backend lane; src-tauri/** only (core parser,
   launch/detect/dispatch, local Tauri plumbing, daemon Desktop method inventory
   and tests). Do not widen Mobile rights. Commit lane handoff, no push/deploy.
4. Pi UI owner: separate isolated lane; src/** only (registry/types/icon,
   preallocated UUID planning, Settings, project discovery/history/context,
   tests). It must not edit App.tsx or AgentPickerDialog.tsx, owned by Shell.
   Backend contract method names/shapes above are binding.
5. Parent integrates handoff commits into Pi branch, merges Shell first, fixes
   integration-only gaps, independently reviews the aggregate and runs suites.
   No concurrent writers in a cwd; shared cargo cache only if serialized.

## Validation and delivery

- Shell: Ask with zero/one/multiple harnesses offers Shell; explicit defaults
  retained; project Host cwd, remote Windows vs Linux shell, offline refusal,
  no agent/session fields, cancel, and default-less choice behavior tests.
- Pi: registry/probe allowlist, exact new/resume argv incl Windows npm shims and
  prefixes; branch-aware JSONL fixtures (names, blocks, tools, errors/unknowns,
  costs/tokens/no double reasoning, truncated tail, Pi-only Project/history);
  dispatch method parity; context isolation; frontend UUID, types and picker.
- Parent full npm test + tsc, Rust workspace/fmt/clippy, affected native Windows
  tests/clippy, then real disposable Windows Project Terminal + Pi through
  Headroom, reconnect same session/PID, and cleanup only owned test Terminals.
- Existing baseline Linux WINCH failure is #58, not a green gate or reason to
  weaken tests. Track any failures separately and do not claim workspace green.
- Stage/publish only reviewed merged dev builds. Never restart running
  Framework/Forge xshell or kill user agents. A Forge Daemon update needs a
  coordinated restart if live user Terminals exist. User installs Framework
  via Update xshell; report version and any pending native restart clearly.
