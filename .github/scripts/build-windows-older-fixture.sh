#!/usr/bin/env bash
# Manual internal 1.4.9 owner pair only; run from the clean candidate's repository root.
set -euo pipefail
base="$(git rev-parse HEAD)"
root="$PWD"
production="$RUNNER_TEMP/xshell-windows-candidate"
raw="$root/src-tauri/xshelld-x86_64-pc-windows-msvc"
stage="$RUNNER_TEMP/xshell-windows-older-fixture-1.4.9"
fixture="$RUNNER_TEMP/xshell-older-fixture-source"
test "$base" = "$GITHUB_SHA"
test -z "$(git status --porcelain --untracked-files=no)"
(cd "$production" && sha256sum -c SHA256SUMS)
sha256sum "$raw" > "$RUNNER_TEMP/xshell-production-raw.sha256"
# A standalone bundle must include the base's ancestry, not a missing shallow parent.
if test "$(git rev-parse --is-shallow-repository)" = true; then
  git fetch --unshallow origin
fi
mkdir "$stage"
cp "$production/SHA256SUMS" "$stage/production-pair.SHA256SUMS"
cp "$RUNNER_TEMP/xshell-production-raw.sha256" "$stage/production-raw.SHA256SUMS"
git worktree add --detach "$fixture" "$base"
trap 'git -C "$root" worktree remove --force "$fixture"' EXIT
node .github/scripts/windows-older-fixture-metadata.mjs "$fixture"
cd "$fixture"
metadata=(src-tauri/Cargo.toml src-tauri/crates/xshelld/Cargo.toml src-tauri/Cargo.lock src-tauri/tauri.conf.json package.json package-lock.json)
printf '%s\n' "${metadata[@]}" | sort > "$stage/expected-metadata-paths.txt"
git diff --name-only | sort > "$stage/actual-metadata-paths.txt"
diff -u "$stage/expected-metadata-paths.txt" "$stage/actual-metadata-paths.txt"
git diff --check
git -c user.name='xshell internal fixture' -c user.email='noreply@xshell.sh' add -- "${metadata[@]}"
git -c user.name='xshell internal fixture' -c user.email='noreply@xshell.sh' commit \
  -m 'test: compile corrected Windows owner pair as 1.4.9 (local fixture only)' \
  -m "Test-only versioned build of $base, not a historical release. No dependency versions changed." \
  -m 'Co-Authored-By: OpenAI <noreply@openai.com>'
fixture_commit="$(git rev-parse HEAD)"
test "$(git rev-parse HEAD^)" = "$base"
test -z "$(git status --porcelain)"
git diff "$base" HEAD -- > "$stage/source.diff"
git show --no-patch --format=fuller HEAD > "$stage/source-commit.txt"
git bundle create "$stage/source.bundle" HEAD
git bundle verify "$stage/source.bundle" 2>&1 | tee "$stage/bundle-verification.txt"
# A worktree's own target dir prevents fixture builds overwriting production R outputs.
unset CARGO_TARGET_DIR XSHELL_WINDOWS_DAEMON_ASSET XSHELL_WINDOWS_DAEMON_SHA256
export XSHELL_BUILD_COMMIT="$(git rev-parse HEAD | cut -c1-12)"
export XSHELL_BUILD_TIME="$(git log -1 --format=%ct HEAD)"
export XSHELL_BUILD_DIRTY=""
{ rustc -Vv; cargo -V; node -v; npm -v; } > "$stage/toolchain-versions.txt"
npm ci
npm run build
cargo build --manifest-path src-tauri/Cargo.toml -p xshelld --profile release-daemon --locked
cargo build --manifest-path src-tauri/Cargo.toml -p xshell --release --features tauri/custom-protocol --locked
cp src-tauri/target/release-daemon/xshelld.exe "$stage/xshelld.exe"
cp src-tauri/target/release/xshell.exe "$stage/xshell.exe"
for image in xshelld xshell; do
  cargo run -q --manifest-path src-tauri/Cargo.toml -p xshell-protocol --no-default-features --locked --example build_stamp -- \
    "$stage/$image.exe" "$XSHELL_BUILD_COMMIT" "$XSHELL_BUILD_TIME" | tee "$stage/$image.build-stamp.txt"
