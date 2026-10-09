# The Mobile, the Relay and the Push Gateway live in a separate repository

Someone who builds xshell as it is today should not need the Mobile's code, its toolchains or its store plumbing. So the split follows where code runs: everything that runs on a dev machine (Desktop, Daemon, the Desktop's Ring and Pairing UI, `xshelld pair`, the Daemon's push decision) stays in this repository, and everything that runs on a phone or a server (the Expo Mobile, the Relay Worker and its workerd image, the Push Gateway) lives in `xshell-remote`, private for now.

The code both sides must agree on stays here, in one dependency-light crate, `xshell-protocol`: the frame codec and message types (moved out of `xshell-core`), device keys, the Roster, Pairing, the Noise sessions and the Relay envelope and client. It builds without PTY, SQLite or Tauri, so the Mobile links it through UniFFI. `xshell-remote` depends on it by git revision. The domain glossary and the design ADRs stay here too, as the single source of truth for both repositories.

This repository's tests reach the Relay through a small in-process test Relay that only routes envelopes. The real Relay in `xshell-remote` runs a contract test suite against `xshell-protocol`'s Relay client, so the two Relays cannot drift apart unnoticed.

## Considered Options

- **One repository with the Mobile in a subdirectory.** Rejected: xshell builders would clone Expo, the Relay and the gateway, and the paid parts would sit in the upstream candidate.
- **Depending on `xshell-core` from the Mobile.** Rejected: it pulls in portable-pty and bundled SQLite, which the Mobile neither needs nor should build.
