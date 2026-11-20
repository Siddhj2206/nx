# nx

A package manager for Fedora bootc systems.

On a bootc system the image is read-only: getting one `-devel` package means
editing the Containerfile, rebuilding the image, updating and rebooting.
Installing tools with Nix instead loads Nix's own glibc, Mesa and Wayland
into the same processes as the host's, and native builds break. nx installs
packages live, in seconds, and keeps the two apart:

- **Host tier.** Fedora RPMs resolved against the exact booted image
  (`-devel` packages, libraries, compilers, services) and overlaid onto
  `/usr` with systemd-sysext, so `pkg-config` and `cargo build` see them at
  once. They are resolved again for every new image.
- **Tool tier.** Standalone command-line tools from one pinned nixpkgs
  release, each with its own sealed libraries, run through small shims that
  never leak Nix's environment into host programs.

`nx add` picks the tier: a tool that needs nothing from the system comes
from nixpkgs, and everything that plugs into it comes from Fedora.

```
$ nx add --dry-run wayland-devel

  + libffi-devel   3.5.2-3.fc45   fedora  29 KiB
  + wayland-devel  1.26.0-3.fc45  fedora  161 KiB

  2 packages · 190 KiB download, 758 KiB installed · dry run, nothing changed

$ nx why rg

  ripgrep 15.2.0

  Asked       rg
  Tier        tool · nixpkgs, sealed beside the image
  Release     nixpkgs-26.11pre1124565.19cfded70b91
  Why         you added it · a standalone tool
  Generation  65 · added in 63
```

nx is built for [Neptuno](https://github.com/Siddhj2206/neptuno), which ships
it; any Fedora bootc image can install it (below).

## Commands

```
nx add <pkg>…        add packages, live
nx remove <pkg>…     remove packages and what only they needed
nx upgrade           newer host builds, and nixpkgs to its current release
nx list | search | info | why
nx generations       every generation and what it changed
nx rollback [N]      make an earlier generation current again
nx apply [file]      make the system match a manifest
nx repo              third-party RPM repos (COPR, .repo files)
nx status | doctor | sync | gc
```

`nx help` lists them all, and `nx <command> --help` explains one.

## How it works

Every change writes `manifest.toml` (what you asked for) and builds a
**generation**: the resolved packages for one image, with a `lock.toml`.
nxd, a small root daemon behind polkit, validates the generation and merges
it as a system extension. Nothing is ever written into the image.

A generation is built for every deployed image (booted, staged and
rollback), so a bootc update comes up with a host tier that matches it, and
at boot `nx select` merges the one built for the image that booted.
Timers refresh repo metadata daily and move the nixpkgs pin weekly;
failures reach the desktop as notifications.

RPM scriptlets aren't run: nx translates them into sysusers, tmpfiles and
presets, and refuses a package whose scriptlets it can't translate unless
you allow it after reading them.

The design, and why it is this way, is in [docs/design.md](docs/design.md)
and [docs/adr/](docs/adr/). [docs/product.md](docs/product.md) describes
the commands and their output.

## Installing

nx links the image's libsolv and librpm, so a release is one tarball per
Fedora release and architecture, `nx-v<version>-fc<release>-<arch>.tar.gz`,
with `SHA256SUMS`. It holds the tree an image unpacks at `/`. In a
Containerfile, check it, unpack it, and enable its units (the preset does
that): `nxd.socket`, `nx-select`, `nx-sync`,
`nx-refresh.timer`, `nx-upgrade.timer`, and the user units
`nx-notify.path` and `.service`. The tool tier needs Nix and its daemon in
the image.

nx 1.0 replaced nx 0.x, a front end to a Nix profile (tag `v0.2.0`).
nx 1.0.x's `nx migrate` moves a 0.x profile into the tool tier; 1.1
dropped it.

## Building

nx builds on the Fedora bootc system it runs on, against the image's own
libsolv and librpm:

```
nx add libsolv-devel rpm-devel
cargo build --release
cargo test
```

With [mise](https://mise.jdx.dev), `mise run check` runs every gate CI
runs and `mise run vm` installs a build into a test VM. Branches, releases and the changelog are in
[docs/development.md](docs/development.md); what changed in each release
is in [CHANGELOG.md](CHANGELOG.md).
