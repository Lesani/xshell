# xshell

A desktop IDE that hosts AI coding-agent CLIs (Claude Code, Codex, Cursor, opencode, Antigravity) in terminal tabs and surfaces their sessions, usage and project state. This file fixes the vocabulary for the remote-hosts feature, where agents run on another machine. Location is meant to be nearly invisible: a session on a **Remote Host** looks and behaves like a local one.

## Language

### Machines and processes

**Desktop**:
The xshell GUI application a person interacts with.
_Avoid_: client, app, frontend (when meaning the whole application)

**Host**:
A machine whose agents, files and session history xshell can drive; the **Desktop**'s own machine is the **Local Host**.
_Avoid_: server, remote, box, machine

**Local Host**:
The **Host** the **Desktop** runs on, served in-process with no network hop.

**Remote Host**:
Any **Host** other than the **Local Host**, reached through its **Daemon**.

**Host Status**:
A **Desktop**'s view of whether it can reach a **Remote Host**: connected, reconnecting, offline, or upgrade pending.

**Daemon**:
The headless `xshelld` process that serves one **Remote Host** to any number of **Desktops**.
_Avoid_: agent (reserved for AI CLIs), server, xshell-server

### Terminals and tabs

**Terminal**:
A running process (an agent CLI or a shell) in a pseudo-terminal, owned by exactly one **Host**.
_Avoid_: session (reserved for an agent's conversation history), PTY, shell

**Tab**:
A **Desktop**'s view of one **Terminal**.

**Attach**:
To bind a **Tab** to a live **Terminal** and replay the output it has not yet shown.
_Avoid_: resume (reserved for an agent CLI reopening a session in a new process)

### Projects

**Project**:
A working directory on a specific **Host**, identified by the pair (Host, path).
_Avoid_: repo, workspace, folder (a **Sidebar Folder** is a grouping of Projects)

**Sidebar Layout**:
A **Desktop**'s personal arrangement of pinned **Projects** into folders, with their icons and display names.

## Relationships

- A **Desktop** connects to zero or more **Remote Hosts**; it always has exactly one **Local Host**
- A **Daemon** serves exactly one **Host** and accepts any number of **Desktops**; a **Host** runs at most one **Daemon** per user, shared by **Desktops** of any version
- The **Desktop** always initiates the connection; a **Daemon** never dials out
- A **Remote Host**'s **Terminals** are the source of truth: every connected **Desktop** shows exactly one **Tab** per **Terminal**, opening and closing **Tabs** as **Terminals** appear and end
- Closing a **Tab** ends its **Terminal** for every **Desktop**; losing the connection or quitting the **Desktop** ends nothing
- The same path on two **Hosts** is two different **Projects**
- **Sidebar Layout** belongs to the **Desktop**, not the **Host**: pinning or arranging a remote **Project** is never mirrored to other **Desktops**
- While a **Remote Host** is offline, the **Desktop** keeps showing its last known **Terminals** and **Project** data as stale and refuses to start new **Terminals** there; on reconnect it reconciles with the **Daemon**
- Several **Desktops** may show the same **Terminal** at once; all may type into it, and its size follows whichever **Desktop** last interacted with it
- A **Daemon** that restarts relaunches its **Terminals**, resuming each agent's session, just as the **Desktop** does for **Local Host** tabs

## Flagged ambiguities

- "agent" already means an AI CLI (claude, codex, …) throughout the codebase, so the background process on a **Remote Host** is the **Daemon**, never "agent".
