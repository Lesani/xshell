#!/bin/sh
# Tests for scripts/install.sh (against a local HTTP server, scratch homes only) and for
# scripts/publish-release.sh (with a fake `gh`: nothing reaches GitHub).
#
#   sh scripts/install.test.sh [path/to/xshelld]
#
# The xshelld defaults to src-tauri/target/debug/xshelld (a debug build: the trial's settle
# time is shortened through its test hook). Needs python3, strip, git. Never touches the
# real home, crontab, systemd or shell profile: every run uses a scratch HOME and
# XSHELL_SERVICE_MECHANISM=none.
set -eu
umask 0022

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/.." && pwd)
src="${1:-$root/src-tauri/target/debug/xshelld}"
[ -x "$src" ] || { echo "no xshelld at $src (cargo build -p xshelld)" >&2; exit 1; }

work=$(mktemp -d "${TMPDIR:-/tmp}/xshell-install-test.XXXXXX")
server_pid=
cleanup() {
  for pidfile in "$work"/*/run/xshell/daemon.pid; do
    [ -f "$pidfile" ] || continue
    pid=$(cat "$pidfile" 2>/dev/null || true)
    [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
  done
  [ -n "$server_pid" ] && kill "$server_pid" 2>/dev/null || true
  sleep 1
  rm -rf "$work"
}
trap cleanup EXIT INT TERM

passed=0
ok() { passed=$((passed + 1)); echo "ok - $1"; }
fail() { echo "FAIL - $1" >&2; [ -n "${2:-}" ] && printf '%s\n' "$2" >&2; exit 1; }

# ---- release assets: build A (this xshelld) and build B (another commit stamped in) ----
strip -o "$work/xshelld-a" "$src"
version=$("$work/xshelld-a" --version | sed -n 's/.*"version":"\([^"]*\)".*/\1/p')
commit=$("$work/xshelld-a" --version | sed -n 's/.*"commit":"\([0-9a-f]\{12\}\)".*/\1/p')
[ -n "$version" ] && [ -n "$commit" ] || fail "cannot read the build of $src"
other=0123456789ab
[ "$commit" != "$other" ] || other=ba9876543210
python3 -I - "$work/xshelld-a" "$work/xshelld-b" "$commit" "$other" <<'EOF'
import sys
src, dst, old, new = sys.argv[1:5]
b = open(src, 'rb').read()
m = b'@@XSHELL-BUILD-V1:' + old.encode()
assert m in b, "no stamp"
open(dst, 'wb').write(b.replace(m, b'@@XSHELL-BUILD-V1:' + new.encode()))
EOF
chmod 700 "$work/xshelld-b"
dir_of() { "$1" --version | python3 -I -c 'import json,sys; v=json.load(sys.stdin); b=v["build"]; print(v["version"] + "+" + b["commit"] + ("+d." + b["dirty"] if b.get("dirty") else ""))'; }
dir_a=$(dir_of "$work/xshelld-a")
dir_b=$(dir_of "$work/xshelld-b")

case "$(uname -m)" in
  x86_64|amd64) triple=x86_64-unknown-linux-musl ;;
  aarch64|arm64) triple=aarch64-unknown-linux-musl ;;
  *) fail "unsupported test machine" ;;
esac
www="$work/www"
for t in a b bad; do mkdir -p "$www/v$t"; done
cp "$work/xshelld-a" "$www/va/xshelld-$triple"
cp "$work/xshelld-b" "$www/vb/xshelld-$triple"
cp "$work/xshelld-a" "$www/vbad/xshelld-$triple"
(cd "$www/va" && sha256sum "xshelld-$triple" > "xshelld-$triple.sha256")
(cd "$www/vb" && sha256sum "xshelld-$triple" > "xshelld-$triple.sha256")
# A sidecar that does not match (the download was tampered with).
(cd "$www/vb" && cp "xshelld-$triple.sha256" "../vbad/xshelld-$triple.sha256")

python3 -I -u -m http.server --bind 127.0.0.1 0 --directory "$www" > "$work/http.log" 2>&1 &
server_pid=$!
i=0
until port=$(sed -n 's/.*port \([0-9]*\).*/\1/p' "$work/http.log" | head -n 1) && [ -n "$port" ]; do
  i=$((i + 1)); [ $i -lt 100 ] || fail "http server did not start" "$(cat "$work/http.log")"
  sleep 0.1
