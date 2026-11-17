#!/bin/sh
# xshell: install (or update) xshell on this machine and add it to your Ring (ADR-0019).
#
#   curl -fsSL https://github.com/Lesani/xshell/releases/download/<tag>/install.sh | sh
#   ... | sh -s -- --no-pair            # install and keep running, do not pair
#   ... | sh -s -- --relay wss://...    # pair through another Relay
#
# It downloads the xshelld build of the release this script is pinned to for this machine,
# verifies its SHA-256 before running anything, installs it under
# ~/.xshell/server/<version>+<build>/, keeps it running across restarts where it can
# (systemd user unit, else crontab @reboot, else the shell profile), moves a running older
# xshelld onto it, then shows the eight words that add this machine to your Ring. Run again
# on a machine that is already set up, it only updates.
#
# Nothing is sent but the downloads (User-Agent "xshell-install").
#
# For tests: XSHELL_INSTALL_BASE (download base URL) and XSHELL_TAG override the release.
set -eu
# Everything this script and what it starts create is private, whatever umask it inherits.
umask 077

# Pinned by publish-release.sh: the release this script belongs to.
TAG="v1.6.0-rjw.2"
REPO_BASE="https://github.com/Lesani/xshell/releases/download"

tag="${XSHELL_TAG:-$TAG}"
base="${XSHELL_INSTALL_BASE:-$REPO_BASE}"
version="${tag#v}"
pair=1
relay=""

while [ $# -gt 0 ]; do
  case "$1" in
    --no-pair) pair=0 ;;
    --relay)
      [ $# -ge 2 ] || { echo "xshell: --relay needs a URL" >&2; exit 2; }
      relay="$2"; shift ;;
    --relay=*) relay="${1#--relay=}" ;;
    *) echo "xshell: unknown option: $1" >&2; exit 2 ;;
  esac
  shift
done

[ -n "${HOME:-}" ] && [ -d "$HOME" ] || { echo "xshell: HOME is not set" >&2; exit 1; }

os=$(uname -s)
arch=$(uname -m)
case "$os/$arch" in
  Linux/x86_64|Linux/amd64) triple=x86_64-unknown-linux-musl ;;
  Linux/aarch64|Linux/arm64) triple=aarch64-unknown-linux-musl ;;
  Darwin/arm64|Darwin/aarch64) triple=aarch64-apple-darwin ;;
  Darwin/x86_64) triple=x86_64-apple-darwin ;;
  *)
    echo "xshell isn't available for $os $arch. Installation stopped." >&2
    exit 1 ;;
esac

if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL -A xshell-install -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -q -U xshell-install -O "$2" "$1"; }
else
  echo "Install curl or wget, then run this command again." >&2
  exit 1
fi

if command -v sha256sum >/dev/null 2>&1; then
  digest() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
  digest() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
  echo "Checksum verification failed. Installation stopped. Nothing was run. Download again and retry." >&2
  exit 1
fi

tmp=$(mktemp -d 2>/dev/null || mktemp -d -t xshell)
trap 'rm -rf "$tmp"' EXIT
trap 'rm -rf "$tmp"; exit 130' INT TERM

echo "Downloading xshell $version..."
fetch "$base/$tag/xshelld-$triple" "$tmp/xshelld"
fetch "$base/$tag/xshelld-$triple.sha256" "$tmp/xshelld.sha256"

echo "Verifying download..."
want=$(sed -n '1s/^\([0-9A-Fa-f]\{64\}\).*/\1/p' "$tmp/xshelld.sha256" | tr 'A-F' 'a-f')
got=$(digest "$tmp/xshelld" | tr 'A-F' 'a-f')
if [ -z "$want" ] || [ "$want" != "$got" ]; then
  echo "Checksum verification failed. Installation stopped. Nothing was run. Download again and retry." >&2
  exit 1
fi
chmod 700 "$tmp/xshelld"

server="$HOME/.xshell/server"
launcher="$HOME/.xshell/service/xshelld"
before=$(head -n 1 "$server/selected" 2>/dev/null || true)

# From here on only absolute paths: the verified download, then the installed build (S4 C6);
# never whatever `xshelld` PATH finds. Installing only installs the bytes; on a machine whose
# Daemon runs, the selection changes only through that Daemon's update (`--switch`).
dir=$("$tmp/xshelld" self-install --select)
bin="$server/$dir/xshelld"
[ -x "$bin" ] || { echo "xshell: $bin is missing" >&2; exit 1; }

if [ "$before" = "$dir" ]; then
  echo "xshell $version is already up to date."
else
  echo "Installed xshell $version."
fi

report=$("$bin" service install --json)

# A running xshelld of another build moves onto this one: in place where it can; an older
# one is asked to exit and waited for, then this build is selected (P1-3).
"$bin" self-install --switch

# Start it where nothing runs, again on every round until this build serves: an older
# Daemon may still be on its way out.
i=0
while :; do
  "$bin" service ensure >/dev/null 2>&1 || true
  status=$("$bin" service status --json 2>/dev/null || true)
  case "$status" in *"\"serving\":\"$dir\""*) break ;; esac
  i=$((i + 1))
  if [ $i -ge 120 ]; then
    echo "xshell is not running. Run xshelld serve to start it." >&2
    exit 1
  fi
  sleep 0.5
done

# The status line (copy cli.service.*), as `xshelld service status` prints it.
"$bin" service status | head -n 1

if [ -d "$HOME/.local/bin" ]; then
  ln -sf "$launcher" "$HOME/.local/bin/xshelld"
fi

[ "$pair" = 1 ] || exit 0
# Already in a Ring: nothing to pair (re-running this script only updates).
if "$bin" self-install --in-ring >/dev/null 2>&1; then
  exit 0
fi
rm -rf "$tmp"
trap - EXIT INT TERM
if [ -n "$relay" ]; then
  exec "$launcher" pair --relay "$relay"
fi
exec "$launcher" pair
