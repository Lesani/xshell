# xshell

A desktop IDE that hosts AI coding-agent CLIs (Claude Code, Codex, Cursor, opencode, Antigravity, Pi) in terminal tabs and surfaces their sessions, usage and project state. This file fixes the vocabulary for the remote-hosts feature, where agents run on another machine. Location is meant to be nearly invisible: a session on a **Remote Host** looks and behaves like a local one.

Membership and Pairing relationships include the agreed [Desktop Ring adoption design](docs/desktop-ring-adoption.md); implementation gaps are listed under Flagged ambiguities.

## Language

### Machines and processes

**Desktop**:
The xshell GUI application a person interacts with, on a computer or a phone.
_Avoid_: client, app, frontend (when meaning the whole application)

**Host**:
A machine whose agents, files and session history xshell can drive; the **Desktop**'s own machine is the **Local Host**.
_Avoid_: server, remote, box, machine

**Local Host**:
The **Host** the **Desktop** runs on, served by that **Desktop**'s **GUI-bound Daemon** (or a **Persistent Daemon** when opted in, Linux and macOS only) over a local socket (a per-user named pipe on Windows), with no network hop.

**Remote Host**:
Any **Host** other than the **Local Host**, reached through its **Daemon**.

**Host Status**:
A **Desktop**'s view of whether it can reach a **Host**: connected, reconnecting, offline, upgrade pending, build differs (the **Daemon** runs this version from another build, or from an unknown one; usable, and the user may replace it with this **Desktop**'s build), xshell closed (its **Daemon** said goodbye to the **Relay** on a clean exit), unreachable (the **Relay** connection dropped without a goodbye), or needs update.

**Daemon**:
The headless `xshelld` process that serves one **Host**'s **Terminals** to any number of **Desktops**.
_Avoid_: agent (reserved for AI CLIs), server, xshell-server

**GUI-bound Daemon**:
A **Daemon** started and ended by a **Desktop** on its own machine, so that machine's **Terminals** are reachable from other **Desktops** only while xshell is open.

**Persistent Daemon**:
A **Daemon** that outlives any **Desktop**, installed by opt-in on the **Local Host** or automatically on a **Remote Host**.

**Mobile**:
A **Desktop** on a phone that observes and steers agent **Terminals** on its **Ring**'s **Hosts**, with fewer rights than a computer **Desktop** (see Relationships); it has no **Local Host**, so no agent ever runs on the phone.
_Avoid_: Mobile Desktop, remote control (Claude Code's and Codex's feature it replaces), companion, phone client, mobile client, remote app

### Terminals and tabs

