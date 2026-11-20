# Releases are static tarballs the image pins by checksum

> **Amended** by [ADR-0006](0006-rust-rebuild-two-tiers.md): nx 1.0 links against the image's glibc and libsolv and is built in a `fedora:<release>` container. Tarballs and the pinned checksum stay.

A `v*` tag builds `nx` for linux/amd64 and linux/arm64 with `CGO_ENABLED=0`,
packs each into `nx-<version>-linux-<arch>.tar.gz` with the license, and
publishes them with a `SHA256SUMS` file. Neptuno's image downloads one by
URL and checks it against a pinned sha256, which Renovate bumps.

## Considered options

- **An RPM in a COPR.** Another build system and repo to keep up, for one
  static binary.
- **Building in the image.** Puts a Go toolchain in the image build.

**Revisit when:** another distribution wants nx packaged its own way.
