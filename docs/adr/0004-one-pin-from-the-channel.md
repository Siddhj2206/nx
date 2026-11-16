# One nixpkgs pin, moved to the channel's release by nx upgrade

The user registry pins `nixpkgs` to `github:NixOS/nixpkgs/<rev>`, and every
package is installed as `flake:nixpkgs#<attr>`. `nx upgrade` asks
channels.nixos.org/nixpkgs-unstable for its current `git-revision`, pins that
revision, and runs `nix profile upgrade --all`. A channel release only exists
once Hydra has built it, so its packages are in the binary cache, and its
`packages.json` matches the pin exactly.

When every profile element was already built from the pinned revision,
`upgrade` stops after the one HTTP request: there's nothing for Nix to do.

## Considered options

- **Pin the branch head** (what the bash nx did). It can be ahead of what
  Hydra has built, and it has no matching packages.json.
- **The channel's nixexprs tarball as the flake input.** It's smaller to
  download, but existing profiles name the GitHub revision, and mixing the
  two would make every package look changed.

**Revisit when:** nx supports more than one nixpkgs (stable alongside
unstable, say).
