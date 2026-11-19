# Package files are verified and unpacked with the image's librpm

The host tier downloads RPMs, checks their signatures and unpacks their
payloads into the store. nx does all three in-process with the librpm the
image already carries, through bindgen bindings checked in to one crate,
`nx-librpm`, the same way ADR-0007 brings in libsolv.

- **Already loaded.** libsolv's `libsolvext` links `librpm`, `librpmio` and
  `librpm_sequoia`, so nx has loaded the image's librpm since M1. Calling it
  directly adds no runtime dependency.
- **The image's trust policy.** Signatures are checked by rpm-sequoia with
  the image's rpm configuration, as dnf and rpm-ostree check them. nx hands
  librpm a keyring holding only the repo's own keys (the image's key files,
  or the one `nx repo add` pinned) and never the system keyring, and it
  refuses a package with no signature, which `rpmReadPackageFile` alone
  accepts.
- **The header is the source of truth.** The signed header gives each
  file's mode, link target and digest; nx streams the payload with librpm's
  archive reader, checks every file against its digest, and refuses
  setuid/setgid files, file capabilities, device nodes and paths that leave
  the package root. The scriptlet translator (M4) and the routing
  classifier (M6) read the same headers.
- **`unsafe` stays in two crates.** `nx-solv` and `nx-librpm` allow it and
  expose safe APIs; the workspace denies it everywhere else.

## Considered

- **Running `rpmkeys` and `rpm2archive`.** Both work unprivileged
  (`rpmkeys --dbpath <dir>` keeps a private keyring), but the verdict comes
  back as text ("digests signatures OK"), a package with no signature still
  passes `--checksig`, and the unpacked tree arrives as a tar stream with
  rpm's metadata already flattened.
- **The `rpm` crate (rpm-rs).** Pure Rust, but it brings its own OpenPGP
  stack and header parser, so nx's answer to "is this signed" could differ
  from the image's own tools.
