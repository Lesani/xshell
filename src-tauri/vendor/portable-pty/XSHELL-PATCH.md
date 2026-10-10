# portable-pty 0.9.0, patched for xshell

This is portable-pty 0.9.0 from crates.io, unchanged except for one addition.
`src-tauri/Cargo.toml` uses it through `[patch.crates-io]`.

## The patch

`src/win/conpty.rs` adds `ConPtyMasterPty::try_clone_input()`. It returns a duplicate of
the write end of the ConPTY's input pipe, made under the lock that guards it. It works only
before `take_writer()`.

## Why

The Daemon writes a Chat View reply (`term.submit`) while it holds the agent's screen,
prompt and status locks. Such a write must never block (Lesani/xshell#40). On Unix the
Daemon sets `O_NONBLOCK` on a duplicate of the PTY master for one write. On Windows the
same thing needs the pipe's handle, for `SetNamedPipeHandleState(PIPE_NOWAIT)`.
portable-pty 0.9.0 hides it: `take_writer()` gives an opaque `Box<dyn Write>`, and the
fields of `ConPtyMasterPty` are private.

## How to drop it

When an upstream portable-pty release has an equivalent method:
1. Remove the `[patch.crates-io]` entry and the `exclude` of `vendor` in `src-tauri/Cargo.toml`.
2. Delete `src-tauri/vendor/portable-pty`.
3. Change the call in `crates/xshelld/src/server/terminal.rs` (`win::input_pipe`).
4. Run `cargo update -p portable-pty`.

Upstream: wezterm/wezterm (`pty/`). No PR is open yet.
