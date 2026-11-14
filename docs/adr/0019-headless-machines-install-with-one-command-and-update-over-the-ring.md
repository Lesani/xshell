# Headless machines install with one command and update over the Ring

Without SSH (ADR-0016), a Desktop can no longer install or update a Daemon by logging in. A headless machine gets xshell from one pasted command that downloads the Daemon, verifies its checksum, starts it, enables it across reboots and prints the Device Code. Updates travel over the Ring: any Desktop newer than a machine offers to update it, one machine or all at once.

The install script is served from its own name (`get.xshell.sh`), never from the Relay's: the Relay is untrusted by design and an installer is fully trusted, so they must not share a name or a codebase. Builds are produced and verified locally and only hosted on GitHub Releases. Until the first next-generation release the script is fetched from GitHub as well.