done
base="http://127.0.0.1:$port"

# One scratch machine: HOME, XDG_RUNTIME_DIR, a PATH without any xshelld.
machine() {
  m="$work/$1"
  mkdir -p "$m/home" "$m/run" "$m/bin"
  chmod 700 "$m/run"
}
run_install() {
  m="$work/$1"; tag="$2"; shift 2
  env -i HOME="$m/home" XDG_RUNTIME_DIR="$m/run" PATH="$m/bin:/usr/bin:/bin" \
    XSHELL_SERVICE_MECHANISM=none XSHELLD_LOGIN_ENV=0 XSHELLD_TEST_TRIAL_SETTLE_MS=500 \
    XSHELL_INSTALL_BASE="$base" XSHELL_TAG="$tag" \
    sh "$here/install.sh" --no-pair "$@"
}
serving() {
  m="$work/$1"
  env -i HOME="$m/home" XDG_RUNTIME_DIR="$m/run" PATH=/usr/bin:/bin XSHELL_SERVICE_MECHANISM=none \
    "$m/home/.xshell/service/xshelld" service status --json | sed -n 's/.*"serving":"\([^"]*\)".*/\1/p'
}

sh -n "$here/install.sh" || fail "install.sh does not parse"
ok "install.sh parses"

# ---- fresh install ----
machine fresh
command -v xshelld >/dev/null 2>&1 && PATH_HAS=1 || PATH_HAS=0
out=$(run_install fresh va 2>&1) || fail "fresh install failed" "$out"
h="$work/fresh/home"
[ -x "$h/.xshell/server/$dir_a/xshelld" ] || fail "not installed under $dir_a" "$out"
cmp -s "$h/.xshell/server/$dir_a/xshelld" "$work/xshelld-a" || fail "installed bytes differ"
[ "$(cat "$h/.xshell/server/selected")" = "$dir_a" ] || fail "selected is not $dir_a"
[ -x "$h/.xshell/service/xshelld" ] || fail "no launcher"
[ "$(serving fresh)" = "$dir_a" ] || fail "$dir_a is not serving" "$out"
case "$out" in *"Downloading xshell a..."*"Verifying download..."*"Installed xshell a."*) ;; *) fail "unexpected output" "$out" ;; esac
# The one status line (copy cli.service.*): nothing is verified to restart it here
# (XSHELL_SERVICE_MECHANISM=none), so never the automatic promise, and no old two-line text.
case "$out" in *"xshell is running. After this machine restarts, it starts only when you next log in to a shell on this machine."*) ;; *) fail "no status line" "$out" ;; esac
case "$out" in *"starts after reboot"*|*"sign in with your usual shell"*) fail "old status text" "$out" ;; esac
[ ! -e "$h/.local/bin/xshelld" ] || fail "linked into a ~/.local/bin that did not exist"
ok "fresh install: verified, installed, selected, serving (PATH without xshelld; ambient PATH had one: $PATH_HAS)"

# ---- run again: nothing to do ----
pid_before=$(cat "$work/fresh/run/xshell/daemon.pid")
mkdir -p "$h/.local/bin"
out=$(run_install fresh va 2>&1) || fail "second install failed" "$out"
case "$out" in *"xshell a is already up to date."*) ;; *) fail "re-run did not say up to date" "$out" ;; esac
[ "$(cat "$work/fresh/run/xshell/daemon.pid")" = "$pid_before" ] || fail "re-run restarted the Daemon"
[ "$(readlink "$h/.local/bin/xshelld")" = "$h/.xshell/service/xshelld" ] || fail "~/.local/bin/xshelld is not the launcher"
ok "re-run on an installed machine changes nothing; links ~/.local/bin/xshelld"

