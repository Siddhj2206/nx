#!/usr/bin/env bash
# M7 checks in the Neptuno test VM, after dev/vm-install. Run as root:
#   ~/.local/share/nx-vm/vm ssh 'bash -s -- <phase>' < tests/vm/m7.sh
#   phase doctor    nx doctor passes on a set-up system and flags a leak
#   phase apply     nx apply adds what a manifest file has, and takes it
#                   back with the old one
#   phase migrate   a package in tester's Nix profile (and a registry pin)
#                   moves into nx; the profile and the pin are gone
#   phase notify    a recorded failure reaches the session once
#   phase upgrade   nx-upgrade.service runs and leaves no failure behind;
#                   nx-refresh.service collects the Nix store
#   phase hint      a package with kernel modules gets the image hint, and
#                   nothing it needs is downloaded first
#   phase race      two nx add at once both end up in the manifest
#   phase owner     what is merged into /usr is root's copy, which tester
#                   can't write
#   phase keys      a repo's pinned key survives nx repo remove, so a
#                   rollback to the repo can still verify its packages
#   phase complete  the shell's completion asks nx for live values
set -euo pipefail
fail() { echo "FAIL: $*" >&2; exit 1; }
as() { local user=$1; shift; runuser -l "$user" -c "$*"; }

case ${1:?phase} in
doctor)
	as tester "nx doctor" | tee /tmp/m7-doctor.txt || fail "nx doctor found a problem"
	grep -q 'nxd is listening' /tmp/m7-doctor.txt || fail "doctor didn't check nxd"
	grep -q 'tools are on PATH' /tmp/m7-doctor.txt || fail "doctor didn't check PATH"
	if as tester "LD_LIBRARY_PATH=/nix/store/x/lib nx doctor" >/tmp/m7-leak.txt 2>&1; then
		fail "a leaked LD_LIBRARY_PATH passed"
	fi
	grep -q 'LD_LIBRARY_PATH points into Nix' /tmp/m7-leak.txt || fail "no leak finding: $(cat /tmp/m7-leak.txt)"
	echo "doctor: ok"
	;;
apply)
	before=/tmp/m7-before.toml
	cp /var/lib/nx/current/manifest.toml "$before"
	chmod 644 "$before"
	as tester "nx apply" 2>&1 | tee /tmp/m7-same.txt
	grep -q 'matches the manifest' /tmp/m7-same.txt || fail "an unchanged manifest built a generation"
	extra=libyaml-devel
	! grep -q "\"$extra\"" "$before" || fail "$extra is in the manifest already"
	! rpm -q "$extra" >/dev/null || fail "the image has $extra; pick another package"
	python3 - "$before" "$extra" >/tmp/m7-after.toml <<'EOF'
import sys, tomllib, re
text = open(sys.argv[1]).read()
pkgs = tomllib.loads(text).get("host", {}).get("packages", [])
new = repr(pkgs + [sys.argv[2]]).replace("'", '"')
if re.search(r"(?m)^\[host\]", text) and "packages" in text.split("[host]", 1)[1].split("\n[", 1)[0]:
    head, rest = text.split("[host]", 1)
    body, sep, tail = rest.partition("\n[")
    body = re.sub(r"(?ms)^packages = \[.*?\]", "packages = " + new, body)
    text = head + "[host]" + body + sep + tail
else:
    text += "\n[host]\npackages = " + new + "\n"
print(text)
EOF
	chmod 644 /tmp/m7-after.toml
	as tester "nx apply /tmp/m7-after.toml" 2>&1 | tee /tmp/m7-apply.txt
	grep -q "+ $extra" /tmp/m7-apply.txt || fail "nx apply didn't add $extra"
	[[ -e /usr/include/yaml.h ]] || fail "$extra isn't merged"
	as tester "nx apply $before" 2>&1 | tee /tmp/m7-back.txt
	grep -q -- "- $extra" /tmp/m7-back.txt || fail "applying the old manifest didn't remove $extra"
	echo "apply: ok"
	;;
