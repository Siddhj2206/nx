#!/usr/bin/env bash
# M6 spike: the tool tier's open questions, in the Neptuno test VM (never the
# daily driver). Run as root:
#   ~/.local/share/nx-vm/vm ssh 'bash -s -- <phase>' < spike/m6-tooltier.sh
#   phase join      how many catalog attributes map to exactly one store path
#                   when packages.json.br is joined with store-paths.xz
#   phase realise   nix-store --realise of a path from store-paths.xz, never
#                   evaluated here, with an indirect root and internal-json log
#   phase caches    whether the daemon picks up extra-substituters without a
#                   restart
#   phase env       run the most-used tools from a bare environment
set -euo pipefail
work=/var/tmp/m6
mkdir -p "$work"
cd "$work"
channel=https://channels.nixos.org/nixpkgs-unstable

fetch() {
	[[ -s release ]] && return
	url=$(curl -sSLo /dev/null -w '%{url_effective}' "$channel")
	echo "$url" >release
	curl -sSL "$url/git-revision" >rev
	time curl -sSLo packages.json.br "$url/packages.json.br"
	time curl -sSLo store-paths.xz "$url/store-paths.xz"
	ls -l packages.json.br store-paths.xz
}

join() {
	fetch
	echo "release: $(cat release)"
	time brotli -dc packages.json.br >packages.json
	time xz -dc store-paths.xz >store-paths
	wc -l store-paths
	python3 - <<'EOF'
import json, collections, re, time
t = time.time()
pkgs = json.load(open("packages.json"))["packages"]
print(f"catalog: {len(pkgs)} attributes, {time.time()-t:.1f}s to load")
# store path name -> paths; name is everything after "<hash>-"
by_name = collections.defaultdict(list)
for line in open("store-paths"):
    p = line.strip()
    by_name[p[44:]].append(p)
dup_names = sum(1 for v in by_name.values() if len(v) > 1)
print(f"store paths: {sum(map(len, by_name.values()))}, names with >1 path: {dup_names}")
stats = collections.Counter()
examples = collections.defaultdict(list)
# which attributes claim the same store path
claims = collections.defaultdict(list)
for attr, p in pkgs.items():
    meta = p.get("meta", {})
    if meta.get("unfree") or meta.get("broken") or meta.get("available") is False:
        stats["skipped (unfree/broken/unavailable)"] += 1
        continue
    name = p.get("name") or f'{p["pname"]}-{p["version"]}'
    outs = meta.get("outputsToInstall") or ["out"]
    found = []
    for o in outs:
        n = name if o == "out" else f"{name}-{o}"
        found.append(by_name.get(n, []))
    if all(len(f) == 1 for f in found):
        kind = "exact"
        for f in found:
            claims[f[0]].append(attr)
    elif any(len(f) == 0 for f in found):
        kind = "missing"
    else:
        kind = "ambiguous"
    stats[kind] += 1
    if len(examples[kind]) < 8:
        examples[kind].append((attr, name, outs, [len(f) for f in found]))
for k, v in stats.most_common():
    print(f"  {k}: {v}")
shared = {p: a for p, a in claims.items() if len(a) > 1}
print(f"store paths claimed by >1 attribute: {len(shared)} (aliases or the same derivation)")
for k, ex in examples.items():
    print(f"examples, {k}:")
    for e in ex:
        print("   ", e)
# The tools the design names, plus common ones.
for attr in ["ripgrep", "neovim", "jq", "fd", "bat", "zig", "git", "gh", "htop",
             "fzf", "eza", "rustup", "just", "helix", "lazygit", "starship",
             "tmux", "yq-go", "hyperfine", "tokei", "rust-analyzer", "nil"]:
    p = pkgs.get(attr)
    if not p:
        print(f"  {attr}: not in catalog"); continue
    name = p.get("name") or f'{p["pname"]}-{p["version"]}'
    outs = p["meta"].get("outputsToInstall") or ["out"]
    r = [by_name.get(name if o == "out" else f"{name}-{o}", []) for o in outs]
    print(f"  {attr}: {name} {outs} -> {[len(x) for x in r]} {r[0][:1]}")
EOF
}

