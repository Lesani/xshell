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

## Coordinated private-dogfood Store ownership upgrade

Before using the canonical Store ownership lease on an existing deployment, stop and drain
all obsolete Daemons and direct Pair/registration commands for that home, including custom
socket and XDG instances. Disable their restarting launchers and upgrade the related binaries
and launchers together. Preserve keys, roster, journals, Terminal data and enrollment; do not
delete mode, lock or identity files to infer absence. The new lease excludes cooperating
upgraded binaries only, not an already running obsolete process. Existing upgraded state can
then use explicit standalone Pair without starting a Daemon; actual registration and unresolved
obligations still require their protected counterpart or exact recovery.

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

## Explicit Linux install maintenance (partial #50)

Updates still **do not automatically remove old installs**. Linux offers a separate,
manual, dry-run-default maintenance command for existing obsolete keyed installs:

```sh
xshelld maintenance cleanup-installs --home /absolute/home \
  --reference-home /absolute/other-served-home --keep 1.5.0+0123456789ab
# Only during the exclusive interval described below:
xshelld maintenance cleanup-installs --home /absolute/home \
  --reference-home /absolute/other-served-home --keep 1.5.0+0123456789ab \
  --delete --confirm-quiescent
```

`--home` is mandatory: ambient `HOME` and `XSHELLD_HOME` never choose the cleanup
root. Reference homes supplement it; `--keep DIR` only adds protection. Both destructive
flags are required together. Duplicates and unsupported options are invocation errors.
The command does not start/stop anything, create state, repair permissions, acquire a
launcher lock, or change selection. Dry-run writes nothing. macOS and Windows explicitly
refuse maintenance without touching the filesystem; this is not full #50 support.

### Operator-coordinated quiescence is required

Before deletion, arrange an **exclusive maintenance interval for the UID and filesystem**:

- Stop all relevant Desktops, reconnect workers, connectors, installers, Daemons, agents
  and detached hook consumers externally. Include every machine, user, service and
  container that can use or mutate these installs. Shared/network HOMEs and incomplete
  process/user namespace views are unsupported.
- Ensure queued SSH commands, cached Desktop dials, suspended connectors, scheduled
  jobs and other delayed launchers cannot resume until this command exits. A process
  snapshot cannot find an invisible pending launch or cached hook path.
- Supply **every served reference HOME**, including cross-HOME `serve --home` and
  `XSHELLD_HOME` use. Hooks in B can name a binary installed in A; protection applies to A.
- Account for custom wrappers/settings and references stored elsewhere. Protect exact
  known targets with `--keep`. If the target set or reference-home coverage is uncertain,
  **do not delete**. Confirmation and keep flags never override a refusal.
- Use only an operator-controlled, non-consuming invocation ancestry. A parent shell or
  launcher must not hold pending product launches or cached hook consumers. Maintain all
  these conditions through command exit.

`--confirm-quiescent` attests those obligations; it is **not** distributed-quiescence
detection. The command cannot prove the operator's view covers every relevant process or
filesystem consumer. It checks for detectable namespace/mapping/mount restrictions;
normal unprivileged denial of root PID1's namespace link alone is not proof of an
incomplete same-UID inventory. Other denied/truncated relevant process data, contradictory
namespace observations, `hidepid`, `subset`, noninitial UID mapping and unresolved
identities still refuse cleanup. No privilege, global policy or account changes are needed
by the product. An ordinary active account will generally remain blocked.

### What is retained, and what can be removed

Only immediate, private, same-UID directories under `HOME/.xshell/server` with exactly
one private, regular, singly-linked executable `xshelld` are eligible. Its bounded,
non-executed known build stamp must reconstruct the semver-plus-build directory name.
No marker is required: existing keyed installs qualify. Legacy version-only installs,
unknown builds, stamp mismatches, extra/nested leaves, links, foreign ownership and
ambiguous layouts are kept, not repaired. The raw selected target (even an older or
nonexecutable one), invoking executable's install/inode, explicit keeps and recognized
generated Claude hooks in all declared HOMEs survive. Unreadable/malformed selection or
settings, custom/unrecognized commands and unresolved hook aliases block all deletion.

Every observed same-UID Daemon/connector/installer/hook consumer blocks **all** deletion.
Unknown same-UID processes outside the verified invocation ancestry also block it; a
shell, editor, `node`, `ssh` or `sleep` is not assumed harmless. Process argv/environment
are inspected without logging them. The actual process credentials are read, rather than
trusting `/proc/PID` ownership (nondumpable processes may have root-owned proc entries).
Only explicitly resolved exits before credentials can be read are skipped; failed reads
of a still-present or already relevant process block cleanup.

The full inventory and then a complete recheck precede any deletion. Each traversal
below the validated root uses Linux `openat2` with `RESOLVE_BENEATH`,
`RESOLVE_NO_SYMLINKS` and **`RESOLVE_NO_XDEV`** before unlink, including the final
reopen. Bind mounts, even on the same filesystem, cannot become deletion targets.
Unsupported kernels/filesystems or boundary errors refuse cleanup: there is **no unsafe
fallback** to device-number or symlink checks. Supported filesystem types are ext*, XFS,
Btrfs, tmpfs, ZFS and overlayfs; type detection does not override the operator's obligation
to exclude shared storage. Directory components are opened without following symlinks.

Inspection is bounded: 256 root children/leaves per directory, 4,096 processes, 128 MiB
per binary, 1 MiB per settings/process record, 512 MiB total reads over inventory and
recheck, 128 invocation ancestors, and a 30-second inspection budget checked between I/O.
Exhaustion refuses deletion rather than trusting a partial inventory.

Output distinguishes `keep`, `would-remove`, `removed` and `blocked`. A failed recheck
makes earlier `would-remove` entries **tentative**, not permission to delete. Exit codes:
**0** completed assessment/deletion, **2** invalid invocation, **1** blocked or I/O failure.
Removal unlinks only `xshelld`, then removes that now-empty directory. It stops at the
first error; `removed-executable` plus a partial-removal error means the binary is gone but
the directory remains. Earlier completed removals are reported truthfully. There is no
rollback or automatic retry. An interrupted empty/partial directory is ambiguous and is
kept on a subsequent run.

Focused tests use deterministic fault observations plus real CLI/process/filesystem
fixtures. The numeric-UID CLI fixtures are explicitly ignored by default because they
need `sudo` solely to change ownership/identity inside owned temporary fixtures (no
accounts, PAM, SSH or owner Hosts). Run them explicitly on an owned Linux development
host with `cargo test -p xshelld --test install_cleanup -- --ignored`. Native release,
macOS/Windows refusal and a privileged isolated real bind-mount scenario remain separate
validation gates; mocked EXDEV/unsupported-open tests are not native bind-mount proof.