**Terminal**:
A running process (an agent CLI or a shell) in a pseudo-terminal, owned by exactly one **Host**.
_Avoid_: session (reserved for an agent's conversation history), PTY, shell

**Agent Status**:
What an agent **Terminal** is doing as its agent's hooks report it: working, needs you (a permission prompt or question), finished (the turn ended), or ended.
_Avoid_: activity, state

**Permission Prompt**:
An agent's question that blocks its turn until answered, with the options its TUI offers; a **Mobile** shows it as buttons, and whoever answers first, at any **Desktop**, wins. Each shown prompt has an id; an answer to a prompt that is no longer current, because someone answered or typed at it first, is "already answered". A prompt whose options cannot be read safely is shown as text only.
_Avoid_: approval dialog, confirmation

**Shell** (new-session choice):
Starts a raw **Terminal** in the selected **Project** on its **Host**, without an
agent harness. It is not an agent or an agent session, and is unavailable to a
**Mobile**.

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
Adding a device's key to an existing **Ring** with approval from an enrolled device.

**Computer Device**:
A computer **Desktop** and its **Local Host**'s **Daemon** treated as one unit of **Ring** membership.

**Device Code**:
A one-time sequence of eight independently random words from one fixed, versioned 4,096-word English list shared by all platforms and UI languages, offered by an unpaired **Computer Device** or standalone headless **Daemon** to request its **Pairing**, rather than an alphanumeric code.

**Pending Leave**:
A durable obligation to register a device's departure with its old **Relay** after it has left locally.

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

**Farewell** (legacy relay-move behavior):
The newest **Roster** owed to an old **Relay** by the existing relay-move implementation; automatic **Ring** moves are being replaced by explicit per-device leave and re-pairing, and this is distinct from a **Pending Leave**.

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

- A **Desktop** connects to zero or more **Remote Hosts**; a computer **Desktop** always has exactly one **Local Host**, a **Mobile** has none
- A **Daemon** serves exactly one **Host** and accepts any number of **Desktops**; a **Host** runs at most one **Daemon** per user, shared by **Desktops** of any version
- A **Desktop** reaches a **Daemon** either by initiating SSH or through its **Ring**'s **Relay**; a **Daemon** dials out only to that **Relay**, never to a **Desktop**
- A **Host** appears on a **Mobile** while its **Daemon** is connected to the **Relay**; a **Mobile** never installs or starts a **Daemon**
- A **Mobile** interacts fully with agent **Terminals** (open, type, approve, Relaunch with skip-permissions) but never acts on a **Host** itself: no shell **Terminals** (which it is never even told about), file browsing or git operations from the **Mobile**; git and files are the agent's job. Each **Daemon** enforces this from the **Mobile**'s role in the **Roster**; changing skip-permissions from a **Mobile** needs a fresh biometric confirmation
- A **Mobile** is woken when an **Agent Status** becomes needs you or finished, each switchable in settings; the **Daemon** decides and seals the payload, so the **Relay** learns only that some device should be woken
- A **Daemon** streams raw **Terminal** output to a **Mobile** only while that **Terminal**'s **Terminal View** is on screen; the **Chat View** receives session updates, never screen redraws. That stream is at most 1 frame per second, rising to 10 per second for 3 seconds after the **Mobile** sends input
- A **Relay** asks the **Push Gateway** to wake a **Mobile** only when no **Mobile** of its **Ring** is in the foreground; the **Push Gateway** never sees **Terminal** content
- A **Daemon** seals each push to a seal key the **Mobile** registers over its session for that purpose alone, never its session key; a **Mobile** counts as in the foreground only while it tells the **Relay** so and its connection stays live, and the **Daemon** and the **Relay** both check it
- "Desktop" as a **Roster** role means a computer **Desktop**; a **Mobile** has its own role, never the desktop role
- Computer **Desktops** may change the **Roster**; a **Mobile** may approve **Pairing**, including full computer **Desktop** membership, or explicitly remove active devices, only after fresh biometric confirmation
- Any authorized full client, including a **Daemon** or computer **Desktop**, may carry out removal of expired devices
- A **Daemon** may remove itself or devices expired under the signed **Ring** policy, but cannot enroll devices or remove other active members; removal of other active members requires an explicit action from an authorized computer **Desktop** or biometric-confirmed **Mobile**
- Pairing a computer **Desktop** grants full membership, including authority to approve further devices without the original **Desktop** being online
- An adopted computer **Desktop** discovers all existing **Ring** **Hosts** and can access them through the **Relay** while their **Daemons** are online, without configuring SSH again; SSH credentials and desktop-specific **Sidebar Layout** are not copied
- Pairing a computer **Desktop** also enrolls its **Local Host**'s **Daemon** in the same **Ring**, without a second pairing step; the **Local Host** is reachable only while its **Daemon** runs
- A **Computer Device** appears as one device for membership management; removing it revokes both its computer **Desktop** and **Local Host** **Daemon** identities together
- Revocation immediately disconnects the removed identities' existing **Relay** sessions and rejects reconnection; peers close their sessions on receiving the signed revocation, without stopping the removed computer's local **Terminals**
- Activity from either identity keeps the whole **Computer Device** enrolled; it expires only when neither identity has been connected within the inactivity timeout. **Mobiles** and standalone headless **Hosts** track their own activity
- **Device Code** input ignores capitalization and accepts spaces or hyphens between words
- A device-join QR contains only the same **Device Code** words plus a marker identifying it as a join-ring device code; it carries no separate credential or **Relay** URL, and scanning uses the same adoption flow as typing the words
- Entering a **Device Code** requests **Pairing** but does not grant access; the joining computer **Desktop** shows the proposed **Ring** and approving device and requires explicit local confirmation before adoption
- A **Device Code** is valid only while its joining settings screen remains open, for at most 10 minutes; closing it or switching away to another xshell tab/window invalidates it, and no live code is retained outside that screen
- OS-level minimization and lock detection are outside scope; code lifetime does not depend on those events
- Declining a **Pairing** request invalidates its **Device Code**; the joining screen offers a fresh code, and rejection does not count as a failed adoption attempt
- An expired **Device Code** stays expired until the user chooses Generate new code; there is no automatic renewal. Reopening the join screen can generate a fresh code
- The **Relay** counts failed adoption attempts per **Ring** across its approving devices in a rolling one-minute window; the third failure starts a three-minute **Ring**-wide cooldown during which no new adoption attempts are accepted; successful attempts do not count toward the threshold
- An unknown or expired **Device Code**, or failed cryptographic verification, counts as a failed adoption attempt; user rejection, cancellation, and network outages do not
- A computer **Desktop** already in a **Ring** must explicitly leave before joining another; **Pairing** never silently replaces membership or merges **Rings**
- Leaving a **Ring** ends local participation by the computer **Desktop** and its **Local Host**'s **Daemon**, preserves local **Terminals** and settings, and records a **Pending Leave** until their removal is registered with the old **Relay**
- A **Pending Leave** is visible in the **Desktop** window, survives restarts, and never blocks joining another **Ring**
- Inactivity expiry is a **Ring**-wide setting in Hosts, defaults to 30 days, and is included in the signed **Ring** policy; computer **Desktops** and **Mobiles** after fresh biometric confirmation may change it, while **Daemons** only enforce it; there is no **Relay** environment override
- Inactivity policy supports an explicit Never expire option as well as finite durations; Never expire is a signed **Ring**-wide choice, not an implicit exemption
- Expiry and revocation are final for an enrollment: increasing or disabling inactivity expiry never restores it; the device must pair again as a fresh enrollment
- Shortening the inactivity timeout applies immediately to existing devices after showing which devices will expire and requiring confirmation; there is no extra grace period
- Inactivity expiry has no last-device exemption; if every device expires, the user must create a new **Ring** and pair devices again, while local **Terminals** and data remain intact
- Authorized clients may rely on the **Relay**'s activity records for expiry; a compromised **Relay** may cause false expiry and loss of availability, but must remain unable to grant unauthorized access
- A **Remote Host** joins its **Desktop**'s **Ring** automatically when that **Desktop** installs its **Daemon** over SSH; a headless **Daemon** can also pair with `xshelld pair`
- Standalone headless `xshelld pair` uses the same eight-word **Device Code** format and enrolls only the **Daemon**; its code is valid only while the command runs, for at most 10 minutes, and is invalidated on exit. The settings-screen lifetime rule applies to GUI **Pairing**
- An unpaired computer **Desktop** offers a **Device Code** to join an existing **Ring** on Windows, Linux, and macOS; QR-based approval by a **Mobile** is planned after the **Device Code** flow
- **Pairing** controls and an optional custom **Relay** URL belong in the **Desktop**'s Hosts settings; a configured URL is used for **Pairing**, otherwise the **Hosted Relay** is used
- An unpaired computer **Desktop** offers explicit Create ring and Join ring actions in Hosts; no **Ring** is created automatically, and normal local use requires neither action
- Join ring opens the **Device Code** screen; Create ring enrolls this **Computer Device** as the first full member
- **Pairing** does not discover other **Relays** or provide a special self-hosted setup flow; a code offered on another **Relay** produces the normal device-not-found result
- A **Mobile** gates features per **Host** on the **Daemon**'s capabilities; when protocol ranges do not overlap it tells the user which side to update (the app, or the **Daemon** at a **Desktop**), and never upgrades a **Daemon** itself
- A **Ring** has exactly one **Relay**, named in its **Roster**; it defaults to the **Hosted Relay**
- There is no move-ring function: switching to another **Relay** requires explicitly deregistering and registering each device in a new **Ring**, rather than moving the existing **Ring** or automatically migrating its members
- An enrolled device cannot change its **Ring**'s **Relay** through the custom URL field; it must leave first, then select the **Relay** for creating or joining another **Ring**
- The **Relay** maintains and enforces the latest signed **Roster**, but is not a membership-granting authority: every device independently checks authorized signatures and refuses older versions
- A compromised **Relay** must remain unable to grant an attacker access by inserting a key; membership changes require authorization independently verifiable by devices
- The **Relay** routes only between keys of the same **Ring**; compromising it can drop traffic but not read or type into **Terminals**
- A **Relay** must enforce authorized **Ring** membership before permitting attachment or forwarding traffic; unauthorized requests must not trigger downstream device communication
- A **Remote Host**'s **Terminals** are the source of truth: every connected **Desktop** shows exactly one **Tab** per **Terminal**, opening and closing **Tabs** as **Terminals** appear and end
- Closing a **Tab** ends its **Terminal** for every **Desktop**; losing the connection or quitting the **Desktop** ends nothing
- The same path on two **Hosts** is two different **Projects**
- A **Mobile** starts chats only in **Projects** the **Host** already knows from agent session history; new **Projects** start at a **Desktop**
- **Sidebar Layout** belongs to the **Desktop**, not the **Host**: pinning or arranging a remote **Project** is never mirrored to other **Desktops**
- While a **Remote Host** is offline, the **Desktop** keeps showing its last known **Terminals** and **Project** data as stale and refuses to start new **Terminals** there; on reconnect it reconciles with the **Daemon**
- A **Host** runs an agent session in at most one live **Terminal**: resuming a session that a **Terminal** runs, or is relaunching, is that **Terminal**
- Several **Desktops** may show the same **Terminal** at once; all may type into it, and its size follows whichever **Desktop** last interacted with it
- A **Mobile** only claims a **Terminal**'s size when typing into its **Terminal View**; viewing, and replying from the **Chat View**, never resize it. When a **Mobile** that holds the size leaves, the size returns to the **Desktop** that last held it
- A **Daemon** that restarts relaunches its **Terminals**, resuming each agent's session; the **Local Host**'s come back this way when the **Desktop** starts its **GUI-bound Daemon** again, and Tabs saved by an older **Desktop** move into that **Daemon** once

## Example dialogue

> **Dev:** "Does pairing this computer add only its Daemon?"
> **Domain expert:** "No. A Computer Device joins as a unit: its Desktop can approve more devices, and its Local Host becomes reachable through the Ring."
> **Dev:** "Can we restore an expired enrollment by disabling expiry?"
> **Domain expert:** "No. Out is out; pair it again."

## Flagged ambiguities

- Computer **Desktop** adoption through a **Device Code** is the agreed target, not yet implemented; existing `xshelld pair` enrolls a **Daemon**, not a computer **Desktop**.
- Explicit per-device leave and re-pairing on a new **Relay** supersedes the automatic relay-move behavior described in ADR-0012; the existing move implementation has not yet been removed.
- **Mobile** approval of **Pairing**, active-device removal, and inactivity-policy changes, with fresh biometric confirmation, and **Daemon** execution of expired-device removal are agreed targets; the current protocol still restricts **Roster** changes to computer **Desktops**.
- Current installations are development devices and will all be updated; backward compatibility with old membership rules is not a delivery gate. Public release is a later gate, not part of this design's current rollout.
- Inactivity expiry uses the signed **Ring**-wide policy configured in Hosts and trusts **Relay** activity records. The current protocol needs scoped authorization for **Daemon** self-removal and expiry cleanup without granting enrollment or arbitrary active-member removal.
- Broader attack prevention and denial-of-wallet protections are deferred to separate work, outside the current **Pairing** design; normal **Relay** membership enforcement and the agreed three-failed-adoption-attempts cooldown remain required.

- "user" in "all of a user's devices" means a **Ring**, not an account: there are no accounts, only paired keys.
- "agent" already means an AI CLI (claude, codex, …) throughout the codebase, so the background process on a **Remote Host** is the **Daemon**, never "agent".
