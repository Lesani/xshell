# Remote Host Daemon updates

The active Remote Host Terminal and Settings → Hosts use the same update action. The
confirmation always warns about **all** running Terminals on that Host, including those
used by other Desktops. Installation alone never stops Terminals. Completion requires a
new usable connection reporting this Desktop's version and exact known build. An accepted
restart that does not reconnect within 120 seconds reports failure; reconnect retries continue.

Linux retains its existing side-by-side installation/selection and Persistent Daemon restart.
Windows remains attach-only over SSH: configure the Host's **Daemon command**, for example
`xshelld`, and keep its owning xshell Desktop open. **Update and reopen that owning Desktop
once** to enable selected Daemon restarts. An older owner cannot gain this ability merely
by receiving a new Daemon executable. Windows has no Persistent Daemon, SSH autostart,
owner IPC service, or remote whole-Desktop replacement.

Only Windows **x86_64/MSVC** update assets are supported. The native `stage-build` command
reads bounded base64 on stdin, verifies length, SHA-256, build stamp and native identity,
and publishes an immutable private `xshelld.exe`. It never selects or starts a Daemon.
The running GUI-bound Daemon checks the exact target, persists Terminals and commits the
Host's selected directory before normal upgrade teardown. Its owning Desktop starts that
selection on reconnect, unless its newly installed bundle is newer. An invalid/unsafe
selection is a visible failure, not a silent fallback.

## Supplying the exact Windows asset to a Linux Desktop build

The existing native Windows CI lane runs the Windows process regressions, then exposes a
branch artifact named `xshelld-x86_64-pc-windows-msvc`. It contains the raw native release
Daemon, `.sha256`, and `.version.json`, built with the checkout's clean source identity.
The existing release workflow also publishes the raw Windows Daemon and its checksum.
The raw asset has no extension in the artifact, because it is a byte source, not a program
to execute on the Linux build machine.

Use the artifact from **the exact clean source revision** being built:

```sh
asset=/absolute/path/to/xshelld-x86_64-pc-windows-msvc
export XSHELL_WINDOWS_DAEMON_ASSET="$asset"
export XSHELL_WINDOWS_DAEMON_SHA256="$(awk 'NR == 1 { print $1 }' "$asset.sha256")"
# From a clean checkout matching that native artifact:
npm run tauri build -- --no-bundle
```

Both variables must be set together, or both omitted. `build.rs` checks the bound SHA-256,
x86_64 PE header and exact clean source build stamp before copying the bytes into its
`OUT_DIR` and embedding them in `xshell`. The delivered Desktop serves those embedded bytes
through its BinarySource; it does not depend on an installer copying a loose Windows file,
or fall back to a different same-version release. The staging helper additionally checks
native `--version`, protocol, platform and architecture before publishing an install.

Normal developer builds may omit the variables and report “This Desktop’s Windows Daemon
build is unavailable.” Sprint delivery builds must provide the matching native asset; a
missing asset is **not** an acceptable combined Linux/Windows delivery build.

Validation without inventing a production asset:

```sh
cd src-tauri
cargo test -p xshell windows_asset --locked
cargo test -p xshell embedded_windows_source --locked
# An incomplete build configuration must fail (no asset is generated):
XSHELL_WINDOWS_DAEMON_SHA256=invalid cargo check -p xshell --locked
# Then clear both variables for normal development:
unset XSHELL_WINDOWS_DAEMON_ASSET XSHELL_WINDOWS_DAEMON_SHA256
cargo check -p xshell --locked
```

The unit-only PE fixture never becomes a production build input. Cross-checking the
Windows Rust target proves compilation only. Native Windows CI and installed release
Desktop + OpenSSH end-to-end evidence remain separate gates; neither is replaced by a
Linux SSH test or by cross-compilation.

## Manual native Windows E2E candidates and older owner fixture

On the feature branch's **CI** workflow, manually request `build_windows_desktop=true`
for the internal `xshell-windows-candidate` Desktop/Daemon pair. Optionally also request
`build_windows_older_fixture=true` to build the fixed-purpose **1.4.9 test-only owner
pair**. The latter requires the former; both default off. Push/default CI and the native
Windows, macOS, Linux and SSH checks continue unchanged. This is not a release workflow:
there is no remote fixture commit, tag, signing, installer, or publication.

The clean target revision **R** is built, checked and uploaded first, including the raw
Windows update asset. The older fixture is then compiled in a separate disposable local
worktree based on exact R, with a separate default Cargo target directory. Its real local
source commit changes only eight self-version lines in these six metadata files:

- `src-tauri/Cargo.toml`, `src-tauri/crates/xshelld/Cargo.toml`, `src-tauri/Cargo.lock`
- `src-tauri/tauri.conf.json`, `package.json`, `package-lock.json`

No dependency constraints or locked dependency versions change. All other R source,
including the Windows profile-anchor correction and owner readiness flag, is retained.
The normal build identity is derived from that actual local fixture HEAD and commit time,
not R's workflow SHA. Both embedded-Windows-asset variables are cleared: this older owner
is a native matching Desktop/Daemon test pair, not a remote-update distribution Desktop.
It is **not** a historical 1.4.9 release and must not be described as one.

The separate `xshell-windows-older-fixture-1.4.9` artifact includes the two native x64
executables, native Daemon `--version` JSON, both image build-stamp checks, Desktop PE
product/file versions, SHA-256 checksums, full base/fixture source IDs and CI/runner
provenance, the version-only source diff, and a verified standalone Git bundle containing
fixture/base history (the runner unshallows its checkout when needed). Rechecks prove the
preserved R Desktop/Daemon and raw asset hashes remain unchanged after the older build.
Verify the artifact checksums, source diff/bundle, versions and identities before VM use.
The small metadata transform can be checked locally without a Windows build:

```sh
node --test .github/scripts/windows-older-fixture-metadata.test.mjs
```

For equal-version replacement, retain the genuine fixed-code 1.5.0 predecessor at
`1a3220e4bcd81ea42e23a0511d1600cdfdbc626b` and use the subsequent R as the actual
replacement target. Do not manufacture another same-version fixture. For older-version
Update, run the actual matching 1.4.9 fixture owner **and** Daemon: a 1.5.0 owner correctly
ignores an older selected Daemon. The Linux controller must be built from clean R with R's
exact embedded Windows asset. Native CI/provenance do not establish live E2E acceptance.
The separate VM walkthrough must still prove actual Update and same-version replacement,
Terminal restoration, default-profile nonmutation, owner-close/offline/no-SSH-autostart/
reopen lifetime, and representative failure-before-stop paths. Do not change the profile's
ownership/DACLs or override HOME to obtain acceptance.
