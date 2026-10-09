# Contributing to xshell

Thanks for your interest in contributing — issues, pull requests, and discussion are all welcome.

xshell is an independent open-source project that drives the official `claude` CLI as a subprocess. It is not affiliated with, endorsed by, or a product of Anthropic.

## Right to contribute

By submitting a contribution you confirm that:

- You are legally entitled to contribute the code you contribute.
- Each of your contributions is your original creation.
- To your knowledge, your contributions do not infringe, violate, or misappropriate any third-party intellectual property or other proprietary rights.

Contributions are accepted under the [MIT License](./LICENSE).

## Getting started

1. Clone the repository and install dependencies:
   ```bash
   git clone https://github.com/MertPROJ/xshell.git
   cd xshell
   npm install
   ```
2. Create a branch for your change:
   ```bash
   git checkout -b feature/your-feature-name
   ```

> If you're contributing from outside the project, fork the repo on GitHub first and clone your fork's URL instead.

### Prerequisites

- [Node.js](https://nodejs.org/) ≥ 18
- [Rust](https://rustup.rs/) stable
- Tauri 2 system dependencies — see the [Tauri prerequisites guide](https://v2.tauri.app/start/prerequisites/)
- The `claude` CLI on `PATH` to exercise Claude-mode terminal tabs end-to-end

## Development

### Run in dev mode

```bash
npm run tauri dev
```

Hot-reloads the React UI; the Rust side rebuilds on save.

### Production build

```bash
npm run tauri build
```

Installers land in `src-tauri/target/release/bundle/`.

## Project structure

```
xshell/
├── src/                       # React frontend
│   ├── components/            # UI components
│   ├── hooks/                 # Custom React hooks
│   ├── hosts/                 # Host routing, status, cache, and terminal reconciliation
│   ├── App.tsx                # Tab shell + main app state
│   ├── shells.ts              # Shell preset detection
│   ├── layout.ts              # Pane split / drag layout
│   └── types.ts               # Shared TypeScript types
├── src-tauri/                 # Rust backend and Cargo workspace
│   ├── src/lib.rs             # Tauri commands and thin wrappers over core
│   ├── src/main.rs            # Entry point
│   ├── src/hosts/             # Desktop host commands, events, and binary downloads
│   ├── crates/core/           # xshell-core: Tauri-free host logic and protocol
│   │   ├── src/dispatch.rs    # Method table for every host command
│   │   ├── src/launch.rs      # Terminal launch spec → command plan
│   │   └── tests/             # Integration tests and text fixtures
│   ├── crates/xshelld/        # Daemon, terminal registry, and connect bridge
│   ├── crates/hostlink/       # xshell-hostlink: SSH transport, install, and reconnect
│   └── tauri.conf.json        # Tauri 2 configuration
├── CONTEXT.md                 # Domain vocabulary
├── docs/adr/                  # Architectural decisions
├── docs/remote-hosts.md        # Remote hosts design and protocol
├── docs/screenshots/          # README screenshots
└── .github/workflows/         # CI / release automation
```

## Making changes

### Before you start

- Check existing issues to avoid duplicates.
- For significant changes, open an issue first to discuss the approach.
- Keep your branch up to date with `main`.
- Read [CONTEXT.md](./CONTEXT.md) for the vocabulary, [docs/adr/](./docs/adr/) for architectural decisions, and [docs/remote-hosts.md](./docs/remote-hosts.md) for the remote hosts design.

### Commit guidelines

- Use present-tense, imperative style ("Add cost chart" not "Added cost chart").
- Keep commits focused and atomic.
- Reference issues when applicable: `Fixes #42`.

### Pull request process

1. Update the README or in-code docs when user-facing behavior changes.
2. Make sure the project still builds: `npm run tauri build`.
3. Open a PR with a clear title, a short description of what changed and why, and screenshots for UI changes.
4. Be open to feedback — most PRs will have some back-and-forth.

## Testing

Run the automated tests before opening a PR:

```bash
npm test                                  # Vitest (frontend unit tests)
cd src-tauri
cargo test --workspace --locked            # All Rust crates
cargo test -p xshell-core --locked         # Core only
cargo test -p xshelld -p xshell-hostlink --locked # Daemon and hostlink
```

Building the desktop crate needs the Tauri system dependencies (see Prerequisites). Core, Daemon, and hostlink tests need no Tauri system dependencies. Run Daemon tests on Linux or macOS; Windows builds a stub.

Host-side logic belongs in `xshell-core`, not in `src/lib.rs`: a new command is a core function plus an entry in `dispatch.rs` (`METHODS` and the `match`), and the desktop gets a thin wrapper that keeps the frontend's parameter names. Core reads the home and temp directories only through `HostCtx`, never from `dirs::home_dir()` or `std::env::temp_dir()`.

The ignored SSH tests need a disposable SSH host or account with non-interactive key access. They install `xshelld` and stop that account's Daemon during cleanup. From `src-tauri/`:

```bash
XSHELL_E2E_SSH_TARGET=xshell-e2e \
XSHELL_E2E_BIN_DIR=/path/to/binaries \
cargo test -p xshelld --test e2e_ssh --locked -- --ignored --test-threads=1
```

The binary directory must contain `xshelld-<triple>` for the host. See [.github/workflows/ci.yml](./.github/workflows/ci.yml) for the throwaway SSH setup.

To use a locally built Daemon in `tauri dev`, build it from `src-tauri/`:

```bash
cargo build -p xshelld --profile release-daemon --target <triple> --locked
```

Use `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`, `aarch64-apple-darwin`, or `x86_64-apple-darwin`. Install the Rust target and its compiler tools first; CI shows the x86_64 musl setup.

The Desktop checks `xshelld-<triple>` beside its executable first. Debug builds then check `src-tauri/target/<triple>/release-daemon/xshelld` and, for the matching local platform, `src-tauri/target/release-daemon/xshelld`. These paths assume the default Cargo layout; lookup uses the executable's parent directory. Missing local binaries fall back to GitHub release downloads.

CI runs these checks on every push and pull request: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, `cargo test --workspace --locked`, a check that `xshell-core` has no Tauri dependency, a check that core never reads the home or temp directory outside `HostCtx`, `cargo clippy` and `cargo test` for `xshell-core` on Windows and macOS, `tsc` (via `npm run build`), and `vitest`. You can run the same commands locally.

Automated tests do not replace a manual pass: please also test your changes against a **packaged build**, not just `tauri dev`. Packaging often reveals issues that don't show up in dev mode.

Manual checklist:

- [ ] App launches without errors on your platform
- [ ] Project sidebar lists projects and sessions correctly
- [ ] Terminal tabs open and accept input (both Claude and Raw modes)
- [ ] Settings persist across restarts
- [ ] For UI changes: both light and dark themes still look right
- [ ] Remote hosts connect, share terminals across Desktops, survive disconnect/quit, reconnect, and restart terminals correctly after Upgrade now

## Reporting issues

When opening an issue, please include:

- xshell version (or commit SHA if building from source)
- OS and version
- Steps to reproduce
- Expected vs. actual behavior
- Screenshots or logs if applicable

## Feature requests

When proposing a feature:

- Describe the use case in plain terms.
- Sketch the expected behavior.
- Be open to alternative shapes — the simplest version that solves the problem usually wins.

## Code of conduct

Be kind, be respectful, focus on the work. Harassment, personal attacks, or discriminatory behavior of any kind will not be tolerated. If something feels off, flag it via an issue or directly to the maintainers.

## Questions?

- Open an issue.
- Join the discussion on existing issues.
- Or reach out to the maintainers directly.

## License

By contributing, you agree that your contributions will be licensed under the [MIT License](./LICENSE).

Thanks for helping make xshell better.
