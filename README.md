# nx

Packages for a Fedora bootc system, live, without rebuilding the image or
rebooting.

> **Rebuild in progress.** This branch is nx 1.0, a Rust rewrite. The
> released nx is 0.x (tag `v0.2.0`), a Homebrew-style front end to a Nix
> profile. See [docs/adr/0006](docs/adr/0006-rust-rebuild-two-tiers.md).

nx sorts every package by whose libraries it loads:

- **Host tier:** Fedora RPMs for the exact booted image (`-devel` packages,
  libraries, compilers, services), overlaid onto `/usr` with systemd-sysext,
  so `pkg-config` and `cargo build` see them at once. Re-resolved
  automatically for every new image.
- **Tool tier:** standalone CLI tools from one pinned nixpkgs release, each
  with its own sealed libraries, exposed through small shims that never leak
  Nix's environment into host programs.

Working today: `nx status`, and for the host tier `nx search`, `nx info`
and `nx add --dry-run`, which resolve against the booted image with its own
libsolv.

```
$ nx add --dry-run libxkbcommon-devel wayland-devel

  + libxkbcommon-devel  1.13.1-4.fc45  fedora      74 KiB
  + wayland-devel       1.26.0-3.fc45  fedora      161 KiB

  2 packages · 235 KiB download, 1.1 MiB installed · dry run, nothing changed

$ nx status

  ghcr.io/siddhj2206/neptuno:latest 45.20261118
  Neptuno 45 · booted from sha256:92409d3fcf84

  Staged      none
  Rollback    45.20261117 from sha256:292ee1bfc6d4
  Host tier   resolves (nx add --dry-run); installs from M3
  Tool tier   not built yet (M6)
```

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