migrate)
	as tester "nx list" | grep -q '^  hello ' && as tester "nx remove hello" >/dev/null
	as tester "nix profile add nixpkgs#hello"
	as tester "nix registry add nixpkgs github:NixOS/nixpkgs/nixos-unstable"
	as tester "nx migrate --dry-run" | tee /tmp/m7-dry.txt
	grep -q '+ hello' /tmp/m7-dry.txt || fail "the dry run doesn't list hello"
	as tester "nx migrate" 2>&1 | tee /tmp/m7-migrate.txt
	as tester "nx list" | grep -q '^  hello ' || fail "hello isn't in nx"
	[[ -z $(as tester "nix profile list --json" | python3 -c 'import json,sys; print(" ".join(json.load(sys.stdin)["elements"]))') ]] ||
		fail "the Nix profile still has packages"
	! as tester "nix registry list" | grep -q '^user .*flake:nixpkgs' || fail "the registry pin is still there"
	as tester "nx doctor" | grep -q "nx 0.x" && fail "doctor still sees nx 0.x"
	[[ $(as tester "command -v hello") == /var/lib/nx/current/bin/hello ]] || fail "hello isn't nx's"
	# Leave tester without a profile again, as m6 expects.
	as tester "rm -rf ~/.nix-profile ~/.local/state/nix/profiles"
	echo "migrate: ok"
	;;
notify)
	fake=/tmp/m7-fakebin
	mkdir -p "$fake"
	cat >"$fake/busctl" <<'EOF'
#!/bin/sh
# As busctl: the expiry (-1) must follow a --.
[ "$1 $2 $3" = "--user -- call" ] || { echo "busctl: bad arguments: $*" >&2; exit 1; }
echo "$@" >>/tmp/m7-notified.log
EOF
	chmod 755 "$fake/busctl"
	rm -f /tmp/m7-notified.log /home/tester/.local/state/nx/notified
	printf '{"tool":"m7 test failure"}' >/var/lib/nx/upgrade-failure.json
	as tester "PATH=$fake:\$PATH nx notify"
	grep -q "nx couldn't upgrade your tools" /tmp/m7-notified.log || fail "nothing was sent: $(cat /tmp/m7-notified.log 2>&1)"
	as tester "PATH=$fake:\$PATH nx notify"
	[[ $(wc -l </tmp/m7-notified.log) == 1 ]] || fail "the same note went out twice"
	rm /var/lib/nx/upgrade-failure.json
	systemd-analyze verify /etc/systemd/user/nx-notify.path /etc/systemd/user/nx-notify.service ||
		fail "the user units don't verify"
	echo "notify: ok"
	;;
upgrade)
	systemd-analyze verify /etc/systemd/system/nx-upgrade.timer /etc/systemd/system/nx-upgrade.service ||
		fail "the units don't verify"
	systemctl start nx-upgrade.service || fail "nx-upgrade.service failed: $(journalctl -u nx-upgrade -n 20 --no-pager)"
	[[ ! -e /var/lib/nx/upgrade-failure.json ]] || fail "a successful upgrade left a failure record"
	[[ $(systemctl is-enabled nx-upgrade.timer) == enabled ]] || fail "the timer isn't enabled"
	# What an upgrade leaves behind goes with the daily refresh.
	since=$(date '+%Y-%m-%d %H:%M:%S')
	systemctl start nx-refresh.service || fail "nx-refresh.service failed: $(journalctl -u nx-refresh -n 20 --no-pager)"
	journalctl -u nx-refresh --since "$since" --no-pager | grep -q 'store paths deleted' ||
		fail "the daily refresh didn't collect the Nix store"
	echo "upgrade: ok"
	;;
hint)
	rpm -q kernel-modules-internal >/dev/null && fail "the image has kernel-modules-internal; pick another package"
	if as tester "nx add kernel-modules-internal" 2>/tmp/m7-hint.txt; then
		fail "a package with kernel modules was added"
	fi
	grep -q 'ships kernel modules' /tmp/m7-hint.txt || fail "no image hint: $(cat /tmp/m7-hint.txt)"
	grep -q 'RUN dnf install -y kernel-modules-internal' /tmp/m7-hint.txt || fail "no Containerfile line"
	if as tester "nx add usbguard-selinux" 2>/tmp/m7-hint2.txt; then
		fail "an SELinux policy module was added"
	fi
	grep -q 'SELinux policy module' /tmp/m7-hint2.txt || fail "no image hint: $(cat /tmp/m7-hint2.txt)"
	# Refused before its dependencies (usbguard itself) are downloaded.
	! grep '↓' /tmp/m7-hint.txt /tmp/m7-hint2.txt | grep -v -e ' metadata' -e 'kernel-modules-internal' -e 'usbguard-selinux' ||
		fail "dependencies were downloaded before the hint"
	echo "hint: ok"
	;;
