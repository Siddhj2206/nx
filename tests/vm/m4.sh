#!/usr/bin/env bash
# M4 exit check in the Neptuno test VM, after dev/vm-install. Run as root:
#   ~/.local/share/nx-vm/vm ssh 'bash -s' < tests/vm/m4.sh
# `nx add syncthing` installs and its service runs; a package whose scriptlet
# nx can't translate is refused until it is allowed by digest; a second
# administrator reuses the shared metadata cache.
set -euo pipefail
fail() { echo "FAIL: $*" >&2; exit 1; }
as() { local user=$1; shift; runuser -l "$user" -c "$*"; }

id tester2 >/dev/null 2>&1 || useradd -m -G wheel tester2
setfacl -R -m g:wheel:rwX,d:g:wheel:rwX /var/lib/nx

# Services: install, run, remove.
as tester "nx add syncthing"
[[ -x /usr/bin/syncthing ]] || fail "syncthing was not installed"
systemctl cat syncthing@.service >/dev/null || fail "syncthing@.service is not visible"
systemctl start syncthing@tester || fail "the instance would not start"
sleep 2
systemctl is-active --quiet syncthing@tester || fail "the instance is not running"
as tester "nx remove syncthing" 2>&1 | tee /tmp/m4-remove.txt
systemctl is-active --quiet syncthing@tester && fail "remove left the instance running"
pgrep -x syncthing >/dev/null && fail "remove left a process behind"
grep -q "stopped and disabled" /tmp/m4-remove.txt || fail "remove did not say it stopped the service"

# An untranslatable scriptlet is refused, with a way forward. (An allowance a
# previous run left in the manifest would let it through.)
as tester "nx remove memcached" >/dev/null 2>&1 || true
sed -i '/^\[\[scriptlets\]\]/,/^$/d' /var/lib/nx/manifest.toml 2>/dev/null || true
if as tester "nx add memcached" >/tmp/m4-refuse.txt 2>&1; then
	fail "memcached was not refused"
fi
grep -q "dnf install" /tmp/m4-refuse.txt || fail "the refusal has no image hint"
grep -q -- "--allow-scriptlets" /tmp/m4-refuse.txt || fail "the refusal has no allow flag"
digest=$(grep -o -- '--allow-scriptlets [^ ]*' /tmp/m4-refuse.txt | head -1 | cut -d' ' -f2)
as tester "nx add memcached --allow-scriptlets $digest"
getent passwd memcached >/dev/null || fail "the memcached user was not created"
[[ -e /etc/sysconfig/memcached ]] || fail "/etc content was not placed"
as tester "nx remove memcached"

# The second administrator reuses the shared cache instead of downloading.
[[ -d /var/lib/nx/cache ]] || fail "no shared cache"
start=$(date +%s)
as tester2 "nx add syncthing"
(($(date +%s) - start < 20)) || fail "tester2 appears to have re-downloaded metadata"
as tester2 "nx remove syncthing"
echo "m4: ok"
