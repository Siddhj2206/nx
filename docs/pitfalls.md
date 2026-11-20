# Pitfalls

What the code can't tell you: behaviour of bootc, rpm, libsolv, systemd and
Nix that cost a bug or a VM session to learn. Grouped by area; read the
section for the area you're changing. Add one when you learn something the
hard way.

## Reading the system

- `bootc status` needs root; the CLI is unprivileged. Use
  `rpm-ostree status --json` (about 50 ms), and read other deployments in
  place under `/sysroot/ostree/deploy/<osname>/deploy/<checksum>.<serial>`:
  their os-release and rpmdb are readable without root.
- `/` on a bootc system shows the merged extension too. Invariant 1 checks
  against the booted deployment's tree under `/sysroot`
  (`Machine::image_root`), and a bootc system is recognised by
  `/run/ostree-booted`, never by whether rpm-ostree answers.
- A deployment's digest is `container-image-reference-digest`; the
  `checksum` field is the ostree commit, which is not what generations are
  keyed by.
- `nx select` runs before `systemd-sysext` with no rpm-ostree daemon: the
  booted deployment comes from `ostree=` on the kernel command line and its
  digest from `ostree show --print-metadata-key=ostree.manifest-digest`.
  `Machine::targeting` re-points the resolver at a staged or rollback
  deployment's own tree; only the booted image's generation is activated.

## Host tier: repos, metadata, the solver

- Fedora 45's repo files are in `/usr/share/dnf5/repos.d`, not
  `/etc/yum.repos.d`; a file of the same name in `/etc` replaces one in
  `/usr`. `updates-archive` is enabled and holds older builds, which is
  where an image relative's exact build is found when `updates` has moved
  on.
- Image relatives are enforced by hiding other builds of an image
  package's source (`Universe::hidden`) from the solver, search and info
  alike, and the installed repo is locked, so a solve can only add
  packages. Filter by `Universe`'s arch, never a hardcoded one.
- An image relative whose exact build no repo has (`Universe::missing`)
  comes from Koji's signed copies (`data/signed/<short key id>/`). The
  solver's problems name the hidden builds in the way (`Problems::involved`);
  re-solving with the newer builds visible doesn't work, because a newer
  `-devel` requires its own newer library, which the locked image refuses.
- `repo_add_rpmdb` reads under the pool's rootdir only with
  `REPO_USE_ROOTDIR`; without it, a staged or rollback image's resolve
  reads the running system's rpmdb (1.0.0 did).
- libsolv selections honour `pool->considered`, so `Pool` clears it before
  selecting and on `prepare`; a stale map hides packages added since (Koji).
- Commands load metadata with `Refresh::IfMissing`: a read never waits on
  the network for a repo that has a cache. Expired or unreachable metadata
  is reported through `Synced::freshness`, not hidden.
- The metadata cache is `/var/lib/nx/cache` when the caller can write it.
  Wheel membership asks NSS (`getent`, `id -G`) after the local files.
- rpm 6 won't create an rpmdb unprivileged (`rpm --initdb` fails on its
  lock). Tests copy a recorded one from `tests/fixtures/rpmdb/`.

## Store, generations and the state lock

- Changing state needs write access to `/var/lib/nx`: the `nx` user owns it
  and wheel has an ACL (`systemd/nx.tmpfiles`); in the VM `tester` is in
  wheel. `NX_STATE_DIR` points nx elsewhere, and then nothing is activated
  (nxd serves only `/var/lib/nx`): `current` moves, nothing merges.
- A manifest change goes through `Session::open`'s `change` closure, which
  runs under the state lock. Reading the manifest before taking the lock
  loses a concurrent command's change.
- `nx add`, `remove` and `repo` build a generation and nxd activates it
  (`Session::install`); `generation::build` never touches `current`. nxd
  doesn't take the state lock the CLI holds while it asks. The session
  holds the lock until `current` has moved (released earlier, a waiting
  command builds on the old generation and drops the change), then lets
  go before `sync::deployed`, which takes it per image: a second `flock`
  from the same process on another descriptor would wait forever.
- The state lock falls back to a read-only open only for someone who can
  write the directory: flock needs no write access, so a fallback for anyone
  would let a non-administrator take the lock and skip the permission check.
- `nx rollback` follows `parent` in `lock.toml`, not the numbering: after a
  rollback, the next generation is numbered past the abandoned ones.
- Nothing gets its final name before its data is on disk: the store stages
  packages and commits them after one `syncfs`, and a generation is synced
  before its rename. An `fsync` per unpacked file is far slower.
- Unpacking checks every parent directory with `symlink_metadata`: a
  package that ships `usr/x -> /etc` and then `usr/x/f` would otherwise
  write outside the store. Files are created with `create_new` (`O_EXCL`
  never follows a symlink).
- Fedora 42 and later have `/usr/sbin -> bin`. A package's `usr/sbin/foo`
  goes to `usr/bin/foo` in the extension; a real `usr/sbin` directory would
  hide the image's symlink.
- `/etc` and `/var` content goes to `usr/share/factory` plus a generated
  `nx-<pkg>.conf` tmpfiles file; never write into `/etc` directly.
