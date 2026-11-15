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

# Pinned by publish-release.sh: the release this script belongs to.
TAG="v1.5.0"
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
    # COPY-GAP: no approved text for an unsupported platform.
    echo "Unsupported platform: $os $arch" >&2
    exit 1 ;;
esac

if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL -A xshell-install -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -q -U xshell-install -O "$2" "$1"; }
else
  # COPY-GAP: no approved text for a missing downloader.
  echo "curl or wget is required." >&2
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

# From here on only absolute paths: the verified download, then the installed build and the
# launcher (S4 C6); never whatever `xshelld` PATH finds.
"$tmp/xshelld" self-install --select
dir=$(head -n 1 "$server/selected")
bin="$server/$dir/xshelld"
[ -x "$bin" ] || { echo "xshell: $bin is missing" >&2; exit 1; }

if [ "$before" = "$dir" ]; then
  echo "xshell $version is already up to date."
else
  echo "Installed xshell $version."
fi

report=$("$bin" service install --json)
mechanism=$(printf '%s\n' "$report" | sed -n 's/.*"mechanism":"\([a-z]*\)".*/\1/p')

# A running xshelld of another build moves onto this one (in place where it can).
"$launcher" self-install --switch
"$launcher" service ensure

# Wait until this build serves.
i=0
while :; do
  status=$("$launcher" service status --json 2>/dev/null || true)
  case "$status" in *"\"serving\":\"$dir\""*) break ;; esac
  i=$((i + 1))
  if [ $i -ge 120 ]; then
    echo "xshell is not running. Run xshelld serve to start it." >&2
    exit 1
  fi
  sleep 0.5
done

case "$mechanism" in
  systemd) echo "xshell is running and starts after reboot (systemd)." ;;
  cron) echo "xshell is running and starts after reboot (cron)." ;;
  profile)
    echo "xshell is running, but will not start automatically after this machine restarts."
    echo "After this machine restarts, sign in with your usual shell to start xshell." ;;
esac

if [ -d "$HOME/.local/bin" ]; then
  ln -sf "$launcher" "$HOME/.local/bin/xshelld"
fi

[ "$pair" = 1 ] || exit 0
rm -rf "$tmp"
trap - EXIT INT TERM
if [ -n "$relay" ]; then
  exec "$launcher" pair --relay "$relay"
fi
exec "$launcher" pair
