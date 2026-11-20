#!/usr/bin/env bash
# M3's checks in the Neptuno test VM, after dev/vm-install. Run as root:
#   ~/.local/share/nx-vm/vm ssh 'bash -s' < tests/vm/m3.sh
# Adds, removes and rolls back a -devel package as `tester` and checks, each
# time, that /usr shows it with no reboot, and that the prefix backend works.
set -euo pipefail
fail() { echo "FAIL: $*" >&2; exit 1; }
nx() { runuser -l tester -c "nx $*"; }
header=/usr/include/xkbcommon/xkbcommon.h
merged() { systemd-sysext status --json=short | grep -q '"hierarchy":"/usr","extensions":\["nx"\]'; }

# From a clean start: an earlier run may have left it added.
nx list | grep -q '^  libxkbcommon-devel ' && nx remove libxkbcommon-devel >/dev/null
nx add libxkbcommon-devel
[[ -e $header ]] || fail "add: $header is not in /usr"
merged || fail "add: nx is not merged over /usr"
[[ $(stat -c %C "$header") == *:usr_t:* ]] || fail "add: $header is not usr_t"
[[ $(cat /var/lib/nx/backend) == sysext ]] || fail "add: backend is not sysext"
added=$(readlink /var/lib/nx/current)

nx remove libxkbcommon-devel
[[ ! -e $header ]] || fail "remove: $header is still in /usr"
removed=$(readlink /var/lib/nx/current)

nx rollback
[[ -e $header ]] || fail "rollback: $header is not back"
[[ $(readlink /var/lib/nx/current) == "$added" ]] || fail "rollback: current is $(readlink /var/lib/nx/current)"

# Someone who isn't allowed gets a refusal, and nothing changes.
before=$(readlink /var/lib/nx/current)
reply=$(runuser -u nobody -- python3 -c '
import socket
s = socket.socket(socket.AF_UNIX); s.connect("/run/nxd.sock")
s.sendall(b"{\"request\":\"activate\",\"generation\":1}\n"); print(s.recv(4096).decode())')
[[ $reply == *'"response":"failed"'* ]] || fail "nobody was not refused: $reply"
[[ $(readlink /var/lib/nx/current) == "$before" ]] || fail "a refused request moved current"

# The prefix backend: nothing merged, `nx shell` finds the headers.
runuser -l tester -c "python3 -c '
import socket
s = socket.socket(socket.AF_UNIX); s.connect(\"/run/nxd.sock\")
s.sendall(b\"{\\\"request\\\":\\\"activate\\\",\\\"generation\\\":${before##*/},\\\"backend\\\":\\\"prefix\\\"}\\n\"); s.recv(4096)'"
! merged || fail "prefix: nx is still merged"
[[ ! -e /var/lib/extensions/nx ]] || fail "prefix: the link is still there"
nx shell -- env | grep -q '^CPATH=/var/lib/nx/current/sysext/usr/include' || fail "prefix: nx shell does not export CPATH"
[[ -e /var/lib/nx/current/sysext/usr/include/xkbcommon/xkbcommon.h ]] || fail "prefix: no header in the prefix"

# Unforced, the next activation goes back to sysext.
nx rollback "${removed##*/}" >/dev/null
nx rollback "${before##*/}"
merged || fail "sysext is not back"
echo "m3: ok"
