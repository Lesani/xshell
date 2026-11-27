# The Ring is the only way machines meet

xshell exists so that one person sees every coding session on every machine they own, on every screen they own. Two ways to reach a machine had grown side by side: SSH Hosts, configured per Desktop and invisible to the Mobile, and the Ring, joined once and visible everywhere. We make the Ring the only way: "add a machine" always means Pairing it into the Ring, and nothing in the main experience may depend on SSH. This ends the role ADR-0002 gave SSH as the way to reach Daemons; existing SSH Hosts keep working as a per-Desktop extra and get no further work.

Because the Ring is now the product, joining it has to be trivially easy and impossible to get half-wrong:

- **Two steps, no confirmation on the new machine.** The new machine shows a Device Code (its pairing screen, or the headless command); the user types it on any device already in the Ring, the Mobile included. The joining-side consent card and its cancel and recovery states are removed. A machine is adoptable only while it shows its code, which is what stops strangers from capturing an unpaired xshell by guessing words.
- **Remove is one action.** Any device in the Ring may remove any machine, itself included, with immediate effect. A removed machine says so on its next start and offers to join again. There is no separate Leave step and no Pending Leave; removing needs a reachable Relay and queues nothing. This replaces ADR-0015's Pending Leave.
- **No sessions other devices cannot see.** The in-process Local Host is removed. Every computer's Terminals live in its Daemon; a Daemon that cannot start is a plain error with a fix, never a silent fallback, and the Daemon ships so it cannot go missing.
- **First launch asks.** A machine outside a Ring opens on one screen: this is my first machine (creates the Ring), or add this machine to my others (shows the Device Code). Skip leaves it working alone.

## Considered options

- Keeping the consent card: it defends against someone who reads the code off the screen and adopts the machine into their own Ring, but it only shows names the attacker chooses, and it carried most of the adoption state machine. The headless path already accepted the same risk.
- Restricting removal to computers: a stolen laptop with only a phone at hand needs removal from the Mobile. An unlocked stolen phone can then remove machines, which costs re-pairing and grants no access.
