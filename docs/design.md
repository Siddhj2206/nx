# nx: Package Manager for bootc Systems — Design

Status: design, adopted 2026-11-19 for nx 1.0 (the Rust rebuild). Drafted as "bx"; the tool keeps the nx name. Changes from that draft are listed in [ADR-0006](adr/0006-rust-rebuild-two-tiers.md).

## Problem and goals

nx is a fast, brew-like package manager for a Fedora bootc daily driver. Packages install and remove live, with no image rebuild and no reboot. One process never loads libraries from two different ABI sources.

**The problem with today's setup (nx 0.x, a front end to a Nix profile):**

- Nix closures carry their own glibc, Mesa, Wayland and libxkbcommon. These get mixed with the host's in one process, which gives glibc mismatches and broken GPU/Wayland loading.
- Shared packages exist in two places, so binaries and libraries shadow each other unpredictably.
- Rust builds that link native libraries are the worst case. A Bevy build needs libxkbcommon, wayland, vulkan-loader and alsa, and those must match the host's GPU driver stack.
- The fallback, adding packages to the bootc image, takes a \~10 minute build, an update and a reboot just to get a `-devel` package.

**Goals**

1. `nx add` / `nx remove` take effect in seconds, live, with no reboot, for everything except kernel-level packages.
2. Native builds (cargo, cc) link against the exact libraries the running system uses.
3. Brew-like UX: simple verbs, fast reads, nothing to learn about Nix or RPM internals.
4. Atomic generations with rollback, matching bootc's model.
5. No infrastructure to run. Packages come from Fedora mirrors and cache.nixos.org; nx owns only the local side.

**Non-goals**

- Hosting a package repository or build farm.
- Desktop GUI apps (use Flatpak).
- Managing the bootc image in any way. Kernel, driver and SELinux-level packages get a hint; the user puts them in their own image.
- Reproducible per-project build environments in the Nix-flake sense. `nx shell` gives a convenient env, not a hermetic build.

## Core principles

1. **One ABI per process.** Libraries a program loads come from exactly one source: the host's Fedora release, or the program's own sealed closure. Never both.
2. **Each source for what it does best.** Packages that plug into the system come from Fedora RPMs for the booted release; standalone tools come from nixpkgs by default, so they update independently of the image.
3. **Nix content never provides libraries or headers to the system.** It only lands in the sealed tool tier.
4. **One owner per name.** Each binary name on `PATH` has exactly one provider. nx never silently shadows a host binary.
5. **The image is the anchor.** Everything nx installs is resolved against, and version-locked to, the digest of the booted bootc image. nx reads the image and never writes it: no Containerfile edits, no builds, no switches.
6. **Declarative over imperative.** RPM scriptlets are translated into systemd declarative equivalents (sysusers, tmpfiles, presets) or refused. They are never run blindly.
7. **Reads are local and instant.** Only `add`, `upgrade` and the background refresh touch the network.
8. **Fail loudly, roll back cheaply.** Every change is a new generation; a bad one is undone with a symlink swap.

## Architecture overview: two tiers over a read-only image

nx sorts every package into one of two tiers by asking whose libraries it needs to load. The host tier shares the image's ABI and the tool tier carries its own sealed ABI. Packages that need boot or kernel involvement get a hint for the user's own image, which nx never touches.

&#91;embedded content: nx architecture · three tiers over the booted image\]

A root daemon, `nxd`, does the privileged activation steps for the host tier; everything else runs unprivileged. The host tier stays small (libraries, -devel, services, compilers); standalone CLI tools default to the tool tier, so they don't move with the image.

## Host tier

The host tier holds Fedora RPMs for the exact booted release, overlaid onto `/usr` so they look as if they were part of the image.

**What goes here:** packages that plug into the system: libraries, `-devel` packages, services, compilers and interpreters, anything that touches GPU, Wayland, audio or the session, plus standalone tools that only an RPM repo provides.

**Resolution**

- Read `ID` and `VERSION_ID` from `/usr/lib/os-release` and the booted image digest from `rpm-ostree status --json` (`container-image-reference-digest` of the booted deployment). `bootc status` needs root; rpm-ostree's answers unprivileged in about 50 ms.
- Load the image's rpmdb as libsolv's *installed* repo. Cache it as a `.solv` file keyed by image digest; it cannot change until the next deployment.
- Load the Fedora repos (`fedora`, `updates`, plus any user-configured COPRs) as available repos, from cached `.solv` files.
- Solve: install the requested packages; everything already in the image counts as present and is never replaced or upgraded. Packages built from the same source as something in the image (e.g. -devel) use that exact build, falling back to Koji when the repos have moved on (invariant 3 in Updates and edge cases).
- If the solution would replace or upgrade a package that is in the image, refuse and explain. Suggest `nx upgrade` after the next image update, or adding it to the image yourself.

**Unpacking**

- Download the RPMs, verify GPG signatures against the Fedora keys shipped in the image, and stream the zstd cpio payload into the content-addressed store.
- Never run `%pre`/`%post` scriptlets. See *Scriptlet translation*.
- Record each package's file list, `Provides` and owner metadata in the local SQLite index.

**Activation backends.** nx probes at startup and picks one; the user can force either.

| Backend | How it works | When used |
| --- | --- | --- |
| `sysext` (preferred) | One aggregate directory extension at `/var/lib/extensions/nx/`, with `usr/lib/extension-release.d/extension-release.nx` declaring `ID` and `VERSION_ID` (or `SYSEXT_LEVEL`). Activated with `systemd-sysext refresh`. Files appear at standard paths in `/usr`. | A test merge succeeds on this image |
| `prefix` (fallback) | The current generation's `usr` (`/var/lib/nx/current/sysext/usr`, no copy; a rollback moves it) is the prefix. `nx shell` and the shell hook export `PATH`, `PKG_CONFIG_PATH`, `CPATH`, `LIBRARY_PATH`, `LD_LIBRARY_PATH` (only inside `nx shell`), `XDG_DATA_DIRS`. Services go under `/etc/systemd/system` via drop-ins pointing into the prefix. | sysext merge fails (overlay depth, SELinux, unsupported) |

**Probing (M3).** Every unforced activation tries sysext first: label, link, `systemd-sysext refresh`, then `status --json` must list `nx` under `/usr`. If the merge fails (overlay depth, an unsupported kernel) nxd puts the previous generation back, removes the link, unmerges and uses the prefix backend, and the CLI says why. The next activation tries sysext again, so a system that can merge later goes back by itself. `[host] backend = "sysext" | "prefix"` in the manifest forces one, and a forced sysext that fails is an error. The backend in use is in `/var/lib/nx/backend` for `nx shell`.

**Rules for the sysext backend**

- Exactly one nx extension, never one per package, to spend only one overlayfs stacking level.
- Never enable bootc transient root (`transient = true` in `prepare-root.conf`); it costs another overlay level.
- Label the extension with `setfiles -r <extension dir> <file_contexts> <extension dir>` after assembly, so each file gets the label of the path it appears at once merged (`usr_t`, `bin_t`, `lib_t`). `restorecon` labels files by where they sit, so everything under `/var/lib/extensions` would show up in `/usr` as `var_lib_t` (M0).
- Only call `systemd-sysext refresh` when host-tier content actually changed. Run `systemctl daemon-reload` afterward if the systemd version doesn't do it automatically.

**Example:** `nx add wayland-devel libxkbcommon-devel` resolves to 1–3 small RPMs, unpacks them, rebuilds the extension directory with reflinks, and refreshes. `pkg-config --cflags xkbcommon` works immediately in the open shell.

## Tool tier

The tool tier is the default home for standalone CLI tools: prebuilt nixpkgs closures from cache.nixos.org, realised by the image's `nix-daemon`, with no Nix evaluation in the common case. Their libraries never reach anything outside their own process.

