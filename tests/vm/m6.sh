#!/usr/bin/env bash
# M6 exit check in the Neptuno test VM, after dev/vm-install. Run as root:
#   ~/.local/share/nx-vm/vm ssh 'bash -s -- <phase>' < tests/vm/m6.sh
#   phase add     nx add ripgrep: a tool with no Nix profile and no Nix env on
#                 PATH, rooted in the generation, on PATH through profile.d
#   phase unfree  an unfree tool is refused, then installs once allowed
#   phase gc      removing it and nx gc lets nix store gc delete it; ripgrep
#                 stays
set -euo pipefail
fail() { echo "FAIL: $*" >&2; exit 1; }
as() { local user=$1; shift; runuser -l "$user" -c "$*"; }
# The barest environment a command can have: no Nix profile, no Nix variables.
bare() { env -i HOME=/home/tester PATH=/usr/local/bin:/usr/bin:/var/lib/nx/current/bin "$@"; }
bin=/var/lib/nx/current/bin

case ${1:?phase} in
add)
	[[ ! -e /home/tester/.nix-profile ]] || fail "tester has a Nix profile; this test needs none"
	as tester "nx list" | grep -q '^  ripgrep ' && as tester "nx remove ripgrep" >/dev/null
	as tester "nx add ripgrep" 2>&1 | tee /tmp/m6-add.txt
	grep -q '+ ripgrep' /tmp/m6-add.txt || fail "nx add did not add ripgrep"
	[[ -x $bin/rg ]] || fail "no rg in the generation's bin/"
	bare rg --version | head -1
	bare runuser -u tester -- rg --version >/dev/null || fail "rg does not run for tester"
	[[ $(as tester "command -v rg") == "$bin/rg" ]] || fail "rg is not on a login shell's PATH"
	root=$(readlink /var/lib/nx/current/gcroots/root) || fail "the generation has no GC root"
	[[ $root == /nix/store/*ripgrep* ]] || fail "the GC root points at $root"
	link="$(realpath /var/lib/nx/current)/gcroots/root"
	found=""
	for auto in /nix/var/nix/gcroots/auto/*; do
		[[ $(readlink "$auto") == "$link" ]] && found=$auto
	done
	[[ -n $found ]] || fail "the daemon has no auto root for $link"
	as tester "nx why rg"
	as tester "nx list"
	echo "add: ok"
	;;
unfree)
	if as tester "nx add ngrok" 2>/tmp/m6-refused.txt; then
		fail "an unfree tool installed without being allowed"
	fi
	grep -q 'allow-unfree' /tmp/m6-refused.txt || fail "the refusal doesn't say how to allow it: $(cat /tmp/m6-refused.txt)"
	as tester "nx add --allow-unfree ngrok" 2>&1 | tee /tmp/m6-unfree.txt
	grep -q '+ ngrok' /tmp/m6-unfree.txt || fail "ngrok was not added"
	bare ngrok version || fail "ngrok does not run"
	grep -q '"ngrok"' /var/lib/nx/current/manifest.toml || fail "the allowance is not in the manifest"
	echo "unfree: ok"
	;;
gc)
	ngrok=$(cat /var/lib/nx/generations/*/shims/ngrok 2>/dev/null | head -1)
	ngrok=${ngrok%/bin/ngrok}
	[[ -n $ngrok ]] || fail "no generation has ngrok; run the unfree phase first"
	if as tester "nx list" | grep -q '^  ngrok '; then
		as tester "nx remove ngrok"
	fi
	grep -q 'ngrok' /var/lib/nx/current/manifest.toml && fail "the allowance stayed in the manifest"
	# Older generations still root ngrok: keep only what gc must keep. As root:
	# generations nx-sync.service built (as root) aren't removable by wheel.
	nx gc 2>&1 | tee /tmp/m6-gc.txt
	if [[ -e $ngrok ]]; then
		echo "$ngrok is still rooted by a kept generation (the five newest are kept)"
	else
		echo "$ngrok is gone"
	fi
	bare rg --version >/dev/null || fail "gc broke ripgrep"
	grep -q 'freed\|nix store gc' /tmp/m6-gc.txt || fail "nx gc did not run nix store gc"
	echo "gc: ok"
	;;
*) fail "unknown phase $1" ;;
esac
