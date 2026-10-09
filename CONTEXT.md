# xshell

A desktop IDE that hosts AI coding-agent CLIs (Claude Code, Codex, Cursor, opencode, Antigravity) in terminal tabs and surfaces their sessions, usage and project state. This file fixes the vocabulary for the remote-hosts feature, where agents run on another machine. Location is meant to be nearly invisible: a session on a **Remote Host** looks and behaves like a local one.

## Language

### Machines and processes

**Desktop**:
The xshell GUI application a person interacts with, on a computer or a phone.
_Avoid_: client, app, frontend (when meaning the whole application)

**Mobile Desktop**:
A **Desktop** on a phone (Android or iOS). It drives agents on **Remote Hosts** with the same capabilities as any other **Desktop**, but has no **Local Host**: no agent ever runs on the phone.
_Avoid_: companion, mobile client, remote app

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
The headless `xshelld` process that serves one **Host**'s **Terminals** to any number of **Desktops**.
_Avoid_: agent (reserved for AI CLIs), server, xshell-server

**GUI-bound Daemon**:
A **Daemon** started and ended by a **Desktop** on its own machine, so that machine's **Terminals** are reachable from other **Desktops** only while xshell is open.

**Persistent Daemon**:
A **Daemon** that outlives any **Desktop**, installed by opt-in on the **Local Host** or automatically on a **Remote Host**.

**Mobile**:
A **Desktop** on a phone that observes and steers **Terminals** on its **Ring**'s **Hosts** and runs no agents itself.
_Avoid_: remote control (Claude Code's and Codex's feature it replaces), companion, phone client

### Terminals and tabs