**What goes here:** any package that doesn't plug into the system (see *Tier routing*): ripgrep, neovim, jq, LSPs, rustup and similar. They update on the nixpkgs pin's schedule, independent of the image.

**What never goes here:** compilers, linkers and interpreters that build or load native code against host libraries (gcc, clang, python3, nodejs, perl, ruby), and anything whose closure contains GPU, display or audio client libraries (`libGL`, `libEGL`, `libvulkan`, `libwayland-client`, `libxkbcommon`, `libX11`, `libasound`, `libpulse`, `libpipewire`). Neptuno already learned this: nx 0.x's `nx add clang` replaced `/usr/bin/cc`, and Bevy builds linked against Nix's libxkbcommon.

**One pin.** All tool-tier packages come from one nixpkgs channel release (a revision Hydra has built), recorded in the lockfile. `nx upgrade --tool` moves the pin to the channel's current release. This is ADR-0004, kept as is.

**Finding store paths without evaluation**

- The catalog comes from the release's `packages.json.br` (attribute, pname, version, `mainProgram`, `outputsToInstall`, unfree/broken flags), as in nx 0.x: built once per release, searched in milliseconds.
- Store paths come from the same release's `store-paths.xz` (\~8 MB, every output Hydra built). Join it with the catalog on name, version and output to get attribute → store path.
- If the join is ambiguous for an attribute, fall back to evaluating just that attribute (see *Unfree and uncached packages*). Measure how often that happens on real data before relying on the join.

**Fetching and verifying** (nix-daemon writes, nx decides)

nx keeps [ADR-0001](adr/0001-nix-does-every-store-write.md): the daemon stays the only writer of `/nix/store`, and it already checks narinfo signatures and `NarHash` against the configured trusted keys. What nx adds is the decision of *what* to realise and the bookkeeping, with no profile and no registry.

1. Walk the closure via `.narinfo` `References` from cache.nixos.org, in parallel (port nx 0.x's `internal/cache` walker), for sizes and the plan.
2. Run `nix-store --realise <store path> --add-root <generation>/gcroots/<name> --indirect` with `--log-format internal-json`, so the daemon substitutes the closure and the generation holds the GC root. Progress comes from the log, as in nx 0.x (fixtures in `testdata/`).
3. Record references in `state.sqlite`; generations are the GC roots.

**What changes compared with Neptuno's Nix setup today**

| Piece | Status | Why |
| --- | --- | --- |
| `nix-daemon`, `nix.mount`, nixbld users, SELinux labels | Kept | Neptuno already ships and tests them; the daemon is the store's only writer |
| narinfo signatures, `NarHash` checks, root-owned store | Kept (in the daemon) | The real security boundary |
| `nix profile`, `~/.nix-profile` on `PATH`, the user flake registry | Dropped | nx's generations, shims and lockfile replace them; nothing from `/nix` reaches `PATH` except shims |
| Global Nix env (`XDG_DATA_DIRS`, `MANPATH` and friends from `nix.sh`/`nix.fish`) | Dropped for nx-installed tools | One ABI per process; the shim sets what each tool needs |

Plain `nix` commands keep working against the same store, as with nx 0.x. Dropping the daemon in favour of nx's own substituter is possible later (the draft's design, recorded in ADR-0006) but only if a measured problem calls for it.

**Unfree and uncached packages**

Hydra doesn't build unfree packages, so they are not in cache.nixos.org or `store-paths.xz`. nx handles them only when the package is on the manifest's `unfree` allowlist: it runs `nix build --impure` of that one attribute at the pinned revision with `NIXPKGS_ALLOW_UNFREE=1` (as nx 0.x does), rooted in the generation. Trusted extra caches (`nx cache add <url> <public key>`) go into the daemon's `extra-substituters`/`extra-trusted-public-keys` through nxd, so they are tried first. Most unfree CLI packages just fetch a vendor binary and patch it, so builds take seconds. Anything that fails to build is refused, pointing to an RPM repo for the host tier or the vendor's installer.

Unfree packages rebuild together when the pin moves.

**Exposing binaries**

- Each exposed binary is a tiny compiled shim in `~/.local/share/nx/bin` (after the system dirs on `PATH`) that sets per-process variables and `exec`s the real binary. Cost: about a millisecond.
- The shim sets: `LOCALE_ARCHIVE` (Nix glibc can't read Fedora's locale archive), `SSL_CERT_FILE=/etc/pki/tls/certs/ca-bundle.crt`, and `TERMINFO_DIRS` including the host's terminfo.
- nx never exports `LD_LIBRARY_PATH`, `PKG_CONFIG_PATH`, `XDG_DATA_DIRS`, `GIO_EXTRA_MODULES` or similar globally.
- Known limits: Nix's glibc can't load Fedora's NSS modules, so user lookups fail for systemd-homed, SSSD or LDAP accounts (plain local users are fine; `nx doctor` warns). Some Nix wrappers set env vars that child host processes inherit; `nx doctor` flags them.

**Garbage collection:** `nx gc` drops generations past retention and runs `nix-store --gc`; the daemon already skips paths that are live roots or in use by running processes.

**Port from nx 0.x:** the `packages.json.br` catalog builder, channel-release pinning, the parallel narinfo closure walker, and the per-package progress output.

## Image-level packages (outside nx)

nx never touches the image. It does not edit the Containerfile, build images, or run `bootc switch`/`upgrade`. When a package can only work from the image, nx refuses to install it and prints a hint the user acts on themselves.

**What triggers a hint**

- Kernel modules and akmods (NVIDIA, VirtualBox).
- SELinux policy modules.
- initrd content and units ordered before `systemd-sysext.service`.
- Scriptlets that edit existing system config (`/etc/nsswitch.conf`, `/etc/pam.d`, alternatives), or anything the scriptlet translator cannot handle.
- Prerequisites nx itself needs that its own package doesn't provide.

**What the hint contains**

- The package and the specific reason, e.g. "ships kernel modules" or "scriptlet edits /etc/pam.d".
- A suggested Containerfile line, printed only: `RUN dnf install -y akmod-nvidia`.
- Any installed host-tier packages that depend on it.

**After the user adds it to their image**

- `nx status` lists outstanding hints until then.
- On the next image digest, nx sees the package in the image's rpmdb, clears the hint, and drops any host-tier copies the image now provides.

## Tier routing and name ownership

The host tier is only for packages that plug into the system; standalone tools default to the tool tier. `nx add <pkg>` decides automatically from the RPM header (fetched by HTTP Range request, a few KB) and the nixpkgs catalog; `--host` and `--tool` override it, and `nx why` shows the reason.

**A package plugs into the system if it:**

- provides libraries, headers or `.pc` files (`lib*.so*`, `/usr/include`, `*.pc`)
- ships units, D-Bus services, udev rules, polkit rules, PAM or NSS modules
- provides plugins host programs load (GStreamer, GTK modules, Qt plugins)
- links GPU, display or audio client libraries
- is a compiler, linker or interpreter (gcc, clang, python3, nodejs, perl, ruby)
- needs setuid or file capabilities
- is the user's login shell

A package whose files are only binaries, man pages and shell completions is standalone.

**Routing order** (first match wins):

