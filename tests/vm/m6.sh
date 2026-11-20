#!/usr/bin/env bash
# M6 exit check in the Neptuno test VM, after dev/vm-install. Run as root:
#   ~/.local/share/nx-vm/vm ssh 'bash -s -- <phase>' < tests/vm/m6.sh
#   phase add     nx add ripgrep: a tool with no Nix profile and no Nix env on
#                 PATH, rooted in the generation, on PATH through profile.d
#   phase unfree  an unfree tool is refused, pointing to a vendor repo or
#                 Flatpak
#   phase build   neovim, a wrapper no cache has, builds from its derivation
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
	# One root per tool package (root, root-1, ...); another tool may come
	# first in the manifest.
	link=""
	for r in /var/lib/nx/current/gcroots/root*; do
		[[ $(readlink "$r") == /nix/store/*ripgrep* ]] && link="$(realpath /var/lib/nx/current)/gcroots/${r##*/}"
	done
	[[ -n $link ]] || fail "no GC root points at ripgrep: $(ls -l /var/lib/nx/current/gcroots)"
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
		fail "an unfree tool installed"
	fi
	grep -q 'Flatpak' /tmp/m6-refused.txt || fail "the refusal doesn't say where to get it: $(cat /tmp/m6-refused.txt)"
	grep -q 'ngrok' /var/lib/nx/current/manifest.toml && fail "the refused package is in the manifest"
	echo "unfree: ok"
	;;
build)
	as tester "nx list" | grep -q '^  neovim ' && as tester "nx remove neovim" >/dev/null
	as tester "nx add neovim" 2>&1 | tee /tmp/m6-build.txt
	grep -q 'building' /tmp/m6-build.txt || fail "neovim was not built from its derivation"
	grep -q '+ neovim' /tmp/m6-build.txt || fail "neovim was not added"
	bare nvim --version | head -1 || fail "nvim does not run"
	echo "build: ok"
	;;
gc)
	nvim=""
	for shim in /var/lib/nx/generations/*/shims/nvim; do
		[[ -e $shim ]] && nvim=$(<"$shim") && break
	done
	nvim=${nvim%/bin/nvim}
	[[ -n $nvim ]] || fail "no generation has neovim; run the build phase first"
	if as tester "nx list" | grep -q '^  neovim '; then
		as tester "nx remove neovim"
	fi
	# A wheel user's gc: generations nx-sync.service built as root can't be
	# removed by tester, so gc sets them aside for root's next gc.
	as tester "nx gc" 2>&1 | tee /tmp/m6-gc.txt
	for g in /var/lib/nx/generations/[0-9]*; do
		[[ -e $g/lock.toml ]] || fail "$g is left without its lockfile"
	done
	if [[ -e $nvim ]]; then
		grep -lq "$nvim" /var/lib/nx/generations/[0-9]*/lock.toml ||
			fail "$nvim survived nix store gc with no generation using it"
		echo "$nvim is still used by a kept generation (gc.keep, 5 by default)"
	else
		echo "$nvim is gone"
	fi
	nx gc >/dev/null
	if compgen -G '/var/lib/nx/generations/.trash-*' >/dev/null ||
		compgen -G '/var/lib/nx/store/.trash-*' >/dev/null; then
		fail "root's nx gc left what tester's set aside"
	fi
	bare rg --version >/dev/null || fail "gc broke ripgrep"
	grep -q 'freed\|nix store gc' /tmp/m6-gc.txt || fail "nx gc did not run nix store gc"
	echo "gc: ok"
	;;
*) fail "unknown phase $1" ;;
esac