# ---- run again with a newer release: only updates, in place ----
out=$(run_install fresh vb 2>&1) || fail "update failed" "$out"
case "$out" in *"Installed xshell b."*) ;; *) fail "update did not install" "$out" ;; esac
[ "$(cat "$h/.xshell/server/selected")" = "$dir_b" ] || fail "selected is not $dir_b"
[ "$(serving fresh)" = "$dir_b" ] || fail "$dir_b is not serving" "$out"
pid_after=$(cat "$work/fresh/run/xshell/daemon.pid")
[ "$pid_after" = "$pid_before" ] || fail "the update did not keep the Daemon's process ($pid_before -> $pid_after)"
[ "$(readlink "/proc/$pid_after/exe")" = "$h/.xshell/server/$dir_b/xshelld" ] || fail "the Daemon does not run $dir_b"
ok "re-run with a newer release updates the running Daemon in place"

# ---- an older Daemon (no in-place update) that acknowledges and exits late (P1-3) ----
for mech in none profile; do
  machine "old-$mech"
  mo="$work/old-$mech"
  # Build a plays a Daemon from before in-place updates: no daemon.stage, exits on an
  # upgrade, and only 1.5 s later (still holding its start lock).
  out=$(env -i HOME="$mo/home" XDG_RUNTIME_DIR="$mo/run" PATH="$mo/bin:/usr/bin:/bin" \
    XSHELL_SERVICE_MECHANISM="$mech" XSHELLD_LOGIN_ENV=0 \
    XSHELLD_TEST_HIDE_CAPABILITIES=daemon.stage,daemon.upgrade-selected \
    XSHELLD_TEST_NO_REEXEC=1 XSHELLD_TEST_EXIT_DELAY_MS=1500 \
    XSHELL_INSTALL_BASE="$base" XSHELL_TAG=va sh "$here/install.sh" --no-pair 2>&1) \
    || fail "old Daemon install ($mech) failed" "$out"
  [ "$(serving "old-$mech")" = "$dir_a" ] || fail "old Daemon ($mech) not serving"
  old_pid=$(cat "$mo/run/xshell/daemon.pid")
  out=$(env -i HOME="$mo/home" XDG_RUNTIME_DIR="$mo/run" PATH="$mo/bin:/usr/bin:/bin" \
    XSHELL_SERVICE_MECHANISM="$mech" XSHELLD_LOGIN_ENV=0 \
    XSHELL_INSTALL_BASE="$base" XSHELL_TAG=vb sh "$here/install.sh" --no-pair 2>&1) \
    || fail "update of an old Daemon ($mech) failed" "$out"
  [ "$(serving "old-$mech")" = "$dir_b" ] || fail "update of an old Daemon ($mech): $dir_b not serving" "$out"
  [ "$(cat "$mo/home/.xshell/server/selected")" = "$dir_b" ] || fail "old Daemon ($mech): $dir_b not selected"
  new_pid=$(cat "$mo/run/xshell/daemon.pid")
  [ "$new_pid" != "$old_pid" ] || fail "old Daemon ($mech): still the old process"
  ! kill -0 "$old_pid" 2>/dev/null || fail "old Daemon ($mech) still runs"
  ok "an older Daemon ($mech) that exits late is replaced by the new build"
done

# ---- whatever umask the installer inherits, everything it creates is private (P1-7) ----
for um in 0000 0002 0277; do
  machine "um$um"
  mu="$work/um$um"
  # A fake systemd user manager: nothing real is touched.
  printf '#!/bin/sh\nexit 0\n' > "$mu/bin/systemctl"
  printf '#!/bin/sh\nexit 0\n' > "$mu/bin/loginctl"
  chmod 755 "$mu/bin/systemctl" "$mu/bin/loginctl"
  out=$( (umask "$um"; env -i HOME="$mu/home" XDG_RUNTIME_DIR="$mu/run" PATH="$mu/bin:/usr/bin:/bin" \
    XSHELL_SERVICE_MECHANISM=systemd XSHELLD_LOGIN_ENV=0 \
    XSHELL_INSTALL_BASE="$base" XSHELL_TAG=va sh "$here/install.sh" --no-pair) 2>&1) \
    || fail "install with umask $um failed" "$out"
  hu="$mu/home"
  for d in .xshell .xshell/server ".xshell/server/$dir_a" .xshell/service .xshell/log .config .config/systemd .config/systemd/user; do
    m=$(stat -c %a "$hu/$d" 2>/dev/null || echo missing)
    [ "$m" = 700 ] || fail "umask $um: $d is $m, not 700" "$out"
  done
  [ "$(stat -c %a "$hu/.xshell/server/$dir_a/xshelld")" = 700 ] || fail "umask $um: binary mode"
  m=$(stat -c %a "$hu/.xshell/server/selected"); [ $(( 0$m & 077 )) -eq 0 ] || fail "umask $um: selected is $m"
  [ "$(serving "um$um")" = "$dir_a" ] || fail "umask $um: not serving" "$out"
  ok "umask $um: every directory 0700, files private, serving"
