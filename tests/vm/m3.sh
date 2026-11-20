#!/usr/bin/env bash
# M3's checks in the Neptuno test VM, after dev/vm-install. Run as root:
#   ~/.local/share/nx-vm/vm ssh 'bash -s' < tests/vm/m3.sh
# Adds, removes and rolls back a -devel package as `tester` and checks, each
# time, that /usr shows it with no reboot.
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
runuser -l tester -c "nx doctor" | grep -q 'merged over /usr' || fail "add: nx doctor doesn't see the merge"
added=$(readlink /var/lib/nx/current)

nx remove libxkbcommon-devel
[[ ! -e $header ]] || fail "remove: $header is still in /usr"

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

# nx 1.0's request with a backend still activates, merged.
runuser -l tester -c "python3 -c '
import socket
s = socket.socket(socket.AF_UNIX); s.connect(\"/run/nxd.sock\")
s.sendall(b\"{\\\"request\\\":\\\"activate\\\",\\\"generation\\\":${before##*/},\\\"backend\\\":\\\"prefix\\\"}\\n\"); s.recv(4096)'"
merged || fail "a request naming a backend didn't merge"
echo "m3: ok"