done
"$stage/xshelld.exe" --version | tee "$stage/xshelld.version.json"
node --input-type=module - "$stage" <<'NODE'
import assert from 'node:assert/strict';
import fs from 'node:fs';
const stage = process.argv[2];
const v = JSON.parse(fs.readFileSync(`${stage}/xshelld.version.json`, 'utf8'));
assert.equal(v.version, '1.4.9');
assert.equal(v.os, 'windows');
assert.equal(v.arch, 'x86_64');
assert.deepEqual(v.build, { commit: process.env.XSHELL_BUILD_COMMIT, dirty: null, time: Number(process.env.XSHELL_BUILD_TIME) });
for (const name of ['xshell', 'xshelld']) {
  const image = fs.readFileSync(`${stage}/${name}.exe`);
  assert.equal(image.readUInt16LE(0), 0x5a4d);
  const pe = image.readUInt32LE(0x3c);
  assert.equal(image.readUInt32LE(pe), 0x4550);
  assert.equal(image.readUInt16LE(pe + 4), 0x8664);
}
NODE
export XSHELL_FIXTURE_STAGE="$(cygpath -w "$stage")"
pwsh -NoProfile -Command '
  $ErrorActionPreference = "Stop"
  $v = (Get-Item (Join-Path $env:XSHELL_FIXTURE_STAGE "xshell.exe")).VersionInfo
  $v | Format-List FileVersion, ProductVersion | Out-String | Tee-Object -FilePath (Join-Path $env:XSHELL_FIXTURE_STAGE "desktop-pe-version.txt")
  if ($v.ProductVersion -ne "1.4.9" -or $v.FileMajorPart -ne 1 -or $v.FileMinorPart -ne 4 -or $v.FileBuildPart -ne 9) { throw "Desktop PE version differs from fixture metadata" }
'
test -z "$(git status --porcelain --untracked-files=no)"
export XSHELL_FIXTURE_BASE="$base" XSHELL_FIXTURE_COMMIT="$fixture_commit"
node --input-type=module - "$stage" <<'NODE'
import fs from 'node:fs';
import os from 'node:os';
const e = process.env;
fs.writeFileSync(`${process.argv[2]}/provenance.json`, JSON.stringify({
  purpose: 'Internal test-only 1.4.9 owner pair compiled from corrected source; not a historical release',
  baseCommit: e.XSHELL_FIXTURE_BASE, fixtureCommit: e.XSHELL_FIXTURE_COMMIT,
  buildCommit: e.XSHELL_BUILD_COMMIT, buildTime: Number(e.XSHELL_BUILD_TIME), buildDirty: null,
  repository: e.GITHUB_REPOSITORY, workflow: e.GITHUB_WORKFLOW, ref: e.GITHUB_REF,
  runId: e.GITHUB_RUN_ID, runAttempt: e.GITHUB_RUN_ATTEMPT,
  runUrl: `${e.GITHUB_SERVER_URL}/${e.GITHUB_REPOSITORY}/actions/runs/${e.GITHUB_RUN_ID}`,
  runner: e.RUNNER_NAME, runnerOS: e.RUNNER_OS, runnerArch: e.RUNNER_ARCH,
  runnerImage: e.ImageOS, runnerImageVersion: e.ImageVersion, osRelease: os.release(),
}, null, 2) + '\n');
NODE
# Recheck preserved production pair/raw only after the older build has finished.
(cd "$production" && sha256sum -c SHA256SUMS) | tee "$stage/production-pair-recheck.txt"
sha256sum -c "$RUNNER_TEMP/xshell-production-raw.sha256" | tee "$stage/production-raw-recheck.txt"
(cd "$stage" && sha256sum xshell.exe xshelld.exe source.bundle source.diff > SHA256SUMS)
