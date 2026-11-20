#!/usr/bin/env bash
# Two administrators and one ordinary user in the Neptuno test VM, after
# dev/vm-install and tests/vm/m3.sh. Run as root:
#   ~/.local/share/nx-vm/vm ssh 'bash -s' < tests/vm/multiuser.sh
# `tester` (wheel) adds a package; `tester2` (wheel) then removes and adds it
# back, so nxd activates generations whose store files `tester` wrote; and
# `tester3` (not in wheel) can read but not change anything.
set -euo pipefail
fail() { echo "FAIL: $*" >&2; exit 1; }
as() { local user=$1; shift; runuser -l "$user" -c "$*"; }
header=/usr/include/xkbcommon/xkbcommon.h

id tester2 >/dev/null 2>&1 || useradd -m -G wheel tester2
id tester3 >/dev/null 2>&1 || useradd -m tester3
# This VM's state predates the ACLs systemd/nx.tmpfiles gives a fresh install.
setfacl -R -m g:wheel:rwX,d:g:wheel:rwX /var/lib/nx

as tester "nx add libxkbcommon-devel"
[[ -e $header ]] || fail "tester: no header"

as tester2 "nx remove libxkbcommon-devel"
[[ ! -e $header ]] || fail "tester2: remove left the header"
as tester2 "nx add libxkbcommon-devel"
[[ -e $header ]] || fail "tester2: add did not bring the header back"
as tester2 "nx rollback"
[[ ! -e $header ]] || fail "tester2: rollback did not undo the add"
as tester2 "nx rollback"
[[ -e $header ]] || fail "tester2: the second rollback did not come back"

# An ordinary user reads, and is told why they can't change anything.
as tester3 "nx list" >/dev/null || fail "tester3: nx list"
as tester3 "nx generations" >/dev/null || fail "tester3: nx generations"
as tester3 "nx shell -- true" || fail "tester3: nx shell"
out=$(as tester3 "nx add libxkbcommon-devel" 2>&1) && fail "tester3: add worked"
[[ $out == *"needs an administrator"* ]] || fail "tester3: unhelpful error: $out"
out=$(as tester3 "nx rollback" 2>&1) && fail "tester3: rollback worked"
[[ $out == *"needs an administrator"* ]] || fail "tester3: unhelpful error: $out"
# ...and nxd refuses them even if they talk to it directly.
reply=$(runuser -u tester3 -- python3 -c '
import socket
s = socket.socket(socket.AF_UNIX); s.connect("/run/nxd.sock")
s.sendall(b"{\"request\":\"activate\",\"generation\":1}\n"); print(s.recv(4096).decode())')
[[ $reply == *'"response":"failed"'* ]] || fail "tester3: nxd did not refuse: $reply"
echo "multiuser: ok"
