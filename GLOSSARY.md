# nx

nx installs packages from nixpkgs into a Nix profile. This is its language.

## Language

**package**:
Something a user installs, named by its nixpkgs attribute (`ripgrep`,
`python313Packages.requests`). One package can install several store paths
(its outputs).
_Avoid_: formula, derivation (in user-facing text)

**dependency**:
A store path a package needs at run time, from its closure.
_Avoid_: requirement

**pin**:
The nixpkgs revision the user registry points `nixpkgs` at. Every package
comes from it.
_Avoid_: lock, channel (the channel is where pins come from)

**channel release**:
One published state of nixpkgs-unstable: a revision Hydra has built, with its
packages.json.

**catalog**:
nx's local file listing every package of one nixpkgs revision, built from the
channel's packages.json.
_Avoid_: index, cache (the cache is the binary cache)

**binary cache**:
cache.nixos.org, where built store paths are downloaded from.

**generation**:
One saved version of the profile (`profile-N-link`). `history` lists them,
`rollback` goes back one.
_Avoid_: version (of the profile), snapshot

**plan**:
What Nix says it will fetch and build before it starts.
