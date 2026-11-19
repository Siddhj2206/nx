# nx

nx installs packages on a Fedora bootc system: Fedora RPMs onto the image's
`/usr`, and sealed nixpkgs tools beside it. This is its language.

## Language

**image**:
The bootc container image the machine boots, identified by its **image
digest** (`sha256:…`, rpm-ostree's `container-image-reference-digest`).
nx reads it and never writes it.
_Avoid_: base, OS (in code), commit (the ostree checksum is something else)

**deployment**:
One image installed on disk: the booted one, at most one staged for the next
boot, and the rollback. Each has a digest and a root under `/sysroot`.

**host tier**:
Packages from Fedora (or an added RPM repo) for the exact booted image,
overlaid onto `/usr`. For anything that plugs into the system: libraries,
`-devel`, compilers, services.
_Avoid_: layer, layered packages (that is rpm-ostree's word)

**tool tier**:
Standalone CLI tools from one nixpkgs pin, each loading only its own
closure's libraries, exposed through shims.

**image relative**:
A host-tier package built from the same source as one in the image
(`libxkbcommon-devel` for `libxkbcommon`). Always the image's exact build.

**image hint**:
What nx prints instead of installing a package that only works from the
image (kernel modules, SELinux policy): the reason and a Containerfile line.

**package**:
Something a user asks for: an RPM name in the host tier, a nixpkgs attribute
in the tool tier. One package can pull in several RPMs or store paths.
_Avoid_: formula, derivation (in user-facing text)

**dependency**:
An RPM or store path a package needs at run time.
_Avoid_: requirement

**manifest**:
`manifest.toml`: what the user asked for. Intent, safe to edit by hand.

**lockfile**:
`lock.toml`: what nx resolved, per image digest and nixpkgs revision.

**generation**:
One immutable, complete installed set under `/var/lib/nx/generations/<N>`,
built for one image digest. `current` points at one; rollback moves it.
_Avoid_: snapshot, version (of the installed set)

**store**:
`/var/lib/nx/store/rpm/<sha256>`: each package file nx installed, verified
and unpacked once, named by the file's SHA-256. Generations link into it.
_Avoid_: cache (the cache holds metadata and downloads, and can be deleted)

**extension**:
The one systemd-sysext directory nx assembles from a generation's host tier.

**shim**:
The tiny binary on `PATH` for each tool-tier command; sets the few variables
the tool needs and execs it.
_Avoid_: wrapper (Nix uses that for its own scripts)

**pin**:
The nixpkgs revision the tool tier comes from. Every tool-tier package comes
from it.
_Avoid_: lock, channel (the channel is where pins come from)

**pinned key**:
An added repo's signing key, saved by `nx repo add` under
`/var/lib/nx/keys` and recorded by fingerprint in `lock.toml`. The repo's
packages must verify against it; a changed one blocks the repo until
`nx repo add <repo> --new-key`.
_Avoid_: pin (that is the nixpkgs revision)

**channel release**:
One published state of nixpkgs-unstable: a revision Hydra has built, with its
packages.json.

**catalog**:
nx's local file listing every package of one nixpkgs revision, built from the
channel's packages.json.
_Avoid_: index, cache (the cache is the binary cache)

**binary cache**:
cache.nixos.org, where built store paths are downloaded from.

**plan**:
What Nix says it will fetch and build before it starts.