- A store entry carries `.nx-meta.json` (scripts, files outside `/usr`); an
  older entry without it is fetched again. A generation carries
  `activation.toml`, which nxd diffs against the merged one.
- A scriptlet allowance (`[[scriptlets]]`) is dropped by `nx remove` when its
  package leaves.
- Generations `nx-sync.service` builds as root have extension directories
  without wheel's ACL (package modes cut the ACL mask), so a wheel user
  can't delete them: `state::discard` renames them to `.trash-*` beside
  themselves (renaming needs write access to the parent only), and the next
  gc as root (`nx sync`'s) purges them. Every listing skips dot names.

## nxd and activation

- The nxd unit must not use `ProtectSystem=`, `PrivateTmp=` or any
  mount-namespace option: the merge would happen in the unit's own
  namespace and nothing else would see it.
- nxd accepts files owned by root, `nx`, the caller and wheel members, and
  checks again that the extension doesn't hide the booted image's files.
- nxd runs as root in a directory wheel can write: it writes `current`
  beside its name (`create_new` or a fresh symlink, never
  a plain `write` that follows a planted link), flushes, then renames.
- The extension link names root's copy, `/var/lib/nx-merged/<N>.<pid>`,
  never a generation: a generation is its builder's, and merging it would
  let anything running as them rewrite `/usr`. nxd (and `nx select` at
  boot) makes the copy in `validate`, reflinked; keep the checks on the
  copy and on opened descriptors, not on the generation. Don't chown the
  store instead: with `fs.protected_hardlinks=1` the next build couldn't
  hardlink files it doesn't own.
- systemd-sysext finds nx through the symlink `/var/lib/extensions/nx`
  (its boot-time search path), not `/run/extensions`. A generation's
  extension directory must be named `nx` when merged: its release file is
  `extension-release.nx`.
- `systemd-sysext status --json=short` lists `{"hierarchy","extensions"}`;
  nxd checks `nx` under `/usr` after a refresh, since a refresh can succeed
  and merge nothing, which is an error.
- `setfiles -r` fails with "Not a directory" on a path through the `current`
  symlink; pass `realpath`.
- `systemctl preset --now` is invalid; preset, check `is-enabled`, then
  start. `systemctl disable` takes no globs: nxd expands `foo@*.service`
  itself. nxd acts only on units the extension ships, never a client's name.

## Shell, PATH and the session

- `environment.d` snippets with `${PATH}` belong in `/usr/lib/environment.d`
  only: pam_env (pam 1.7 with libeconf) reads `/etc/environment.d/*.conf` as
  drop-ins of `/etc/environment`, unexpanded, and a login's PATH becomes the
  literal `${PATH}:…` (no `bash`, no `grep`).
- A bootc image's fish reads `/usr/etc/fish` and `vendor_conf.d`, never
  `/etc/fish/conf.d`, and computes its vendor directories before anything
  sets `XDG_DATA_DIRS`: `dev/vm-install` bind-mounts a copy of
  `/usr/share/fish/vendor_conf.d` for `nx.fish`.

## The test VM

- polkit reads actions only from `/usr/share/polkit-1/actions`, which is
  read-only in the VM; `dev/vm-install` bind-mounts a copy (until reboot,
  run it again). An ssh login is not a local, active session, so the wheel
  rule can't allow it: the VM has a rule for `tester` (`dev/vm-test.rules`).
- Staging an update: the registry is slow, so `podman save --format
  oci-archive` the host's image, copy it to the VM and
  `bootc switch --transport oci-archive <file>` (as a `systemd-run` unit, so
  it outlives ssh). The VM's `/boot` is an autofs: `ls /boot` first, or
  `ostree-finalize-staged` fails at shutdown with "Remounting /boot
  read-write: Invalid argument" and the update is dropped.

## Tool tier: Nix

- Fedora's nix ships only `nix`: no `nix-store`, `nix-env` or `nix-build`.
  Realise with `nix build <path> --out-link <link>` (an indirect root that
  names the link's absolute path, so make it at the generation's final name).
- `store-paths.xz` covers every platform Hydra builds and narinfos carry no
  `System`; store paths come from `nix eval` (ADR-0009), never a name match.
- `packages.json.br` marks every unfree, broken or insecure package
  `available = false` (Hydra allows none of them): only an `x` flag without
  `u`, `b` or `i` means "not for this platform".
- Don't set `SSL_CERT_FILE` or `LOCALE_ARCHIVE` for Nix tools: they find the
  host's certificates and locale archive by default, and Fedora 45 has no
  `/etc/pki/tls/certs/ca-bundle.crt`. Only `TERMINFO_DIRS` is needed.
- `nix store gc` prints its "… freed" summary on stdout, not stderr.
- Read the store through the `Nix` trait (`Nix::has`, `Nix::on_disk`,
  `Nix::available`), never `Path::new("/nix/store/…")`: the machine running
  the tests may have a store with the very paths a fixture names. HTTP for
  the tool tier is `nx_http::Http`, the same `Env.http` the host tier uses.

Nix's internal-json log, from nx 0.x:

- Nix logs download retries at error level with "warning:" in the text, and
  logs recovered failures as "error (ignored):". Classify by text, not level.
- A plan's paths arrive one message each, and a build plan and a fetch plan
  are two separate announcements.