realise() {
	fetch
	# A path no eval has touched: hyperfine's x86_64-linux build, told apart
	# from the other platforms' by the glibc its narinfo references.
	glibc=$(nix eval --raw --file "$(cat release)/nixexprs.tar.xz" glibc.outPath)
	path=
	while read -r p; do
		h=${p#/nix/store/}
		if curl -sS "https://cache.nixos.org/${h%%-*}.narinfo" | grep -q "^References:.*${glibc#/nix/store/}"; then
			path=$p
		fi
	done < <(xz -dc store-paths.xz | grep -E -- '-hyperfine-[0-9.]+$')
	echo "glibc: $glibc path: $path"
	# Fedora's nix ships only the new CLI: `nix build <store path>` substitutes
	# it, and --out-link registers an indirect root, as nix-store --add-root
	# --indirect did. As the unprivileged wheel user nx runs as.
	roots=/var/tmp/m6-roots
	install -d -o tester "$roots"
	rm -f "$roots/hyperfine"
	time runuser -u tester -- nix build "$path" --out-link "$roots/hyperfine" \
		--log-format internal-json -v 2>realise.log
	readlink "$roots/hyperfine"
	find /nix/var/nix/gcroots/auto -lname "$roots/hyperfine" -printf '%p -> %l\n'
	"$roots/hyperfine/bin/hyperfine" --version | head -1
	nix path-info --json "$path" | head -c 400; echo
	echo "log lines: $(grep -c '^@nix' realise.log)"
	grep -o '"type":[0-9]*' realise.log | sort | uniq -c
	head -c 1500 realise.log; echo
	# Realised twice: nothing to do.
	time runuser -u tester -- nix build "$path" --out-link "$roots/hyperfine" --log-format internal-json 2>&1 | wc -l
	# A path no cache has: what the error looks like.
	fake=/nix/store/00000000000000000000000000000000-nothing-1.0
	runuser -u tester -- nix build "$fake" --no-link --log-format internal-json 2>&1 | tail -3 || true
	# A wrapper Hydra doesn't build (neovim): its drv from eval, built locally.
	drv=$(runuser -u tester -- nix eval --raw --file "$(cat release)/nixexprs.tar.xz" neovim.drvPath)
	rm -f "$roots/neovim"
	time runuser -u tester -- nix build "$drv^out" --out-link "$roots/neovim" --log-format internal-json -v 2>neovim.log
	"$roots/neovim/bin/nvim" --version | head -1
	grep -o '"type":[0-9]*' neovim.log | sort | uniq -c
}

caches() {
	nix config show | grep -E '^(substituters|trusted-public-keys|trusted-substituters|trusted-users) '
	url=https://nix-community.cachix.org
	key='nix-community.cachix.org-1:mB9FSh9qf2dCimDSUo8Zy7bkq5CX+/rkCWyvRCYg3Fs='
	fake=/nix/store/00000000000000000000000000000000-nothing-1.0
	try() {
		runuser -u tester -- nix build --no-link "$fake" --option extra-substituters "$url" \
			--option extra-trusted-public-keys "$key" 2>&1 | grep -iE "untrusted|ignor|substituter" | head -2 || echo "  (accepted: no warning)"
	}
	echo "--- before: an untrusted user's extra substituter"
	try
	cp /etc/nix/nix.conf nix.conf.bak
	printf 'extra-trusted-substituters = %s\nextra-trusted-public-keys = %s\n' "$url" "$key" >>/etc/nix/nix.conf
	echo "--- nix.conf changed, daemon not restarted"
	try
	echo "--- after systemctl restart nix-daemon"
	time systemctl restart nix-daemon
	try
	# A running build survives a restart? Start one, restart, see.
	cp nix.conf.bak /etc/nix/nix.conf
	systemctl restart nix-daemon
}

env_check() {
	fetch
	tar=$(cat release)/nixexprs.tar.xz
	bin() { runuser -u tester -- nix eval --raw --file "$tar" "$1" --apply 'p: (p.bin or p).outPath'; }
	declare -A paths
	for t in ripgrep:rg fd:fd bat:bat jq:jq fzf:fzf eza:eza git:git gh:gh htop:htop just:just \
		helix:hx lazygit:lazygit starship:starship tmux:tmux hyperfine:hyperfine tokei:tokei \
		zig:zig yq-go:yq curl:curl less:less; do
		attr=${t%%:*} exe=${t#*:}
		path=$(bin "$attr")
		paths[$attr]=$path
		runuser -u tester -- nix build --no-link "$path" 2>/dev/null || { echo "$attr: realise failed"; continue; }
		# A bare environment, as a shim would start it, as the tester user.
		out=$(cd /tmp && runuser -u tester -- env -i HOME=/home/tester TERM=xterm-256color LANG=en_US.UTF-8 \
			PATH=/usr/bin "$path/bin/$exe" --version 2>&1 | head -2 | tr '\n' ' ' || true)
		echo "$attr: $out"
	done
	git=${paths[git]}/bin/git tmux=${paths[tmux]}/bin/tmux less=${paths[less]}/bin/less
	run() { runuser -u tester -- env -i HOME=/home/tester PATH=/usr/bin "$@" 2>&1 | head -2 || true; }
	echo "--- certificates: git ls-remote over https, without and with SSL_CERT_FILE"
	run "$git" ls-remote https://github.com/NixOS/nixpkgs HEAD
	run SSL_CERT_FILE=/etc/pki/tls/certs/ca-bundle.crt "$git" ls-remote https://github.com/NixOS/nixpkgs HEAD
	echo "--- locale: a UTF-8 locale without and with LOCALE_ARCHIVE"
	run LANG=de_DE.UTF-8 "$git" status
	run LANG=de_DE.UTF-8 LOCALE_ARCHIVE=/usr/lib/locale/locale-archive "$git" status
	glibc_locales=$(runuser -u tester -- nix eval --raw --file "$tar" glibcLocales.outPath)
	echo "glibcLocales: $glibc_locales"
	echo "--- terminfo: tmux and less on a terminal only the host knows"
	find /usr/share/terminfo/x -name 'xterm-ghostty' -o -name 'xterm-kitty'
	run TERM=xterm-kitty "$less" --version | head -1
	run TERM=xterm-kitty "$tmux" -L m6 new -d true
	run TERM=xterm-kitty TERMINFO_DIRS=/usr/share/terminfo "$tmux" -L m6 new -d true
	echo "--- NSS: whoami-style lookup with a Nix glibc"
	run "${paths[git]}/bin/git" var GIT_COMMITTER_IDENT
}

case ${1:?phase} in
join) join ;;
realise) realise ;;
caches) caches ;;
env) env_check ;;
*) echo "unknown phase $1" >&2; exit 2 ;;
esac