1. Needs the image (kernel modules, SELinux policy, initrd content, a scriptlet the translator rejects) → refuse with an image hint.
2. Plugs into the system → **host tier**, from Fedora or an enabled third-party repo.
3. Standalone and in cache.nixos.org at the pin → **tool tier**.
4. Standalone, unfree or uncached, on the `unfree` allowlist, and found in a trusted extra cache or buildable locally → **tool tier** (see *Unfree and uncached packages*).
5. Standalone and available from an enabled RPM repo → **host tier** (e.g. a vendor's own repo for an unfree tool).
6. Otherwise → refuse with fuzzy-match suggestions, or point to the vendor's installer.

Routing decisions and their reasons are recorded in the lockfile and cached per NEVRA or store path. `routing.cli_tools = "fedora"` in the manifest restores Fedora-first for standalone tools.

**Name mapping.** Fedora names are canonical. A small alias table maps common nixpkgs attribute names and binary names (`rg` → `ripgrep`). `nx search` shows both sources with the source marked.

**Name ownership**

- Every exposed binary name has exactly one owner: the image, the host tier, or one tool-tier package.
- Installing a package that would expose a name already owned elsewhere is refused unless `--override <name>` is passed. Overrides are recorded in the manifest and listed by `nx doctor`.
- Within the host tier, ownership is enforced by RPM file conflicts as normal.
- `nx why <name>` prints the owner, tier, source, version, and which generation added it.

## Repositories and COPR

The host tier accepts COPR and other third-party RPM repos through the same pipeline as Fedora's own. Adding a repo is always explicit, its key is pinned, and by default Fedora wins name collisions.

**Adding repos**

- `nx repo add copr:owner/project` expands to the COPR URL for the booted release and architecture, e.g. `https://download.copr.fedorainfracloud.org/results/<owner>/<project>/fedora-$releasever-$basearch/`, and the project key at its `pubkey.gpg`.
- `nx repo add <url>` adds any other RPM repo (VS Code, Tailscale, etc.).
- Repos the image already enables in `/etc/yum.repos.d` are imported read-only and marked as image-provided. nx never edits those files.
- `nx repo list` / `nx repo remove` manage them. Repos are recorded in `manifest.toml`; the metadata checksum and key fingerprint of each go in `lock.toml`.

**Pipeline:** identical to Fedora's. repomd → cached `.solv` → libsolv → RPM header classification → host tier. No zchunk is assumed for third-party repos.

**Guardrails**

| Concern | Rule |
| --- | --- |
| Trust | Show the key fingerprint on add and pin it. Packages must verify against the pinned key; a changed key blocks the repo until the user confirms. |
| Priority | Fedora repos have higher libsolv priority by default. A COPR package with the same name as a Fedora one is used only via `nx add foo@copr:owner/project` or a raised repo priority. |
| Image protection | No repo can replace or upgrade a package that is in the image (already enforced by the resolver). |
| Release lag | After a Fedora upgrade a COPR may lack the new release's chroot. Its packages fail to re-resolve and appear in `nx status`; the rest of the host tier proceeds. |
| Scriptlets | Same translator. COPR packages are more likely to hit unknown scriptlets and get an image hint instead. |

**Example manifest entry**

```toml
[[repo]]
name = "copr:atim/lazygit"
# dnf semantics: lower number wins; Fedora repos default to 99.
# Omit to keep Fedora preferred; 50 would prefer this COPR on name collisions.
# priority = 50
```

**Milestone:** M2, with `manifest.toml`; M1 resolves against the repos the image enables.

## Scriptlet translation

nx never executes RPM scriptlets. It pattern-matches each one against known idioms, emits the declarative systemd equivalent, and refuses the package if anything is left unmatched.

| Scriptlet does | nx emits instead |
| --- | --- |
| `useradd` / `groupadd` / `getent` guards | A `sysusers.d` file in the extension, then `systemd-sysusers` |
| `mkdir` / `chown` / `chmod` under `/var` or `/run` | A `tmpfiles.d` file, then `systemd-tmpfiles --create` |
| `systemctl preset` / `%systemd_post` macros | Apply the package's presets; enable/start only if the preset says so |
| `%systemd_preun` / `%systemd_postun` | Stop and disable on remove, before the refresh |
| `ldconfig` | Nothing (paths are standard on sysext); run it in the prefix backend |
| Cache triggers (`glib-compile-schemas`, `update-desktop-database`, `gtk-update-icon-cache`, `fc-cache`) | Run after refresh as post-activation hooks |
| `update-alternatives`, edits to existing `/etc` files, `semodule`, `dracut`, `depmod` | Refuse with an image hint |

**Implementation notes**

- Most Fedora scriptlets come from a small set of macros, so match on the expanded macro bodies (`%systemd_post`, `%sysusers_create_compat`, `%tmpfiles_create`). Keep a test corpus of real scriptlets from popular packages.
- Ship a per-package allowlist override file so a user can mark a scriptlet as "safe to ignore" after reviewing it.
- Units shipped in `/usr/lib/systemd/{system,user}` work as normal after the refresh and a daemon-reload. Enabling writes symlinks in `/etc`, which is writable on bootc.

## Store, generations, lockfile

All nx state lives under `/var/lib/nx` and is addressed by content. A generation is an immutable snapshot of the installed set; switching between them is one atomic rename.

**On-disk layout**

```
/var/lib/nx/
  store/rpm/<sha256>/          # unpacked RPM payload trees, one per package NEVRA+digest
  generations/<N>/
    sysext/                    # assembled extension tree (reflinks/hardlinks into store/rpm)
    bin/                       # tool-tier shims
    gcroots/                   # indirect Nix GC roots for this generation's tool tier
    manifest.toml              # what the user asked for
    lock.toml                  # what was resolved
  current -> generations/<N>   # atomic symlink swap
  cache/
    solv/<repo>-<checksum>.solv
    solv/installed-<image-digest>.solv
    repodata/                  # zchunk metadata
    nixindex-<rev>.sqlite
  state.sqlite                 # file ownership, name owners, history
/var/lib/extensions/nx -> /var/lib/nx/current/sysext
~/.local/share/nx/bin -> /var/lib/nx/current/bin
```

**Manifest** (`manifest.toml`, user intent, safe to edit and commit to dotfiles):

```toml
[host]
packages = ["wayland-devel", "libxkbcommon-devel", "alsa-lib-devel", "syncthing"]

[tool]
channel = "nixpkgs-unstable"
packages = ["ripgrep", "neovim", "zig"]
unfree = ["some-unfree-tool"]   # allowlist for the unfree fallback

[[cache]]
url = "https://nix-community.cachix.org"
key = "nix-community.cachix.org-1:..."

[routing]
cli_tools = "nix"   # or "fedora"

[overrides]
names = []
```

**Lockfile** (`lock.toml`, machine-written):

- image digest and Fedora `VERSION_ID`
- per repo: metadata checksum and timestamp
- per host package: NEVRA, sha256, the user request that pulled it in
- nixpkgs revision and, per tool package, store path and narHash

**Operations**

- Every mutating command builds generation N+1 in full, verifies it, then swaps `current` and refreshes. A crash mid-build leaves `current` untouched.
- `nx rollback [N]` makes generation N current, by default the one the current generation was built from (`parent` in `lock.toml`), so it undoes the last change even after earlier rollbacks; with no parent it takes the newest earlier number. It refuses a generation built for another image digest until M5's compatibility check. `nx generations` lists them with diffs.
- `nx apply` makes the system match `manifest.toml`, for declarative use and for new machines.

## Image upgrade lifecycle

Every new image digest invalidates the host tier, not only a Fedora major bump: a regular image update can change library versions that host-tier packages were resolved against. nx re-resolves for the staged image before the reboot and picks the matching generation at boot.

**When an update is staged**

1. Trigger: bootc ships `bootc-status-updated.path`, which watches the bootc root directory for changes. nx hooks a service onto its target.
2. Read the staged deployment's digest from `rpm-ostree status --json` and its `VERSION_ID` from that deployment's own `usr/lib/os-release`.
3. Build `installed-<staged-digest>.solv` from the staged deployment's rpmdb, read in place at `/sysroot/ostree/deploy/<osname>/deploy/<checksum>.<serial>/usr/lib/sysimage/rpm` (readable unprivileged).
4. Re-solve the manifest against it and assemble a new generation tagged with the staged digest. Packages now provided by the image are dropped from the host tier automatically.
5. If anything fails to resolve, keep going with the rest and record the failures for `nx status`. Send a desktop notification.
6. Re-check tool-tier closures (ELF scan, see *Doctor*). They don't depend on the image, so normally nothing changes.

**At boot**

- `nx-select.service` runs `Before=systemd-sysext.service`. It reads the booted digest and points `current` at the newest generation built for it.
- If none exists (the staging step failed or was skipped), it runs the compatibility check from \*Updates and edge cases\* on the previous generation and merges it if it passes; otherwise it points at an empty generation so the host tier simply isn't merged. The system still boots cleanly; nx resolves in the background and refreshes live when done.
- Old generations for digests that are no longer deployed are garbage-collected.

**Partly answered (2026-11-19):** on Neptuno the rollback deployment's rpmdb opens unprivileged in place (`rpm --dbpath <deployment>/usr/lib/sysimage/rpm`). Still to confirm in M5: that a *staged* deployment's tree exists under `/sysroot/ostree/deploy` before reboot (bootc may finalize it only at shutdown); if not, fall back to pulling the image with podman. The boot-time fallback above makes this non-fatal.

## Updates and edge cases

Six things update on independent clocks, and nx is correct only if a host-tier generation is always a function of (manifest, image digest, repo state). The image is the anchor: host-tier packages follow the image, and only packages unrelated to the image move with the repos.

**The clocks**

| What changes | Typical cadence | Triggered by | nx reaction |
| --- | --- | --- | --- |
| bootc image (digest) | user's choice, often weekly | `bootc upgrade` / `switch` / `rollback` | Re-resolve for the staged digest before reboot; select at boot |
| Fedora major release | \~6 months | user rebases the image | Same, plus `VERSION_ID` change, renames/obsoletes, COPR chroot gaps |
| Fedora `updates` repo | daily | Fedora | Background metadata refresh; notify; apply on `nx upgrade` |
| nixpkgs channel | days to weeks | upstream | Background index rebuild; apply on `nx upgrade` |
| COPR repos | arbitrary | COPR owners | Same as `updates` |
| nx itself | releases | `nx self-update` | State-format migration; no re-resolve needed |

**Invariants** (every code path must keep these; write tests for each)

1. **No overlap with the image.** A generation is never merged if any of its files also exists in the booted image's `/usr`. An overlay above `/usr` would silently shadow the image's newer files with older ones.
2. **Generations are bound to a digest.** A generation built for another digest is merged only if a compatibility check passes against the booted image: same `VERSION_ID`, every host-tier package's requirements satisfied by the booted rpmdb, and invariant 1.
3. **Exact builds for image relatives.** A host-tier package built from the same source package as something in the image (typically `-devel`, `-libs`, `-static`) uses that exact build (same NEVR), from the repos if still present, otherwise from Koji (`kojipkgs.fedoraproject.org`), verified against Fedora keys. Never a newer build that would require upgrading the image's copy.
4. **Image packages are never upgraded or replaced.** If a package needs a newer version of something in the image, nx refuses and says to update the image.
5. **Every deployed digest keeps a generation.** Booted, staged and rollback deployments each retain a matching generation; GC never removes them.
6. **Every manifest change resolves for all deployed digests.** `nx add` while an update is staged rebuilds both the current and the staged generation.
7. **Boot never needs the network.** Generation selection uses only local state.
8. **Manifest is intent, not state.** A package the image now provides stays in the manifest, marked image-provided, so it comes back if the image drops it.
9. **State is versioned.** `state.sqlite`, manifest and generation format carry a version. An older nx facing newer state falls back to the newest snapshot it can read, else goes read-only.

**Default update policy**

- A daily timer refreshes Fedora/COPR metadata (including `updateinfo.xml`, for security advisories) and the nixpkgs index. It never applies anything.
- nx sends a desktop notification when host-tier packages have security advisories, or when tool-tier packages are stale beyond a threshold.
- `nx upgrade` applies host-tier leaf upgrades (packages unrelated to the image) and tool-tier upgrades. `--host` / `--tool` restrict it. Image relatives never move here; they move with the image.
- Opt-in: `auto_upgrade = "security"` applies security advisories automatically. Off by default, because it swaps files under running programs.
- Image updates need no nx command: the staged-image hook and `nx-select` handle them.

### Image changes, nx packages unchanged

| Scenario | Risk | nx behavior | User sees |
| --- | --- | --- | --- |
| Image updated within the same release; staging hook ran | Host-tier relatives now mismatch the new image builds | Re-resolve for the staged digest; image relatives move to the new exact builds; packages the image now ships are dropped (invariant 8) | Nothing; after reboot everything works |
| Image updated; staging hook did **not** run (nx offline, timer missed, image switched by another tool) | No generation for the booted digest | At boot, `nx-select` runs the compatibility check (invariant 2) on the previous generation. Pass → merge it, re-resolve in background, refresh live. Fail → merge nothing, re-resolve in background | Usually nothing. On fail: host-tier packages missing for the first minutes after boot, then a notification |
| Image now ships a package the host tier had | Overlay would shadow the image's newer files | Overlap check (invariant 1) drops it from the generation; manifest keeps it as image-provided | `nx status`: "foo now provided by image" |
| Image drops a package the host tier depended on, or one the user wanted | Missing dependency | Re-resolve pulls it into the host tier from repos (image-provided entries in the manifest come back) | Nothing, or a hint if it is image-only |
| Image now ships a binary a tool-tier package also exposes | Name collision without any user action | Image wins (system dirs precede nx's bin on `PATH`); nx reports it | `nx status`: "rg from nixpkgs is shadowed by the image; remove?" |
| New Fedora major (e.g. 43 → 44) | `VERSION_ID` changes; renamed, obsoleted or retired packages; COPR lacks the new chroot | Re-resolve against the new release's repos; follow `Obsoletes`; anything unresolvable is excluded and listed **before reboot** | Pre-reboot notification: "after reboot, these will be missing: …" |
| `bootc rollback` to the previous deployment | Current generation built for the wrong digest | Invariant 5: the rollback digest's generation still exists; `nx-select` picks it | Nothing |
| Several updates staged before one reboot | Stale staged generations | Only the newest staged digest is resolved; older staged generations are GC'd | Nothing |
| `bootc switch` to a different image (e.g. a different base) | Different package set, maybe no `/nix` directory, maybe no rpmdb | Treated as a new digest; missing `/nix` disables the tool tier with a hint; missing rpmdb disables the host tier | Hints in `nx status` and `nx doctor` |
| Image built without an rpmdb | Cannot compute the installed set | Host tier disabled; tool tier still works | `nx doctor` explains |

### Repo changes, image unchanged

| Scenario | Risk | nx behavior | User sees |
| --- | --- | --- | --- |
| `updates` has a newer build of a host-tier leaf | None, normal upgrade | Applied on `nx upgrade` (or automatically for security advisories if opted in) | Notification, then the normal upgrade output |
| `updates` has a newer build of a library that is in the image | Newest `-devel` no longer matches the image's library | Invariant 3: keep the exact image build of the relatives; fetch from Koji if the repo dropped it | Nothing; `-devel` always matches |
| A new leaf needs a newer library than the image has | Would require upgrading the image's copy | Invariant 4: refuse | "foo needs libbar ≥ 2.1, image has 2.0: update your image" |
| A host-tier package is retired from the repos | No future updates | Keep the installed build from the store; mark orphaned; exclude it on the next release change if unresolvable | `nx status`: "orphaned" |
| Metadata stale or offline | Cannot fetch | Everything local still works; `add` of an uncached package fails cleanly; boot unaffected (invariant 7) | Clear offline error |
| Mirror out of sync (metadata newer than packages) | Checksum or 404 failures | Retry on another mirror; verify checksums from metadata | Nothing, or a retry message |
| Fedora release reaches EOL while the image stays on it | No more updates; repos move to the archive | Warn; point repo URLs at the archive so resolves keep working | Persistent warning in `nx status` |
| User runs `nx upgrade` and later `bootc upgrade` | Host-tier leaves newer than what the new image expects | Re-resolve for the staged digest re-checks everything; invariant 4 refuses anything that would need image changes | Nothing in the common case |

### nx itself

nx ships inside the user's image, installed by the user's own Containerfile. nx still never modifies the image; it just lives there.

**Packaging (decision):** an RPM (or equivalent Containerfile step) that installs:

- `nx` and `nxd` in `/usr/bin`
- units (`nxd.socket`, `nxd.service`, `nx-select.service`, `nx-refresh.timer`) in `/usr/lib/systemd/system`, enabled via presets
- the polkit rule in `/usr/share/polkit-1/rules.d`
- nothing for `/nix`: the tool tier needs the image's Nix (`nix-core`, `nix.mount`, the daemon), which Neptuno already ships. Without it the tool tier is disabled with a hint.

On Neptuno the units are declared in `build/packages/*.toml` like every other unit ("declare, then enable"); the install step follows nx 0.x's: a release tarball pinned by checksum in `20-overlay.sh`.

**Consequence:** nx's version changes only when the image changes. CLI and `nxd` always match, and there is no self-update. The hard case becomes `bootc rollback`: an older nx boots against state a newer nx already migrated.

| Scenario | Risk | nx behavior | User sees |
| --- | --- | --- | --- |
| Image update brings a newer nx | State format changed | Migrate `state.sqlite` / manifest / generation format at first run after boot (or in `nx-select`), after snapshotting the old state | One-line "migrated state to v3" |
| `bootc rollback` brings back an older nx | Older binary meets migrated state | Migrations keep the previous format's snapshot. An older nx finding newer state switches to the newest snapshot it understands, read-write. Generations record the format they were built with, so `nx-select` can still pick the rollback digest's generation | Nothing in the common case; a note in `nx status` |
| Rollback past a snapshot that no longer exists | No readable state | Read-only mode; `nx-select` still merges the rollback digest's generation if its format is readable; mutating commands refuse | "state is newer than this nx; boot the newer image or run `nx reset`" |
| New nx version changes routing rules (e.g. a longer GPU library list) | Existing installs no longer match the rules | Never re-route silently; `nx doctor` lists packages the new rules would route differently | Doctor report |
| New nx version changes the scriptlet translator | A package previously refused might now be allowed, or vice versa | Applied on the next re-resolve; changes listed in the pre-reboot notification like any other difference | Notification |

**Rules for state-format changes:** prefer additive schema changes that older readers ignore; keep the previous format's snapshot until no deployed image needs it (same retention as invariant 5); test every migration both forward and rollback.

### Tool tier

| Scenario | Risk | nx behavior | User sees |
| --- | --- | --- | --- |
| nixpkgs channel moves; image unchanged | None to the host | Applied on `nx upgrade --tool`; old store paths kept until GC | Normal upgrade output |
| Image updated; tools unchanged | None: closures carry their own libraries | Rescan only for new name collisions with the image | Nothing |
| Nix gone from a new image | No daemon; tools missing | Tool tier disabled; hint to add Nix back to the image | Hint |
| Tool upgraded while a process is running it | Running process keeps old store path | Nothing to do: old paths stay on disk until GC, which skips paths in use | Nothing |
| GC while a generation is still referenced by a rollback | Deleting what rollback needs | GC roots = all kept generations | Nothing |
| Pin moves while unfree tools are installed | Their store paths change; not in any cache | Re-run the unfree path for each (extra caches, then a local `nix build`); if one fails, keep the old build in the generation and report it | Slightly longer upgrade; a note if a rebuild failed |
| Extra cache rotates its key or disappears | Possible compromise, or misses | Key change blocks that cache until confirmed; misses fall through to a local build | Prompt on next upgrade |
| A tool's routing changes (e.g. Fedora image now ships it, or the classifier changes) | Same name provided twice | Never re-route silently; image wins on PATH; nx status and doctor report it | Status note |

### COPR and third-party repos

| Scenario | Risk | nx behavior | User sees |
| --- | --- | --- | --- |
| COPR rotates its signing key | Possible compromise | Repo blocked until the user confirms the new fingerprint | Prompt on next `upgrade` / `add` |
| COPR has no build for a new Fedora release yet | Package missing after reboot | Excluded from the staged generation; listed in the pre-reboot notification; retried on each metadata refresh | "foo (copr:x/y) unavailable for F44; will retry" |
| COPR rebuilds the same version (release bump only) | Churn | Treated as a normal update | Normal upgrade |
| COPR package collides with a newer Fedora package | Silent swap | Fedora wins unless the user pinned the COPR | Nothing, or a note in `nx info` |
| COPR repo deleted | 404s | Keep the installed build; mark orphaned | `nx status`: "orphaned" |

### Runtime and failures

| Scenario | Risk | nx behavior | User sees |
| --- | --- | --- | --- |
| Host-tier upgrade while programs run | Running processes keep old inodes but may later `dlopen` new plugins (GStreamer, GTK modules) next to old libraries | After activation, list processes holding replaced files (like `dnf needs-restarting`); `try-restart` services whose package changed, as `%systemd_postun_with_restart` would | "restart these: …" |
| Power loss while building a generation | Partial generation | Built in a temp dir, renamed into place only when complete; `current` untouched | Nothing |
| Power loss during `sysext refresh` | Extension unmerged | Activation is idempotent; `systemd-sysext.service` re-merges at boot | Nothing |
| Disk fills up | Failed download or unpack | Space check before downloading (size from metadata); GC suggestion | Clear error with `nx gc` hint |
| Two `nx` commands at once | Racing writes | One global lock in `/var/lib/nx`; read commands don't take it | Second command waits |
| `bootc upgrade` runs while `nx add` is running | Staged digest changes mid-resolve | Re-check the staged digest before finishing; if it changed, re-resolve for it too (invariant 6) | Slightly longer `add` |
| Overlay stacking depth exceeded on a new kernel | Sysext won't merge | `nx-select` detects the merge failure and switches to the prefix backend for that boot | Notification + `nx doctor` detail |

### Open questions for updates

- [ ] Do Koji's signed copies (`data/signed/<key>/`) exist for every build nx might need, and how should nx verify one that only exists unsigned? *Partly answered (M2):* they exist under the key's short ID (`f577861e` for Fedora 45) for the mesa builds tested; nx tries each of the repo's keys and refuses anything that doesn't verify, so an unsigned-only build is refused for now.
- [ ] How long does the boot-time compatibility check take for a realistic host tier? It must stay well under a second.
- [ ] Should the pre-reboot notification also be written somewhere `bootc status` users will see it?

## Privilege model

The `nx` CLI runs unprivileged. A small root daemon, `nxd`, does only the steps that need root, so there are no sudo prompts in normal use.

**nxd**

- Socket-activated systemd service on a Unix socket, one process per connection (`nxd.socket` with `Accept=yes` starts `nxd@.service` with the connection on stdin), so a request is handled in a fresh process that exits when it answers. A lock serializes concurrent requests. The unit uses no mount-namespace options (`ProtectSystem=`, `PrivateTmp=`): the merge would be invisible outside it.
- Exposes a narrow, typed API, one JSON line each way (`nx-proto`): `Activate { generation, backend? }` (M3); `run_sysusers()`, `run_tmpfiles()`, `enable_units([...])` (M4); `gc()` (M5). The client names a generation by number and never sends a path.
- Never downloads, resolves or parses packages itself. The unprivileged CLI does that work and writes into store directories owned by a dedicated `nx` system user; nxd validates that a generation is complete and correctly owned before activating it.
- Authorization via polkit: the active local session user in `wheel` may call it without a password. Everyone else needs admin auth.

**Decisions for M3 (best practice, checked in the VM 2026-11-19)**

- *Extension link.* `/var/lib/extensions/nx -> /var/lib/nx/current/sysext`, created by `tmpfiles.d`. `man systemd-sysext` names `/var/lib/extensions` the primary place to install extensions and `/run` and `/etc` suitable only for symlinks to them; only `/var/lib/extensions` is merged at boot without extra setup, and the link name `nx` is what matches `extension-release.nx`. On Neptuno the link merged, kept `usr_t` labels, and was merged again after a reboot. `setfiles -r` wants the resolved directory (`realpath`), not a path through `current`.
- *The `nx` user.* A system user from `sysusers.d` (`u!`, no login shell, home `/var/lib/nx`) owns the state directory and runs the unprivileged background services (refresh timer, staging). `tmpfiles.d` creates `/var/lib/nx` setgid with a default ACL for group `wheel`, so interactive wheel users, who may call nxd without a password, write generations as themselves without being members of a group nx has to manage. nxd accepts a generation whose files are owned by root, the owner of the state directory (`nx`) or the calling uid (`SO_PEERCRED`). It refuses setuid and setgid files, file capabilities, device nodes, FIFOs and sockets, and files writable by everyone, and an extension-release that doesn't match the running system. It never follows a symlink: a link in a package is just a link in the merged `/usr`. It does not use `DynamicUser=`: state under `/var/lib/nx` must outlive the service and be owned by a stable uid.
- *No wider trust.* A wheel user can already activate without a password, so a tree they own widens nothing for them. A user outside wheel is refused by polkit before nxd opens the tree, so what they own never reaches `/usr`.

- *One admin per store, for now.* nxd accepts files owned by root, `nx` and the caller, so a second wheel user can't activate a generation that links a package the first one unpacked. Until nxd (running as `nx`) does the unpacking, one account per machine adds packages.

**Trust boundaries**

- RPMs (repos and Koji): GPG-verified against Fedora keys from the image before unpacking; third-party repos against their pinned keys.
- Nix store paths: written only by `nix-daemon`, which verifies narinfo signatures against cache.nixos.org's key or a user-pinned extra cache key, and every NAR against its `NarHash`. Locally built unfree packages are trusted via the pinned nixpkgs revision and the vendor download hash it fixes.
- Generation contents: files owned by root in the final assembled tree; no setuid/setgid bits allowed from the host tier unless the RPM ships them and the user allowlists the package.
- Scriptlets are never executed (see *Scriptlet translation*).

## Performance design

nx should feel instant for reads and take about a second plus download time for installs. It gets there by never waiting on metadata in the foreground, never evaluating Nix on the hot path, and caching everything tied to the immutable image.

| Target (estimate) | Operation |
| --- | --- |
| < 100 ms | `list`, `why`, `search`, `info`, `status` (local SQLite only) |
| \~1 s + download | `add` of a host-tier package with warm metadata |
| near-instant | `add` of a cached tool-tier package (symlink only) |
| measure first | `systemd-sysext refresh` on the target image (the slowest remaining step) |

**Techniques**

1. **Background metadata refresh.** A systemd timer refreshes repo metadata and the nixpkgs index. Commands use the cache; `nx add --refresh` forces it. Never auto-update in the foreground like brew.
2. **zchunk metadata.** Fedora publishes zchunk repodata, so refreshes download only changed chunks. Skip `filelists` unless a query needs file paths.
3. **Precomputed installed set.** The image's rpmdb is turned into `installed-<digest>.solv` once per image digest.
4. **libsolv directly** (the solver dnf5 uses), loading cached `.solv` files. No Python, no dnf process.
5. **No Nix evaluation on the hot path.** Lookups use the prebuilt local index; closures come from `.narinfo` references.
6. **Parallel I/O.** HTTP/2 with a pool of connections, fastest-mirror selection, parallel downloads, and streaming zstd decompression straight into the store.
7. **Cheap generation assembly.** Reflinks (btrfs/xfs) or hardlinks from the store; directory sysext, so no squashfs/erofs build.
8. **Skip work.** Refresh the sysext only when host-tier content changed. Tool-tier changes only touch symlinks.
9. **Startup.** One Rust binary, no runtime to boot, lazy-open SQLite, no network unless the command needs it.

## CLI surface

The verbs mirror brew. Every mutating command prints the tier and source it chose, and supports `--dry-run`.

| Command | Does |
| --- | --- |
| `nx add <pkg> [--host\|--tool] [--override <name>]` | Install; routed automatically; live in seconds. Image-only packages get a hint instead |
| `nx remove <pkg>` | Uninstall; stops and disables its units first |
| `nx upgrade [--host\|--tool] [pkg]` | Host-tier leaf upgrades and/or moving the nixpkgs pin; new generation |
| `nx search <term>` | Search Fedora, third-party repos and the nixpkgs catalog, source marked |
| `nx info <pkg>` | Version, tier, source, routing reason, files, dependencies |
| `nx list` (`ls`) | Installed packages by tier |
| `nx why <name\|path>` | Owner of a binary or file, its routing reason, and the generation that added it |
| `nx repo add\|list\|remove` | Manage COPR and third-party RPM repos. `add` takes `copr:owner/project`, a `.repo` file's URL, or a base URL with `--key`; `--priority` sets the dnf priority; `--new-key` replaces a pinned key after a rotation |
| `nx cache add\|list\|remove` | Manage trusted extra Nix binary caches (URL + pinned key) |
| `nx shell [-- cmd]` | Env for building (prefix backend exports; sysext needs none) |
| `nx apply` | Make the system match `manifest.toml` |
| `nx generations` / `nx rollback [N]` | List / switch generations |
| `nx status` | Image digest, backend, pending re-resolves, failures, outstanding image hints |
| `nx doctor` | Health checks (below) |
| `nx gc` | Remove unreferenced store content |

**`nx doctor` checks**

- ELF scan of every exposed binary: `NEEDED` libraries, `RUNPATH`/`RPATH`, and glibc symbol versions. Flags any binary that would load libraries from both `/usr` and `/nix/store`.
- Environment leaks: `LD_LIBRARY_PATH`, `PKG_CONFIG_PATH` or `XDG_DATA_DIRS` entries pointing into `/nix` in the user's shell, and Nix wrappers that set them for children.
- Name collisions and active overrides.
- Sysext merge status, overlay stacking depth headroom, SELinux labels on the extension.
- Lockfile vs. booted image digest mismatch.
- Leftovers from nx 0.x Nix profiles (`~/.nix-profile` on `PATH`) during migration.

## Implementation: Rust, crates, layout

nx is written in Rust as one Cargo workspace. Rust was chosen because the namespace/mount syscalls, libsolv FFI and the bootc/composefs ecosystem are all first-class there, while Go's multithreaded runtime makes namespace work awkward and cgo complicates static builds.

**Workspace layout**

```
nx/
  crates/
    nx-cli/        # clap CLI, output, progress; unprivileged
    nxd/           # root daemon: activation, sysusers/tmpfiles, units
    nx-core/       # store, generations, manifest/lock, state.sqlite, routing
    nx-rpm/        # repo metadata, libsolv bindings, download, verify, unpack
    nx-nix/        # nixpkgs catalog, narinfo closure walk, realise via the daemon, internal-json log
    nx-activate/   # sysext + prefix backends, probing, refresh
    nx-scriptlet/  # scriptlet idiom matcher -> sysusers/tmpfiles/presets
    nx-elf/        # ELF scanning for routing and doctor
    nx-proto/      # typed CLI <-> nxd protocol
  systemd/         # nxd.socket, nxd.service, nx-select.service, nx-refresh.timer
  polkit/          # rules file
  tests/           # integration tests run in a bootc VM
```

Crates are added when a milestone needs them, not ahead of time; the tree above is where things go, not a checklist.

**Linking.** The binaries link dynamically against the image's glibc and libsolv (`libsolv` is already in the image through dnf5/rpm-ostree; build against `libsolv-devel` in a `fedora:<release>` container). nx then shares the host ABI like any other host-tier program, and releases are built per Fedora release instead of as static tarballs ([ADR-0006](adr/0006-rust-rebuild-two-tiers.md) amends ADR-0005).

**Candidate crates** (verify maintenance status before adopting)

| Need | Crate |
| --- | --- |
| CLI | `clap` |
| Async, HTTP/2 | `tokio`, `reqwest` |
| Local index | `rusqlite` |
| Config | `serde`, `toml` |
| RPM headers, payload, signatures | `rpm` (rpm-rs) |
| Dependency solving | libsolv via `bindgen` (own `-sys` crate) |
| Nix narinfo, store paths | `nix-compat` (from snix/tvix) |
| ELF parsing | `goblin` |
| Syscalls, mounts | `rustix` |
| Compression | `zstd`, `xz2` |
| Daemon IPC, polkit | Unix socket + `SO_PEERCRED`, or `zbus` |
| Logging | `tracing` |

**Conventions for Claude Code**

- `cargo clippy -- -D warnings` and `cargo fmt` clean on every commit.
- No `unwrap()` outside tests; errors via `thiserror` in libraries, `anyhow` in binaries.
- Every module that touches the system goes behind a trait so unit tests run without root, with a fake filesystem root.

## Risks and open questions

The biggest risk is the sysext backend on bootc. That is why the prefix backend is first-class and chosen automatically, not an afterthought.

| Risk | Evidence | Mitigation |
| --- | --- | --- |
| sysext/confext support on bootc is distro-dependent | The [bootc filesystem docs](https://bootc.dev/bootc/bootc-filesystem.7.html) say support depends mostly on the distribution, and a bootc PR documented them as unsupported ([Fedora Discussion](https://discussion.fedoraproject.org/t/unsupported-systemd-confext-and-sysext-on-bootc/202199)) | Probe at startup; fall back to prefix backend |
| overlayfs stacking depth exhausted | Same bootc docs; a Jan 2026 Silverblue sysext failure hit "maximum fs stacking depth exceeded", traced to a kernel regression ([thread](https://discussion.fedoraproject.org/t/systemd-sysext-service-is-unable-to-start/179403)) | One aggregate extension; no transient root; doctor reports headroom |
| SELinux labels on extension content | bootc docs mention SELinux labeling issues; fedora-sysexts runs `restorecon` on extension dirs | `setfiles -r` against the extension root after assembly (M0: `restorecon` gives `var_lib_t`); doctor verifies |
| Host tier drifts from the image | Any image update can change library versions | Re-resolve on every digest change; boot-time generation selection |
| Scriptlet translator misses an idiom | Long tail of packages | Refuse unknown; test corpus; per-package allowlist |
| Nix wrappers leak env vars to host children | Some wrappers set `XDG_DATA_DIRS` etc. | Doctor flags them; exclude GPU/display closures from tool tier |
| Nix missing from the image | Composefs root is read-only | Tool tier disabled with a hint; doctor checks the daemon socket |

**Open questions**

- [x] Where an image relative's exact build comes from once `updates` has moved on: Fedora 45 enables `updates-archive`, which keeps older update builds, so the resolver finds most of them there before Koji is needed. The resolver hides every other build of an image package's source (`Universe::hidden`).
- [x] Benchmark `systemd-sysext refresh` latency on the actual image (M0): 12–13 ms to merge or re-merge, 7 ms for a no-change refresh, 73 ms for `systemd-sysext.service` at boot. A refresh is an overlay mount, so it barely grows with extension size; assembly (unpack, `setfiles`) is what scales.
- [x] Confirm the systemd version on the image handles `refresh` and daemon-reload correctly: systemd 262 refreshes in place and reloads the service manager itself (`--no-reload` turns that off), so nx needs neither a restart of `systemd-sysext.service` nor its own `daemon-reload`.
- [ ] Best way to read a staged deployment's rpmdb before reboot.
- [ ] Whether to use `ID`+`VERSION_ID` or `SYSEXT_LEVEL` in extension-release, given re-resolution happens per digest anyway.
- [ ] Prefix backend: how to run services whose units hardcode `/usr` paths (drop-in rewrite vs. refuse).

**Open questions for the tool tier**

- [ ] How many top-level attributes map ambiguously when joining `packages.json.br` with `store-paths.xz`?
- [ ] Does `nix-store --realise` of a store path taken from `store-paths.xz` substitute cleanly when the local store has never seen its derivation?
- [ ] Can nxd add extra substituters without restarting a daemon that is mid-build?
- [ ] Which shim environment variables are actually needed beyond `LOCALE_ARCHIVE`, `SSL_CERT_FILE` and `TERMINFO_DIRS`? Test the 20 most-used tools.

## Prior art

None of these covers live install of host-ABI `-devel` packages on bootc, which is nx's reason to exist. Several are worth borrowing from.

| Project | Approach | What nx takes or avoids |
| --- | --- | --- |
| [fedora-sysexts](https://travier.github.io/fedora-sysexts) / [sysexts-manager](https://cfp.all-systems-go.io/all-systems-go-2025/talk/DCVQLK/) | Prebuilt per-release sysexts for Fedora image-based systems, updated with systemd-sysupdate; WIP manager aimed at bootc | Reuse build and lifecycle ideas (restorecon, refresh handling); nx resolves arbitrary RPMs locally instead of a curated catalog |
| [Toolpak](https://blogs.gnome.org/alatiera/) (GNOME OS, design stage) | DDI images, shared runtime at `/usr` + tool at `/app`, wrapper binaries enter a mount namespace; tools bundle all dependencies | Model for a future tool-tier backend; explicitly defers project build setups, so doesn't cover `-devel` |
| Homebrew on Universal Blue | Prefix with its own glibc, binaries prepended to `PATH` | Avoid: its binaries override system ones, and mismatches break things (the Toolpak post cites QEMU overriding the system GLib) |
| nx 0.x / Nix profiles | Nix closures exposed via profile, env vars set globally | Avoid: libraries and env leak into host processes, causing the glibc/Mesa mismatches nx exists to fix |
| rpm-ostree layering | Packages layered into the deployment | Avoid: on bootc, local rpm-ostree changes block `bootc upgrade` ([RHEL-35866](https://issues.redhat.com/browse/RHEL-35866)) |
| [ABRoot](https://awesome.ecosyste.ms/projects/github.com%2Fvanilla-os%2Fabroot) (Vanilla OS) | Generates a local OCI image with user changes and applies it on top | Avoid for the fast path: rebuild + reboot, same cost as an image rebuild |
| SUSE sysext images | Sysexts rebuilt alongside each OS snapshot ([systemd-devel](https://lists.freedesktop.org/archives/systemd-devel/2024-November/050870.html)) | Confirms the version-lockstep approach |

## Milestones for Claude Code

Build in this order, riskiest first. Each milestone ends with a working, tested command, not scaffolding. Develop and test in a bootc VM built from the user's own image, never on the daily driver until M5: Neptuno rebuilds daily, so the image digest changes almost every day and a host tier without the M5 lifecycle breaks on the next update.

1. **M0 — Spike: activation on the real image.** Hand-build a directory sysext containing one `-devel` package (`spike/m0-sysext.sh`); measure `merge`/`refresh` time; confirm SELinux labels and overlay headroom; build a Bevy hello-world against it. Write the result into this doc. *Exit:* backend decision confirmed with numbers. *Result (Neptuno 45.20261118, systemd 262, kernel 7.3.3, SELinux enforcing, in a KVM VM):* the sysext backend works and stays the default. `wayland-devel` and `libxkbcommon-devel` (60 files) unpack in 33 ms, label in 30 ms, merge in 12 ms; `pkg-config` and the headers work at once, and a C program using both compiles, links and runs (the Bevy build is the same link against the same files, so it waits for M3's exit check). The extension survives a reboot. Two findings changed the design: labels need `setfiles -r`, not `restorecon` (above), and overlay headroom is zero: `/` is already a composefs overlay, the sysext adds the second level, and a third overlay on `/usr` fails with "maximum fs stacking depth exceeded". So exactly one extension and no transient root are hard limits, and anything else that overlays `/usr` (`ostree admin unlock`) cannot coexist with a merged extension. Podman is unaffected. `dnf download` needs `--arch $(uname -m) --arch noarch`, or i686 builds collide on unpack.
2. **M1 — Host-tier resolver.** Repo metadata download and caching, libsolv bindings, installed-set `.solv` from the image rpmdb, `nx search` / `nx info` / `nx add --dry-run`. *Exit:* dry-run on `libxkbcommon-devel` lists only the missing RPMs, in < 1 s with warm cache. *Status:* resolver done. On Neptuno 45.20261118 the dry-run lists only `libxkbcommon-devel` (the image has `libxkbcommon` and `pkgconf`) in 0.57 s warm (installed set and three repos from `.solv` caches); cold, it is bound by the metadata download (~45 MB). `nx repo add` moved to M2, since repos are recorded in `manifest.toml`; until then the resolver uses the repos the image enables (`fedora`, `updates`, `updates-archive`). Commands read the metadata cache and download only for a repo with nothing cached; `--refresh` forces a download, and metadata past its `metadata_expire` (or kept because no mirror answered) draws a warning. The refresh timer that keeps it current comes with M5. Still to come from the spec: the Koji fallback for image relatives (M2, with package downloads); `nx info`'s files, dependencies and routing reason (M2 and M6); the image-only hint on `nx add` (M7); and search across the tool tier (M6).
3. **M2 — Store and generations.** Download, verify, unpack into the store; assemble generations; manifest and lockfile; `nx list`, `nx why`. *Exit:* generations build correctly without activation. *Status:* done. Package files are verified and unpacked in-process with the image's librpm ([ADR-0008](adr/0008-librpm-from-the-image.md)), against the repo's own keys only; every file is checked against the signed header's digest, and setuid, file capabilities and device nodes are refused. `nx add` re-solves the whole manifest (locked builds favored), downloads four at a time with each file checked against the metadata's SHA-256 (falling back to other mirrors), unpacks into `store/rpm/<sha256>` (refusing any entry whose path goes through a symlink, so a package can't write outside its directory), and builds `generations/<N>` (hardlinks into the store, `extension-release.nx`, `manifest.toml`, `lock.toml`), refusing any file the booted deployment's `/usr` already has (invariant 1) or two packages disagree about. A path under a directory the image has as a symlink (`/usr/sbin -> bin`) moves to where the symlink leads, as rpm would install it. The tree is checked against the plan and the TOML read back, everything is flushed (one `syncfs` for the store, one for the generation) before each rename, and `current` is swapped last. There is a space check before downloading. A spec the image already provides stays in the manifest (invariant 8), and `nx list` shows it as in the image. An older nx facing a generation a newer one wrote shows the newest one it can read and refuses changes (invariant 9). Added repos' fingerprints are shown before anything changes; a pinned key that no longer matches the lockfile blocks the repo until `nx repo add <repo> --new-key`, and a package that fails to verify suggests the same. `foo@copr:owner/project` takes a package from that repo only. The Koji fallback works: with `updates-archive` disabled on Neptuno 45.20261117, `nx add mesa-libEGL-devel` fetched the image's exact 26.2.6-1.fc45 from Koji's signed copies (`data/signed/f577861e/`), and a C program built against the merged generation (EGL, xkbcommon, wayland-client) linked and ran. `nx add wayland-devel libxkbcommon-devel` took 3.6 s with downloads; `nx list` takes 5 ms. `nx repo add` takes `copr:owner/project`, a `.repo` URL, or a base URL with `--key`, shows and pins the key, and builds a generation like any manifest change. Until nxd (M3) the CLI needs write access to the state directory (`NX_STATE_DIR` overrides `/var/lib/nx`), store files belong to that user, and files outside `/usr` are left out of the extension (reported) until the scriptlet translator (M4). Left for later: the live download block under the `↓` lines; `state.sqlite` (`nx why` reads the generation's tree and lockfile, which is instant at this size; the index arrives when history and `nx generations` need it, M3); the `nx gc` hint on a full disk (with `nx gc`, M5); and `nx remove`, `nx rollback` and `nx generations` (M3).
4. **M3 — Activation and nxd.** Sysext and prefix backends with probing; nxd with polkit; `nx add`, `nx remove`, `nx rollback` live. *Exit:* `nx add wayland-devel` then `cargo build` of a Bevy app succeeds with no reboot. *Status:* done. `nx add`, `remove`, `repo add|remove` and `rollback` build or pick a generation and ask nxd to make it live (`nx generations` lists them from their lockfiles; `state.sqlite` stays deferred). nxd is socket-activated, one process per connection; it authorizes through `pkcheck` (the wheel rule allows an active local session without a password), validates the tree, labels it with `setfiles -r`, swaps `current`, links `/var/lib/extensions/nx` and runs `systemd-sysext refresh`, then checks `status --json` lists `nx` under `/usr`. On the Neptuno 45.20261118 VM an activation takes 80–150 ms end to end, and a request from a user polkit doesn't allow is refused with `current` untouched. `nx shell` exports the prefix backend's variables; with sysext it has nothing to add. Exit check: `nx add cargo wayland-devel libxkbcommon-devel alsa-lib-devel systemd-devel` (Fedora's own cargo, from the host tier, 295 s of that was the 81 MB download in the VM) and then `cargo build` of a Bevy 0.20 app with default features (wayland, x11, alsa, udev) succeeded in 5 m 52 s with no reboot. `tests/vm/m3.sh` repeats the add, remove, rollback, refusal and prefix checks. Left for later: forcing the backend from the CLI (only `[host] backend` in the manifest), services and sysusers/tmpfiles from packages (M4), the compatibility check that would let `nx rollback` cross image digests (M5, so it refuses now), and `nxd`'s own re-check of invariant 1 (nx-select needs it at boot, M5).
5. **M4 — Scriptlets and services.** Translator for sysusers/tmpfiles/presets/triggers with a test corpus; unit enable/start/stop. *Exit:* `nx add syncthing` installs and runs the service; unknown scriptlets are refused with a clear message.
6. **M5 — Image upgrade lifecycle.** Staged-image re-resolve, `nx-select.service`, notifications, GC. *Exit:* a bootc upgrade + reboot comes up with a matching host tier and no manual steps.
7. **M6 — Tool tier.** Port nx 0.x's catalog, narinfo walker and internal-json progress; the `store-paths.xz` join; realisation through the daemon with per-generation GC roots; compiled shims; the system-plugging classifier and routing; the unfree path and extra caches. *Exit:* `nx add ripgrep` works with no Nix profile and no Nix env on `PATH`, and an allowlisted unfree tool installs. This is where nx 1.0 replaces nx 0.x in Neptuno.
8. **M7 — Doctor, image hints, polish.** `nx doctor`, `nx apply`, migration helper from nx 0.x, shell completions with live values (package names from the catalog and the repos, installed packages for `remove`): compare clap_complete's dynamic completion with a [usage](https://usage.jdx.dev/) spec exported from clap, which also gives man pages and Markdown reference. *Exit:* daily-driver ready.

**Working on it:** `AGENTS.md` has the gates and conventions. Work one milestone per session, and update this doc's open questions as they get answered.