**Terminal**:
A running process (an agent CLI or a shell) in a pseudo-terminal, owned by exactly one **Host**.
_Avoid_: session (reserved for an agent's conversation history), PTY, shell

**Agent Status**:
What an agent **Terminal** is doing as its agent's hooks report it: working, needs you (a permission prompt or question), finished (the turn ended), or ended.
_Avoid_: activity, state

**Permission Prompt**:
An agent's question that blocks its turn until answered, with the options its TUI offers; a **Mobile** shows it as buttons, and whoever answers first, at any **Desktop**, wins.
_Avoid_: approval dialog, confirmation

**Tab**:
A **Desktop**'s view of one **Terminal**.

**Attach**:
To bind a **Tab** to a live **Terminal** and replay the output it has not yet shown.
_Avoid_: resume (reserved for an agent CLI reopening a session in a new process)

**Relaunch**:
To end a **Terminal**'s process and start an updated launch spec in its place (same **Terminal**, same **Tabs**), resuming the agent's session; used to turn an agent's skip-permissions flag on or off.
_Avoid_: restart. A **Daemon** starting its **Terminals** again after its own restart restores them; that is not a Relaunch

### Relay and pairing

**Relay**:
A self-hosted, untrusted router that joins end-to-end encrypted streams between **Daemons** and **Desktops** of one **Ring**, and never sees plaintext.
_Avoid_: server, proxy, devrelay (the private predecessor it is modelled on)

**Ring**:
The set of devices (**Daemons** and **Desktops**) whose keys were paired with each other; it is what "one user" means to xshell.
_Avoid_: account, team, user (when meaning the set of devices)

**Roster**:
The signed, versioned list of a **Ring**'s device keys and roles; the latest version is the **Ring**'s membership.
_Avoid_: member list, device list

**Pairing**:
Adding a device's key to a **Ring**, done once per device by scanning a code shown on a device already in it.

**Chat View**:
A **Mobile**'s rendering of an agent **Terminal** as the conversation read from the agent's session, with replies sent as input to the **Terminal**.
_Avoid_: transcript view, remote control

**Inbox**:
A **Mobile**'s home screen: every agent **Terminal** across the **Ring**'s **Hosts**, ordered by **Agent Status** with needs you first.
_Avoid_: dashboard, feed

**Terminal View**:
A **Tab**'s raw rendering of a **Terminal**'s output, as every **Desktop** shows it; on a **Mobile**, the fallback behind the **Chat View**.

**Push Gateway**:
The one service the xshell project runs centrally: it wakes a **Mobile** through APNs or FCM for a paid subscription, and carries only sealed payloads it cannot read.
_Avoid_: notification server, push relay

**Hosted Relay**:
A **Relay** run by the xshell project for subscribers who do not self-host.

**Subscription**:
A store purchase (Push or Hosted tier) made on one **Mobile** that covers its whole **Ring**.
_Avoid_: account, license, plan

### Projects

**Project**:
A working directory on a specific **Host**, identified by the pair (Host, path).
_Avoid_: repo, workspace, folder (a **Sidebar Folder** is a grouping of Projects)

**Sidebar Layout**:
A **Desktop**'s personal arrangement of pinned **Projects** into folders, with their icons and display names.

## Relationships

- A **Desktop** connects to zero or more **Remote Hosts**; a computer **Desktop** always has exactly one **Local Host**, a **Mobile Desktop** has none
- A **Daemon** serves exactly one **Host** and accepts any number of **Desktops**; a **Host** runs at most one **Daemon** per user, shared by **Desktops** of any version
- A **Desktop** reaches a **Daemon** either by initiating SSH or through its **Ring**'s **Relay**; a **Daemon** dials out only to that **Relay**, never to a **Desktop**
- A **Host** appears on a **Mobile** while its **Daemon** is connected to the **Relay**; a **Mobile** never installs or starts a **Daemon**
- A **Mobile** interacts fully with agent **Terminals** (open, type, approve, Relaunch with skip-permissions) but never acts on a **Host** itself: no shell **Terminals**, file browsing or git operations from the **Mobile**; git and files are the agent's job. Each **Daemon** enforces this from the **Mobile**'s role in the **Roster**; changing skip-permissions from a **Mobile** needs a fresh biometric confirmation
- A **Mobile** is woken when an **Agent Status** becomes needs you or finished, each switchable in settings; the **Daemon** decides and seals the payload, so the **Relay** learns only that some device should be woken
- A **Daemon** streams raw **Terminal** output to a **Mobile** only while that **Terminal**'s **Terminal View** is on screen; the **Chat View** receives session updates, never screen redraws. That stream is at most 1 frame per second, rising to 10 per second for 3 seconds after the **Mobile** sends input
- A **Relay** asks the **Push Gateway** to wake a **Mobile** only when no **Mobile** of its **Ring** is in the foreground; the **Push Gateway** never sees **Terminal** content
- Only **Desktops** may change the **Roster**; **Mobiles** and headless **Daemons** are members that cannot add or remove anyone
- A **Remote Host** joins its **Desktop**'s **Ring** automatically when that **Desktop** installs its **Daemon** over SSH; other machines pair with `xshelld pair`
- A **Mobile** gates features per **Host** on the **Daemon**'s capabilities; when protocol ranges do not overlap it tells the user which side to update (the app, or the **Daemon** at a **Desktop**), and never upgrades a **Daemon** itself
- A **Ring** has exactly one **Relay**, named in its **Roster**; it defaults to the **Hosted Relay**, and moving to another is a new **Roster** version
- The **Relay** stores the latest **Roster** but cannot forge one: every device checks its signatures and refuses older versions
- The **Relay** routes only between keys of the same **Ring**; compromising it can drop traffic but not read or type into **Terminals**
- A **Remote Host**'s **Terminals** are the source of truth: every connected **Desktop** shows exactly one **Tab** per **Terminal**, opening and closing **Tabs** as **Terminals** appear and end
- Closing a **Tab** ends its **Terminal** for every **Desktop**; losing the connection or quitting the **Desktop** ends nothing
- The same path on two **Hosts** is two different **Projects**
- A **Mobile** starts chats only in **Projects** the **Host** already knows from agent session history; new **Projects** start at a **Desktop**
- **Sidebar Layout** belongs to the **Desktop**, not the **Host**: pinning or arranging a remote **Project** is never mirrored to other **Desktops**
- While a **Remote Host** is offline, the **Desktop** keeps showing its last known **Terminals** and **Project** data as stale and refuses to start new **Terminals** there; on reconnect it reconciles with the **Daemon**
- Several **Desktops** may show the same **Terminal** at once; all may type into it, and its size follows whichever **Desktop** last interacted with it
- A **Mobile** only claims a **Terminal**'s size when typing into its **Terminal View**; viewing, and replying from the **Chat View**, never resize it
- A **Daemon** that restarts relaunches its **Terminals**, resuming each agent's session, just as the **Desktop** does for **Local Host** tabs

## Flagged ambiguities

- "user" in "all of a user's devices" means a **Ring**, not an account: there are no accounts, only paired keys.
- "agent" already means an AI CLI (claude, codex, …) throughout the codebase, so the background process on a **Remote Host** is the **Daemon**, never "agent".
