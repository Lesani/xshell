# A Remote Host's Terminals are owned by its Daemon, not by any Desktop

_Revised by ADR-0005: the Local Host's Terminals now run in a GUI-bound Daemon with these semantics too, and the Desktop moves its saved `open_tabs` into that Daemon once._

Local tabs are Desktop-owned: the Desktop persists `open_tabs` and respawns each one with the agent's resume flag on launch. For Remote Hosts we invert this. The Daemon owns the set of Terminals and every connected Desktop mirrors it — a Terminal opened from one Desktop appears on all of them, closing a Tab ends the Terminal everywhere, and a Desktop that disconnects or quits ends nothing. The Daemon also persists each Terminal's launch recipe and, after its own restart, relaunches them with the agent's resume flag, so a Remote Host survives reboots exactly the way local tabs survive app restarts.

The goal is location transparency: a session running on another machine should look and behave like a local one. Desktop-owned remote tabs were rejected because two Desktops would then disagree about what is running, and a Desktop that was offline when its Tab was closed elsewhere would resurrect it.

The Local Host deliberately keeps the Desktop-owned model rather than running through a local Daemon: agents that silently keep running (and spending) after the user quits would surprise existing users, Windows has no Daemon yet, and it would change every user's local path at once. Because the Desktop and Daemon share one core, a later opt-in "keep local Terminals running after quit" needs no redesign.
