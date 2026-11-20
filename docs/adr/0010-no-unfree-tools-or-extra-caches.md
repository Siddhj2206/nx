# No unfree tools or extra Nix caches

nx 1.0 installed an unfree nixpkgs package when the manifest's
`[tool] unfree` allowlist named it (`nx add --allow-unfree`), and trusted
extra binary caches from `[[cache]]` (`nx cache add`). Hydra builds nothing
unfree, so every unfree tool was a local build of a vendor download, and
an untrusted user's `extra-substituters` only work once the daemon lists
them in `trusted-substituters`, which it reads at start. So the two
features cost:

- nxd writing `/etc/nix/nx.conf` and `nix.conf`'s `!include` as root, and
  restarting `nix-daemon` on activation;
- their own manifest keys, a command (`nx cache`), a routing branch, an
  `allowUnfreePredicate` in the evaluation, and `--option
  extra-substituters` on every `nix` call;
- a pruning rule in `nx remove` and a lock field for "built because
  unfree".

Neptuno has one administrator, who used neither: the vendor tools worth
having publish an RPM repo or a Flatpak.

nx 1.1 refuses unfree packages, pointing to the vendor's RPM repo
(`nx repo add`, then `nx add --host`) or its Flatpak, and talks to
cache.nixos.org only. Building from a derivation stays: wrappers Hydra
doesn't build (`neovim`) need it.

## Considered

- **Keep extra caches, drop unfree.** Caches are the half that writes
  `/etc` as root and restarts the daemon; without unfree packages nothing
  nx installs needs one.
- **Keep both behind a flag.** The code and the root-side writes stay for
  a feature with no user.

## Consequences

- Before upgrading to 1.1, `nx remove` the unfree packages. A manifest
  that still has `[tool] unfree` or `[[cache]]` is read with a warning and
  kept as written, so a rollback to 1.0 still finds them.
- 1.1's nxd removes `/etc/nix/nx.conf` and the `!include` on activation and
  restarts the daemon once; 1.2 drops that cleanup.
- nxd no longer touches `/etc/nix` or `nix-daemon`, which the systemd
  hardening and the `nx`-user builder (plan 1.1, Phases 1 and 2) build on.