race)
	# Two changes at once: the second waits on the lock and must build on
	# the first's generation, not on the one before it.
	picked=()
	for p in libyaml-devel jansson-devel lz4-devel libsodium-devel; do
		rpm -q "$p" >/dev/null || grep -q "\"$p\"" /var/lib/nx/current/manifest.toml || picked+=("$p")
	done
	(( ${#picked[@]} >= 2 )) || fail "no two packages to add"
	a=${picked[0]} b=${picked[1]}
	as tester "nx add $a" >/tmp/m7-race-a.txt 2>&1 &
	pa=$!
	as tester "nx add $b" >/tmp/m7-race-b.txt 2>&1 &
	pb=$!
	wait "$pa" || fail "nx add $a failed: $(cat /tmp/m7-race-a.txt)"
	wait "$pb" || fail "nx add $b failed: $(cat /tmp/m7-race-b.txt)"
	for p in "$a" "$b"; do
		grep -q "\"$p\"" /var/lib/nx/current/manifest.toml || fail "$p was lost from the manifest"
	done
	as tester "nx remove $a $b" >/dev/null
	echo "race: ok"
	;;
owner)
	# What appears in /usr is root's copy, not the generation tester built.
	p=libyaml-devel
	as tester "nx add $p" >/dev/null
	link=$(readlink /var/lib/extensions/nx)
	[[ $link == /var/lib/nx-merged/* ]] || fail "the link names $link, not root's copy"
	merged=/usr/include/yaml.h
	built=$(readlink -f /var/lib/nx/current)/sysext$merged
	[[ $(stat -c %u "$merged") == 0 && $(stat -c %u "$built") != 0 ]] || fail "owners: $(stat -c '%U %n' "$merged" "$built")"
	[[ $(stat -c %i "$merged") != $(stat -c %i "$built") ]] || fail "$merged is the generation's own file"
	as tester "test -w $merged" && fail "tester can write $merged"
	as tester "test -w $(dirname "$merged")" && fail "tester can write $(dirname "$merged")"
	[[ $(stat -c %u /var/lib/nx-merged) == 0 ]] || fail "/var/lib/nx-merged isn't root's"
	(( $(find /var/lib/nx-merged -mindepth 1 -maxdepth 1 | wc -l) == 1 )) || fail "old copies were left: $(ls /var/lib/nx-merged)"
	as tester "nx remove $p" >/dev/null
	echo "owner: ok"
	;;
keys)
	# A repo's key is pinned by content and outlives nx repo remove, so a
	# rollback to a generation with the repo can still verify its packages.
	repo=copr:lilay/topgrade
	grep -q "\"$repo\"" /var/lib/nx/current/manifest.toml || as tester "nx repo add $repo" >/dev/null
	sha=$(python3 -c 'import sys,tomllib; print(next(r["key"] for r in tomllib.load(open("/var/lib/nx/current/lock.toml","rb"))["repo"] if r["id"]=="copr:lilay:topgrade"))')
	[[ -f /var/lib/nx/keys/$sha.asc ]] || fail "the lock names $sha, which isn't pinned"
	as tester "nx repo remove $repo" >/dev/null
	[[ -f /var/lib/nx/keys/$sha.asc ]] || fail "nx repo remove took the key an earlier generation names"
	as tester "nx rollback" >/dev/null
	grep -q "\"$repo\"" /var/lib/nx/current/manifest.toml || fail "the rollback didn't bring the repo back"
	as tester "nx add libyaml-devel" >/tmp/m7-keys.txt 2>&1 || fail "a change after the rollback failed: $(cat /tmp/m7-keys.txt)"
	as tester "nx remove libyaml-devel" >/dev/null
	as tester "nx repo remove $repo" >/dev/null
	echo "keys: ok"
	;;
complete)
	installed=$(as tester "nx list" | awk 'NR>0 && /^  [^ ]/ {print $1; exit}')
	[[ -n $installed ]] || fail "nothing installed to complete"
	as tester "COMPLETE=fish nx -- nx remove ''" | grep -q "^$installed" || fail "nx remove doesn't complete $installed"
	as tester "COMPLETE=fish nx -- nx do" | grep -q '^doctor' || fail "commands don't complete"
	echo "complete: ok"
	;;
*) fail "unknown phase $1" ;;
esac
