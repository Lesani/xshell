# Linux and macOS run one always-on Daemon

ADR-0005 made the Local Host's Daemon GUI-bound by default and offered a Persistent Daemon by opt-in, with code to hand running Terminals from one to the other. Three states on one platform (headless, GUI-bound, opted-in persistent) meant three ways to pair and to fail. On Linux and macOS there is now one: the Daemon always runs on its own, exactly as on a headless machine, and the Desktop window is a viewer onto it. A Linux desktop is "a headless machine plus a window".

ADR-0005's reason, that agents must not keep running and spending unnoticed after quit, is kept by asking instead of deciding: quitting while Terminals run on this machine asks "Keep N sessions running?" (Keep running / Stop them), with "Do not ask again" and a setting to always keep them in the background. The next launch shows the same Terminals again.

Windows stays GUI-bound for now and confirms on close that the Terminals running on this computer will end. A background Daemon on Windows is wanted and deferred, not ruled out.
