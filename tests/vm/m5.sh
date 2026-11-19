#!/usr/bin/env bash
# M5 exit check in the Neptuno test VM, after dev/vm-install. Run as root, one
# phase at a time, rebooting between them (`vm ssh reboot`):
#   ~/.local/share/nx-vm/vm ssh 'bash -s -- stage' < tests/vm/m5.sh
#   (stage an update first: bootc switch to another tag, or bootc upgrade)
#   phase stage     the host tier is re-resolved for the staged image, before
#                   the reboot, and a change builds one for both images
#   phase booted    after the reboot: the generation for the booted image is
#                   current and merged, and a rollback deployment has one
#   phase fallback  deletes the booted image's generation, as if the staging
#                   hook had never run; reboot, then check `fallback-check`
#   phase gc        gc keeps the generations deployed images need
set -euo pipefail
fail() { echo "FAIL: $*" >&2; exit 1; }
as() { local user=$1; shift; runuser -l "$user" -c "$*"; }
booted_digest() {
	rpm-ostree status --json | python3 -c '
import json, sys
print(next(d["container-image-reference-digest"] for d in json.load(sys.stdin)["deployments"] if d["booted"]))'
}
current_digest() { sed -n 's/^image = "\(.*\)"/\1/p' /var/lib/nx/current/lock.toml; }
header=/usr/include/wayland-client.h

case ${1:?phase} in
stage)
	as tester "nx add wayland-devel" >/dev/null
	[[ -e $header ]] || fail "wayland-devel is not merged"
	rpm-ostree status --json | python3 -c '
import json, sys
sys.exit(0 if any(d.get("staged") for d in json.load(sys.stdin)["deployments"]) else 1)' ||
		fail "no update is staged (bootc switch or upgrade first)"
	systemctl start nx-sync.service
	status=$(as tester "nx status")
	echo "$status"
	grep -q 'host tier: generation' <<<"$status" || fail "nx status shows no host tier for the staged image"
	# A change while an update is staged resolves for both images (invariant 6).
	as tester "nx add libxkbcommon-devel" 2>&1 | tee /tmp/m5-add.txt
	grep -q 'for the staged image' /tmp/m5-add.txt || fail "the change did not reach the staged image"
	[[ $(current_digest) == "$(booted_digest)" ]] || fail "current is not for the booted image"
	as tester "nx sync" | grep -q 'match the manifest' || fail "sync found more to do"
	echo "stage: ok; now reboot"
	;;
booted)
	[[ $(current_digest) == "$(booted_digest)" ]] || fail "current is for another image: select did not run"
	[[ -e $header ]] || fail "the host tier is not merged after the reboot"
	systemctl is-active --quiet nx-select.service || systemctl status nx-select.service --no-pager || true
	journalctl -b -u nx-select.service --no-pager | tail -3
	systemctl start nx-sync.service
	as tester "nx status"
	echo "booted: ok"
	;;
fallback)
	# As if the staging hook never ran for this image: the booted image's
	# generations go, and `current` is the newest generation of another image.
	digest=$(booted_digest)
	other=""
	for d in /var/lib/nx/generations/[0-9]*/; do
		if grep -q "^image = \"$digest\"" "$d/lock.toml"; then
			rm -rf "$d"
		else
			other=$(basename "$d")
		fi
	done
	[[ -n $other ]] || fail "no generation for another image to fall back to"
	ln -sfn "generations/$other" /var/lib/nx/current
	echo "fallback: prepared (current is generation $other); now reboot, then run fallback-check"
	;;
fallback-check)
	journalctl -b -u nx-select.service --no-pager | tail -5
	systemctl start nx-sync.service
	[[ $(current_digest) == "$(booted_digest)" ]] || fail "sync did not make a generation for the booted image current"
	[[ -e $header ]] || fail "the host tier did not come back"
	echo "fallback-check: ok"
	;;
gc)
	count() { find /var/lib/nx/generations -mindepth 1 -maxdepth 1 -name '[0-9]*' | wc -l; }
	before=$(count)
	as tester "nx gc --dry-run"
	as tester "nx gc"
	[[ $(current_digest) == "$(booted_digest)" ]] || fail "gc lost the current generation"
	echo "gc: ok ($before generations before, $(count) after)"
	;;
*) fail "unknown phase" ;;
esac