done

# `xshelld service install` run directly (not through install.sh) under a loose umask.
machine direct
md="$work/direct"
printf '#!/bin/sh\nexit 0\n' > "$md/bin/systemctl"; cp "$md/bin/systemctl" "$md/bin/loginctl"
chmod 755 "$md/bin/systemctl" "$md/bin/loginctl"
( umask 0002; env -i HOME="$md/home" XDG_RUNTIME_DIR="$md/run" PATH="$md/bin:/usr/bin:/bin" \
  "$work/xshelld-a" service install --mechanism systemd >/dev/null ) || fail "direct service install failed"
for d in .config .config/systemd .config/systemd/user .xshell .xshell/service; do
  m=$(stat -c %a "$md/home/$d" 2>/dev/null || echo missing)
  [ "$m" = 700 ] || fail "direct service install, umask 0002: $d is $m"
done
ok "service install creates missing ancestors 0700 under umask 0002"

# ---- a download that does not match its checksum ----
machine bad
if out=$(run_install bad vbad 2>&1); then fail "a bad checksum was accepted" "$out"; fi
case "$out" in *"Checksum verification failed. Installation stopped. Nothing was run. Download again and retry."*) ;; *) fail "no checksum message" "$out" ;; esac
[ ! -e "$work/bad/home/.xshell" ] || fail "something was installed after a bad checksum"
ok "bad checksum: nothing installed, nothing run"

# ---- an unsupported platform ----
machine odd
printf '#!/bin/sh\ncase "$1" in -s) echo FreeBSD;; -m) echo x86_64;; esac\n' > "$work/odd/bin/uname"
chmod 755 "$work/odd/bin/uname"
if out=$(run_install odd va 2>&1); then fail "an unsupported platform was accepted" "$out"; fi
case "$out" in *"xshell isn't available for FreeBSD x86_64. Installation stopped."*) ;; *) fail "no platform message" "$out" ;; esac
[ ! -e "$work/odd/home/.xshell" ] || fail "something was installed on an unsupported platform"
ok "unsupported platform refused"

