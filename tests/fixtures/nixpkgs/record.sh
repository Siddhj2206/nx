#!/usr/bin/env bash
# Records what the tool tier fetches to add one package, `hello`, at the
# nixpkgs channel's current release, for session_tests:
#   git-revision, git-revision.url  the channel's answer, and where its
#                                   redirect ended (the release directory)
#   packages.json.br                the release's package list, cut to
#                                   hello, jq and ripgrep, compressed again
#   eval.json                       nx's evaluation (eval::expression) of
#                                   hello, as `nix eval` answers it
#   narinfo/<hash>.narinfo          cache.nixos.org's narinfo for every
#                                   path in hello's closure
# Runs in the nixos/nix image, so the host's Nix store is untouched.
#   tests/fixtures/nixpkgs/record.sh
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
rm -rf "$here/narinfo"
podman run --rm -i --security-opt label=disable -v "$here:/out" docker.io/nixos/nix:2.35.4 \
	bash -euo pipefail -s <<'EOF'
cd /out
at=$(curl -fsSL -o git-revision -w '%{url_effective}' https://channels.nixos.org/nixpkgs-unstable/git-revision)
printf '%s\n' "$at" >git-revision.url
release=${at%/git-revision}
nixexprs=$release/nixexprs.tar.xz
# A profile of its own: installing into the default one drops its curl.
nix-env -p /tmp/tools -f "$nixexprs" -iA jq brotli >/dev/null 2>&1
PATH=/tmp/tools/bin:$PATH

curl -fsSL "$release/packages.json.br" | brotli -d |
	jq -c '{version, packages: (.packages | with_entries(select(.key == "hello" or .key == "jq" or .key == "ripgrep")))}' |
	brotli -c >packages.json.br

# eval::expression for one attribute; the answer has the same shape.
nix --extra-experimental-features nix-command eval --json --file "$nixexprs" --apply '
f: let
  pkgs = f { system = "x86_64-linux"; overlays = [ ]; config = { }; };
  lib = pkgs.lib;
  p = pkgs.hello;
in {
  hello = {
    name = lib.getName p;
    version = p.version or "";
    drv = p.drvPath;
    outputs = map (o: { name = o; path = p.${o}.outPath; }) (p.meta.outputsToInstall or [ "out" ]);
  };
}' >eval.json

mkdir -p narinfo
mapfile -t queue < <(jq -r '.hello.outputs[].path' eval.json)
declare -A seen
while ((${#queue[@]})); do
	path=${queue[0]}
	queue=("${queue[@]:1}")
	base=${path#/nix/store/}
	hash=${base%%-*}
	[[ -n ${seen[$hash]:-} ]] && continue
	seen[$hash]=1
	curl -fsSL "https://cache.nixos.org/$hash.narinfo" -o "narinfo/$hash.narinfo"
	# The image has no sed.
	while read -r key refs; do
		if [[ $key == References: ]]; then
			for ref in $refs; do queue+=("/nix/store/$ref"); done
		fi
	done <"narinfo/$hash.narinfo"
done
EOF
