#!/bin/sh
# Publish locally built Daemons as a GitHub pre-release that install.sh downloads (ADR-0019,
# D8). No CI: everything is built and checked here; GitHub only hosts the files.
#
#   scripts/publish-release.sh [--dry-run]
#
# Inputs: the version in src-tauri/Cargo.toml and `--profile release-daemon` builds in
# src-tauri/target/<triple>/release-daemon/xshelld for x86_64- and aarch64-unknown-linux-musl
# (Darwin triples too, when built). Every version is published once: give each pre-release
# its own semver version (for example 1.6.0-rjw.3).
#
# Checks before anything is published (S4 C9):
# - the working tree is clean, and its commit exists on the release repository;
# - each build is statically linked, its build stamp names exactly that commit (not dirty),
#   and the one for this machine reports the version and commit when it runs;
# - install.sh is pinned to v<version>;
# - no release v<version> exists, and no tag v<version> points at another commit (never
#   --clobber).
# Then the release is created as a pre-release whose tag targets that exact commit, with
# xshelld-<triple>, xshelld-<triple>.sha256 and install.sh.
#
# Environment (tests): XSHELL_PUBLISH_REPO (default Lesani/xshell), XSHELL_PUBLISH_ROOT (the
# checkout, default this script's parent), XSHELL_PUBLISH_STAMP (the build-stamp checker:
# a command taking <binary> <expected display>; default the build_stamp example).
set -eu

dry=0
case "${1:-}" in
  --dry-run) dry=1 ;;
  "") ;;
  *) echo "usage: $0 [--dry-run]" >&2; exit 2 ;;
esac

root="${XSHELL_PUBLISH_ROOT:-$(cd "$(dirname "$0")/.." && pwd)}"
repo="${XSHELL_PUBLISH_REPO:-Lesani/xshell}"
cd "$root"

fail() { echo "publish-release: $*" >&2; exit 1; }

command -v gh >/dev/null 2>&1 || fail "gh is required"
command -v file >/dev/null 2>&1 || fail "file is required"

version=$(sed -n '/^\[package\]/,/^\[/s/^version *= *"\([^"]*\)".*/\1/p' src-tauri/Cargo.toml | head -n 1)
[ -n "$version" ] || fail "no version in src-tauri/Cargo.toml"
tag="v$version"

[ -z "$(git status --porcelain)" ] || fail "the working tree is not clean"
sha=$(git rev-parse HEAD)
short=$(git rev-parse --short=12 HEAD)

pinned=$(sed -n 's/^TAG="\(.*\)"$/\1/p' scripts/install.sh | head -n 1)
[ "$pinned" = "$tag" ] || fail "scripts/install.sh is pinned to $pinned, not $tag"

gh api "repos/$repo/commits/$sha" >/dev/null 2>&1 || fail "commit $sha is not on $repo; push it first"

if gh release view "$tag" --repo "$repo" >/dev/null 2>&1; then
  fail "release $tag exists already; publish a new version"
fi
if existing=$(gh api "repos/$repo/git/ref/tags/$tag" --jq .object.sha 2>/dev/null); then
  [ "$existing" = "$sha" ] || fail "tag $tag exists at $existing, not at $sha"
fi

stamp() {
  if [ -n "${XSHELL_PUBLISH_STAMP:-}" ]; then
    $XSHELL_PUBLISH_STAMP "$1" "$2"
  else
    cargo run -q --manifest-path src-tauri/Cargo.toml -p xshell-protocol \
      --no-default-features --locked --example build_stamp -- "$1" "$2"
  fi
}

case "$(uname -s)/$(uname -m)" in
  Linux/x86_64) native=x86_64-unknown-linux-musl ;;
  Linux/aarch64) native=aarch64-unknown-linux-musl ;;
  Darwin/arm64) native=aarch64-apple-darwin ;;
  Darwin/x86_64) native=x86_64-apple-darwin ;;
  *) native= ;;
esac

out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT INT TERM
files=""
for triple in x86_64-unknown-linux-musl aarch64-unknown-linux-musl aarch64-apple-darwin x86_64-apple-darwin; do
  bin="src-tauri/target/$triple/release-daemon/xshelld"
  case "$triple" in
    *-linux-musl) [ -f "$bin" ] || fail "missing $bin: cargo build -p xshelld --profile release-daemon --target $triple" ;;
    *) [ -f "$bin" ] || continue ;;
  esac
  case "$triple" in
    *-linux-musl)
      file -b "$bin" | grep -q "static" || fail "$bin is not statically linked" ;;
  esac
  stamp "$bin" "$short" >/dev/null || fail "$bin is not a build of $short"
  if [ "$triple" = "$native" ]; then
    v=$("$bin" --version) || fail "$bin --version failed"
    case "$v" in *"\"version\":\"$version\""*) ;; *) fail "$bin reports $v, not $version" ;; esac
    case "$v" in *"\"commit\":\"$short\",\"dirty\":null"*) ;; *) fail "$bin reports $v, not commit $short" ;; esac
  fi
  cp "$bin" "$out/xshelld-$triple"
  (cd "$out" && sha256sum "xshelld-$triple" > "xshelld-$triple.sha256")
  files="$files $out/xshelld-$triple $out/xshelld-$triple.sha256"
done
cp scripts/install.sh "$out/install.sh"
files="$files $out/install.sh"

notes="xshell $version (commit $short). Install or update a machine without a window:

    curl -fsSL https://github.com/$repo/releases/download/$tag/install.sh | sh"

if [ "$dry" = 1 ]; then
  echo "would run: gh release create $tag --repo $repo --prerelease --target $sha --title \"xshell $version\"$files"
  exit 0
fi
# shellcheck disable=SC2086
gh release create "$tag" --repo "$repo" --prerelease --target "$sha" \
  --title "xshell $version" --notes "$notes" $files
echo "published $tag ($short) to $repo"
