# nx 1.0 is a Rust rebuild around a host tier and a tool tier

nx 0.x put every package into a Nix profile. That works for standalone tools,
but compilers and libraries from Nix mix their glibc, Mesa and Wayland with
the host's, and the only clean way to get a `-devel` RPM on a bootc image was
a ten-minute image rebuild and a reboot. nx 1.0 sorts packages into two tiers
by whose libraries they load: a host tier of Fedora RPMs for the booted
image, overlaid live onto `/usr` with a systemd-sysext extension, and a tool
tier of sealed nixpkgs closures exposed through shims.
[docs/design.md](../design.md) is the design.

The rebuild is in Rust, in this repository, and starts from an empty tree
instead of a port: of nx 0.x only the catalog builder, the narinfo walker and
the internal-json progress survive, and those land in M6. Until then the Go
nx 0.x stays the release Neptuno installs (tag `v0.2.0`).

## Changes from the "bx" draft

The design was drafted as a separate tool called bx. Adopting it here changed
five things:

- **The name stays nx.** It is the command Neptuno already ships and the
  repository, ADRs and release pipeline already exist.
- **nix-daemon stays the only store writer** (ADR-0001 holds for the tool
  tier). The draft dropped the daemon and had nxd unpack NARs into its own
  store, which meant its own substituter, a bind-mounted store, `nix` still
  needed for unfree builds, and plain `nix` commands that could no longer see
  the store. The leak nx 1.0 exists to fix comes from `PATH` and environment
  variables, not from who writes the store; shims and routing fix that alone.
- **Deployment status comes from `rpm-ostree status --json`.** `bootc status`
  needs root, and the CLI is unprivileged. Other deployments' rpmdbs are read
  in place under `/sysroot/ostree/deploy`.
- **The daily-driver gate moves from M4 to M5.** Neptuno rebuilds daily, so a
  host tier without the image upgrade lifecycle breaks on the next update.
- **Releases link against the image's glibc and libsolv** and are built in a
  `fedora:<release>` container. This amends ADR-0005: still a tarball pinned
  by checksum, no longer static.

## Considered options

- **Port nx 0.x to Rust first, then add the host tier.** The port would be
  replaced by M6 anyway (no profile, no registry), and it would delay M0, the
  spike that decides whether sysext works on the image at all.
- **A new repository for bx, keeping nx in Go.** Two tools, two release
  pipelines, and a migration path between them, for a tool that replaces nx
  once the tool tier lands.
- **Stay in Go.** libsolv and mount work need cgo or a large pure-Go rewrite;
  Rust has the libsolv FFI, the `rpm` crate and `nix-compat`.

## Consequences

- ADR-0003 (read the profile from disk) is superseded: there is no profile.
- ADR-0001, ADR-0002 and ADR-0004 still hold, for the tool tier.
- `testdata/` keeps nx 0.x's recorded Nix logs for the M6 progress parser.
- Neptuno's `NX_VERSION` stays on 0.x until M6 ships.

**Revisit when:** M0 shows sysext can't be merged on the image (the prefix
backend then becomes the only one), or the daemon turns out to be the
measured bottleneck for the tool tier.