# ---- publish-release.sh with a fake gh ----
sh -n "$here/publish-release.sh" || fail "publish-release.sh does not parse"
pub="$work/pub"
mkdir -p "$pub/repo/scripts" "$pub/repo/src-tauri" "$pub/bin"
cat > "$pub/bin/gh" <<'EOF'
#!/bin/sh
echo "gh $*" >> "$GH_LOG"
case "$1 $2" in
  "api repos/"*)
    case "$2" in
      */commits/*) [ "${GH_COMMIT_ON_REPO:-1}" = 1 ] || exit 1; echo '{}' ;;
      */git/ref/tags/*) [ -n "${GH_TAG_SHA:-}" ] || exit 1; echo "$GH_TAG_SHA" ;;
    esac ;;
  "release view") [ "${GH_RELEASE_EXISTS:-0}" = 1 ] || exit 1 ;;
  "release create") ;;
  *) exit 2 ;;
esac
EOF
cat > "$pub/bin/file" <<'EOF'
#!/bin/sh
echo "ELF 64-bit LSB pie executable, static-pie linked, stripped"
EOF
cat > "$pub/bin/stamp" <<'EOF'
#!/bin/sh
grep -q "@@XSHELL-BUILD-V1:$2:" "$1"
EOF
chmod 755 "$pub/bin/gh" "$pub/bin/file" "$pub/bin/stamp"
(
  cd "$pub/repo"
  git init -q .
  git -c user.email=t@example.invalid -c user.name=t commit -q --allow-empty -m init
  short=$(git rev-parse --short=12 HEAD)
  printf '[package]\nname = "xshell"\nversion = "9.9.9-test.1"\n' > src-tauri/Cargo.toml
  sed 's/^TAG=".*"$/TAG="v9.9.9-test.1"/' "$here/install.sh" > scripts/install.sh
  for t in x86_64-unknown-linux-musl aarch64-unknown-linux-musl; do
    mkdir -p "src-tauri/target/$t/release-daemon"
  done
  printf '.gitignore\nsrc-tauri/target/\n' > .gitignore
  git add . && git -c user.email=t@example.invalid -c user.name=t commit -q -m files
)
sha=$(git -C "$pub/repo" rev-parse HEAD)
short=$(git -C "$pub/repo" rev-parse --short=12 HEAD)
fake_daemon() {
  printf '#!/bin/sh\n# @@XSHELL-BUILD-V1:%s:1@@\necho %s\n' "$2" \
    "'{\"name\":\"xshelld\",\"version\":\"9.9.9-test.1\",\"build\":{\"commit\":\"$2\",\"dirty\":null,\"time\":1}}'" \
    > "$pub/repo/src-tauri/target/$1/release-daemon/xshelld"
  chmod 755 "$pub/repo/src-tauri/target/$1/release-daemon/xshelld"
}
fake_daemon x86_64-unknown-linux-musl "$short"
fake_daemon aarch64-unknown-linux-musl "$short"
publish() {
  : > "$pub/gh.log"
  env PATH="$pub/bin:/usr/bin:/bin" GH_LOG="$pub/gh.log" XSHELL_PUBLISH_ROOT="$pub/repo" \
    XSHELL_PUBLISH_STAMP="$pub/bin/stamp" "$@" sh "$here/publish-release.sh"
}
created() { grep -q "^gh release create" "$pub/gh.log"; }

out=$(publish env 2>&1) || fail "publish failed" "$out"
grep -q "^gh release create v9.9.9-test.1 --repo Lesani/xshell --prerelease --target $sha " "$pub/gh.log" || fail "wrong release command" "$(cat "$pub/gh.log")"
for f in xshelld-x86_64-unknown-linux-musl xshelld-x86_64-unknown-linux-musl.sha256 xshelld-aarch64-unknown-linux-musl xshelld-aarch64-unknown-linux-musl.sha256 install.sh; do
  grep -q "/$f\( \|$\)" "$pub/gh.log" || fail "release lacks $f" "$(cat "$pub/gh.log")"
done
ok "publish: pre-release of the exact commit with both builds, sidecars and install.sh"

out=$(publish env GH_COMMIT_ON_REPO=0 2>&1) && fail "published a commit missing on the repo" "$out"
created && fail "created a release for a commit missing on the repo"
ok "publish refuses a commit that is not on the repository"

out=$(publish env GH_RELEASE_EXISTS=1 2>&1) && fail "published over an existing release" "$out"
created && fail "created a release that exists"
ok "publish refuses an existing release"

out=$(publish env GH_TAG_SHA=0000000000000000000000000000000000000000 2>&1) && fail "published over a tag at another commit" "$out"
created && fail "created a release over a foreign tag"
out=$(publish env GH_TAG_SHA="$sha" 2>&1) || fail "a tag at the same commit was refused" "$out"
ok "publish refuses a tag at another commit, accepts one at this commit"

fake_daemon aarch64-unknown-linux-musl 0123456789ab
out=$(publish env 2>&1) && fail "published a build of another commit" "$out"
created && fail "created a release with a foreign build"
fake_daemon aarch64-unknown-linux-musl "$short"
ok "publish refuses a build whose stamp names another commit"

sed -i 's/^TAG=".*"$/TAG="v0.0.1"/' "$pub/repo/scripts/install.sh"
git -C "$pub/repo" -c user.email=t@example.invalid -c user.name=t commit -q -am pin
out=$(publish env 2>&1) && fail "published with a wrongly pinned install.sh" "$out"
created && fail "created a release with a wrongly pinned install.sh"
ok "publish refuses an install.sh pinned to another release"

echo dirty >> "$pub/repo/scripts/install.sh"
out=$(publish env 2>&1) && fail "published from a dirty tree" "$out"
created && fail "created a release from a dirty tree"
ok "publish refuses a dirty tree"

echo "all $passed install/publish tests passed"
