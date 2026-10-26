# Windows SSH connections attach to the GUI-bound Daemon

Windows Hosts already have a GUI-bound Daemon on a per-user named pipe, but
`xshelld connect` refuses Windows. An SSH Desktop therefore cannot reach those
Terminals, even though the Windows Desktop and Mobiles can.

Allow `xshelld connect` on Windows as a binary stdio bridge to the existing
named pipe. Keep the pipe's current-user identity and ownership checks. No new
network listener, authentication scheme, wire protocol, or persistent service
is introduced. The Windows Desktop must be open; a missing Daemon produces the
existing not-running exit code and message. The bridge never starts a Daemon,
and an SSH disconnect must not end the Daemon or its Terminals.

Windows pipes do not provide a half-close. End either bridge direction on EOF
or fatal error and cancel the opposite blocked operation; do not leak a worker,
hang on stdin, or wait indefinitely while flushing a disconnected peer. Keep
stdout exclusively binary protocol bytes and diagnostics on stderr. Test real
OpenSSH binary transport as well as the local pipe bridge.

Windows Hosts are initially configured using an explicit Daemon command pointing
to the installed executable. Automatic Windows probing/install/upgrade and a
Windows Persistent Daemon were excluded from the initial bridge slice. Unix
connection semantics stay unchanged. Closing the Windows Desktop continues to
end its agents.

## Amendment: explicit Daemon updates (#77)

Configured Windows Hosts can stage a verified Daemon build and explicitly request
its selection and restart through the owning Desktop. This is not automatic Host
setup: `connect` remains attach-only and never starts a Daemon or Desktop.

The owning Desktop must first be updated and reopened with support for selected
GUI-bound restart. Its explicit readiness flag gates the Daemon capability. An old
owner shows that prerequisite rather than an operative remote Update action.
Staging does not select or stop anything; the running Daemon validates the exact
target, persists its Terminals, atomically selects the build and exits. The same
owning Desktop restarts it with its existing parent and Job lifetime. No Windows
Persistent Daemon or new owner IPC service is introduced.

See [Host Daemon updates](../host-daemon-updates.md) for the update and artifact
requirements.
