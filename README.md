# nx

Packages for a Fedora bootc system, live, without rebuilding the image or
rebooting.

nx 1.0 is a Rust rebuild of nx 0.x (tag `v0.2.0`, a front end to a Nix
profile); `nx migrate` moves a 0.x profile into it. See
[docs/adr/0006](docs/adr/0006-rust-rebuild-two-tiers.md).

nx sorts every package by whose libraries it loads:

- **Host tier:** Fedora RPMs for the exact booted image (`-devel` packages,
  libraries, compilers, services), overlaid onto `/usr` with systemd-sysext,
  so `pkg-config` and `cargo build` see them at once. Re-resolved
  automatically for every new image.
- **Tool tier:** standalone CLI tools from one pinned nixpkgs release, each
  with its own sealed libraries, exposed through small shims that never leak
  Nix's environment into host programs.

```
nx add <pkg>…        live, in seconds: tools from nixpkgs, the rest as Fedora RPMs
nx remove <pkg>…     and what only they needed
nx upgrade           newer host builds and the current nixpkgs release
nx apply [file]      make the machine match a manifest
nx list | why | search | info | status | doctor
nx rollback [N]      an earlier generation, live again
nx repo | cache      third-party RPM repos, extra Nix caches
nx migrate           nx 0.x's Nix profile into nx
```

Every change builds a generation for every deployed image (booted, staged,
rollback); at boot `nx select` merges the one built for the image that
booted. Timers refresh metadata daily (and collect garbage) and move the
nixpkgs pin weekly; failures reach the desktop as notifications.

## Installing

nx links the image's libsolv and librpm, so a release is one tarball per
Fedora release and architecture, the tree an image unpacks at `/`:
`nx-v<version>-fc<release>-<arch>.tar.gz`, with `SHA256SUMS`. In a
Containerfile, unpack it, check it, and enable its units (the preset does,
or list them: `nxd.socket`, `nx-select`, `nx-settle`, `nx-sync`,
`nx-refresh.timer`, `nx-upgrade.timer`, and the user units
`nx-notify.path` and `.service`). The tool tier needs the image's Nix and
its daemon. Neptuno does all of this.

The design, milestones and open questions are in
[docs/design.md](docs/design.md).

## Building

nx links the image's libsolv, so it builds in a Fedora container of the
image's release (`dev/Containerfile`); `dev/run` runs a command in it with
this checkout mounted:

```
dev/run cargo build --release   # in the container's /target/release/nx
dev/run cargo test
```

With [mise](https://mise.jdx.dev), `mise run check` runs every gate CI
does and `mise tasks` lists the rest. Branches, the changelog and releases
are in [docs/development.md](docs/development.md); what changed in each
release is in [CHANGELOG.md](CHANGELOG.md).
