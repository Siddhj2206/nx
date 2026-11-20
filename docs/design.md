# nx: Package Manager for bootc Systems — Design

Status: built. nx 1.0 (the Rust rebuild) shipped as `v1.0.0` on 2026-11-20 and Neptuno runs it; this doc describes it, with *As built* notes where the build differs from the plan. What comes next is in [plan-1.1.md](plan-1.1.md), and its decisions land here as they are made. Drafted as "bx"; the tool keeps the nx name. Changes from that draft are listed in [ADR-0006](adr/0006-rust-rebuild-two-tiers.md).

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
- Reproducible per-project build environments in the Nix-flake sense.

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
- Record each package's scriptlets and files outside `/usr`, read from its signed header, beside it in the store (`.nx-meta.json`); what a generation holds is its `lock.toml`.

**Activation.** One aggregate directory extension, linked at `/var/lib/extensions/nx`, with `usr/lib/extension-release.d/extension-release.nx` declaring `ID` and `VERSION_ID` (or `SYSEXT_LEVEL`), merged with `systemd-sysext refresh`: files appear at standard paths in `/usr`. nxd labels root's copy, links it, refreshes, and then `status --json` must list `nx` under `/usr`. If the merge fails (overlay depth, an unsupported kernel) nxd puts the previous generation and link back and the activation is an error that carries what `systemd-sysext` said; `nx doctor` says whether nx's extension is merged.

nx 1.0 also had a *prefix* backend: when the merge failed, nxd left the generation's `usr` unmerged and `nx shell` exported it as a prefix (`PATH`, `PKG_CONFIG_PATH`, `CPATH`, …), with `nx-settle.service` falling back at boot. Neptuno always merged, so 1.1 dropped it (ADR-0011) with `[host] backend`, `/var/lib/nx/backend`, `nx shell` and `nx select --after-merge`.

**Rules for the extension**

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

**Finding store paths: one evaluation per attribute and pin** ([ADR-0009](adr/0009-store-paths-by-evaluation.md))

- The catalog comes from the release's `packages.json.br` (attribute, pname, version, `mainProgram`, `outputsToInstall`, unfree/broken flags), as in nx 0.x: built once per release, searched in milliseconds. Search, info and routing read only the catalog.
- Store paths come from evaluating just the requested attributes, all in one `nix eval --json --file <release>/nixexprs.tar.xz` call, for `x86_64-linux` (the `Universe`'s arch), the outputs in `outputsToInstall` and the `drvPath`. The result goes into `lock.toml` per attribute, so an attribute is evaluated once per pin: `nx add` evaluates what is new, `nx upgrade --tool` evaluates everything once after moving the pin. Warm, an attribute takes 0.5–1.9 s and 130–550 MB; the first evaluation at a pin first fetches the release's nixexprs tarball (34 s on the VM), which the refresh timer can do ahead of time.
- The `store-paths.xz` join was measured and dropped (M6 spike): the file lists every platform Hydra builds, so 98% of the attributes it has match several paths by name; catalog names don't match store names for language packages (`actdiag` is `python3.14-actdiag-3.0.0`); and wrappers Hydra doesn't build (`neovim`) aren't in it at all.

**Fetching and verifying** (nix-daemon writes, nx decides)

nx keeps [ADR-0001](adr/0001-nix-does-every-store-write.md): the daemon stays the only writer of `/nix/store`, and it already checks narinfo signatures and `NarHash` against the configured trusted keys. What nx adds is the decision of *what* to realise and the bookkeeping, with no profile and no registry.

1. Walk the closure via `.narinfo` `References` from cache.nixos.org, in parallel (port nx 0.x's `internal/cache` walker), for sizes and the plan.
2. Run `nix build <store path>… --out-link <generation>/gcroots/<name> --log-format internal-json`, so the daemon substitutes the closure and the generation holds the GC root (`--out-link` registers an indirect root under `/nix/var/nix/gcroots/auto`, as `nix-store --add-root --indirect` did; Fedora's nix ships only the `nix` command). A path no cache has (a wrapper such as `neovim`) builds locally from its `drvPath` (`nix build <drv>^<outputs>`). The auto root names the link's absolute path, so the links are made at the generation's final name, after its rename. Progress comes from the log, as in nx 0.x (fixtures in `testdata/`; `nix-build-storepath.jsonl` is Nix 2.35's). The log is unversioned, so the parser degrades instead of failing (1.1): an action, type or field it doesn't know is skipped, a field of another JSON type reads as absent, an `@nix` line it can't read is dropped, and a message whose `level` it can't read is classed by its text (`error:`, `warning:`); a Nix that changes the log costs the progress, not the install.
3. The generation's `lock.toml` records the store paths, and its `gcroots/` link holds them.

**What changes compared with Neptuno's Nix setup today**

| Piece | Status | Why |
| --- | --- | --- |
| `nix-daemon`, `nix.mount`, nixbld users, SELinux labels | Kept | Neptuno already ships and tests them; the daemon is the store's only writer |
| narinfo signatures, `NarHash` checks, root-owned store | Kept (in the daemon) | The real security boundary |
| `nix profile`, `~/.nix-profile` on `PATH`, the user flake registry | Dropped | nx's generations, shims and lockfile replace them; nothing from `/nix` reaches `PATH` except shims |
| Global Nix env (`XDG_DATA_DIRS`, `MANPATH` and friends from `nix.sh`/`nix.fish`) | Dropped for nx-installed tools | One ABI per process; the shim sets what each tool needs |

Plain `nix` commands keep working against the same store, as with nx 0.x. Dropping the daemon in favour of nx's own substituter is possible later (the draft's design, recorded in ADR-0006) but only if a measured problem calls for it.

**Uncached and unfree packages**

A package cache.nixos.org has no build of (a wrapper such as `neovim`) is built from its derivation (`nix build <drv>^<outputs>`), rooted in the generation, and rebuilt when the pin moves. Unfree packages are refused (nx 1.1): Hydra builds none of them, so each was a local build of a vendor download, and allowing them took a manifest allowlist, an `allowUnfreePredicate` in the evaluation and a routing branch. Extra binary caches went with them (nx 1.1): they were the only reason nxd wrote `/etc/nix` and restarted `nix-daemon`, which reads `nix.conf` only at start. The refusal points to the vendor's RPM repo (`nx repo add`, then `nx add --host`) or its Flatpak. 1.1's nxd removes the `/etc/nix/nx.conf` and `!include` 1.0 wrote, and nx warns about a manifest that still has `[tool] unfree` or `[[cache]]`; both are kept as written for an older nx after a rollback.

**Exposing binaries**

- Each exposed binary is a tiny compiled shim in the current generation's `bin/`, which a profile snippet puts on `PATH` after the system's directories (`/var/lib/nx/current/bin`), that sets per-process variables and `exec`s the real binary. Cost: about a millisecond.
- The shim sets only `TERMINFO_DIRS`, to the caller's value, then `/usr/share/terminfo`, then an empty entry (ncurses' own default): Nix's ncurses doesn't know host-only terminals such as `xterm-ghostty`. The M6 spike ran 20 common tools (ripgrep, fd, bat, jq, fzf, eza, git, gh, htop, just, helix, lazygit, starship, tmux, hyperfine, tokei, zig, yq, curl, less) from an empty environment and all worked. Nix's glibc already reads `/usr/lib/locale/locale-archive`, so `LOCALE_ARCHIVE` is unneeded, and Nix's OpenSSL finds the host's certificates through `/etc/ssl/certs`; `SSL_CERT_FILE=/etc/pki/tls/certs/ca-bundle.crt` breaks it, since Fedora 45 has no such file.
- nx never exports `LD_LIBRARY_PATH`, `PKG_CONFIG_PATH`, `XDG_DATA_DIRS`, `GIO_EXTRA_MODULES` or similar globally.
- Known limits: Nix's glibc can't load Fedora's NSS modules, so user lookups fail for systemd-homed, SSSD or LDAP accounts (plain local users are fine; `nx doctor` warns). Some Nix wrappers set env vars that child host processes inherit; `nx doctor` flags them.

**Garbage collection:** `nx gc` drops generations past retention and runs `nix store gc`; the daemon already skips paths that are live roots or in use by running processes. The timer's `nx sync --refresh` does the same once a day, so what the weekly tool upgrade leaves behind goes without anyone running `nx gc`.

**Port from nx 0.x:** the `packages.json.br` catalog builder, channel-release pinning, the parallel narinfo closure walker, and the per-package progress output.

**As built (M6)**

- *Crates.* `nx-nix` holds the Nix side: the channel and catalog (`packages.json.br`, streamed into a line file per release), the batched evaluation (ADR-0009), the narinfo closure walker, `nix` behind a trait, and the internal-json log parser (ported from nx 0.x; Nix 2.35's resumed-download retry is recognised too). `nx-shim` is the shim, std only. The CLI's `tool.rs` drives them; `route.rs` decides tiers.
- *Lock and generation.* `lock.toml` has `[tool]`: the release (rev, name, URL) and per package its attribute, `lib.getName`, version, derivation, outputs with store paths, and the commands exposed. A package locked at the same release is never evaluated again. A generation has `bin/<name>` (a hard link to one copy of the shim, under `store/shim/<sha256>`) and `shims/<name>` (the store file it runs); the shim finds its record from `/proc/self/exe`. The tool tier is the same in every image's generation, except for which names it exposes.
- *Realising.* Before a change: evaluate what is new, walk the closure of what isn't local in cache.nixos.org, then one `nix build --no-link` of store paths, or of `<drv>^<outputs>` for a package no cache has (a derivation collected since its evaluation is evaluated again first). After the generation has its final name, `nix build --out-link <generation>/gcroots/root` roots its outputs; a generation whose roots fail is not made live. `nx gc` roots kept generations that lack roots before it runs `nix store gc` (the generation `nx select` makes at boot, before the daemon is up, has none). A build that finds an output gone from the store (a plain `nix store gc` between generations) realises it again from its derivation or the cache; if that fails, the generation is built without its commands, with a warning, so the host tier still moves. `nx upgrade --tool` at an unchanged pin evaluates and realises again whatever is gone.
- *Exposure.* Computed per build, against the image's tree and the host tier's packages: a command the image or the host tier has (`usr/bin`, `usr/sbin`, `bin`, `sbin`) is not exposed, since it comes first on `PATH`. Between tool packages the first in the manifest keeps a name unless it is in `[overrides]` (`nx add --override <name>`), which gives it to the later one. `nx add` refuses a new package that loses a command to another tool package or to the image or the host tier, or that has no commands at all; `--override <name>` gives a later tool package a name an earlier one has, and `--without <name>` adds a package without a command the image or the host tier has (recorded as `[overrides] without`), a warning says so, unless that would leave it nothing to expose. A package whose every command the system already has is refused whatever the flags say, with a message that they already exist. A name the image took over since the last generation is a warning. `--dry-run` doesn't run this check: it needs the built host tier.
- *Routing.* `route::decide`, in order: a path, provide or `name@repo` → host; `--host`/`--tool`; in the image → host; plugs into the system (`nx_rpm::plugs`: `-devel`/`-static`/`-headers`/`-libs`, a toolchain command on `PATH` among its files, a `lib*` package with a soname, `pkgconfig()` provides, GPU/display/audio sonames among its requires, `systemd`) → host; a usable catalog package that isn't a toolchain, unless `routing.cli_tools = "fedora"` → tool; an RPM → host; else refuse (a toolchain with no RPM of its name, such as `gnumake` or `jdk`: say `nx search` and `--tool`; unfree: say the vendor's RPM repo or Flatpak; nothing: close names from the catalog). Toolchains are one list of commands for both tiers (`nx_rpm::plugs::TOOLCHAINS`: `go`, `gcc`, `python3.*`, `node`, `java`, `make`, …): an RPM is one if it ships such a command in `/usr/bin` (its file list, from `primary.xml`), a nixpkgs package if its `mainProgram` (else its `pname` less `-wrapper`) is one, so `go_1_23`, `python312` and `jdk21` need no name rules. `routing.toolchains` adds commands, `routing.standalone` takes them out (`zig`, for nixpkgs' copy). Each entry's reason is recorded in `lock.toml`'s `[route]` table, carried from generation to generation and pruned with the manifest; `nx why` shows it. Repo metadata is enough for the classifier, so no RPM header is fetched. A catalog name matches by attribute, then top-level pname, then `mainProgram` (`rg` → `ripgrep`). `nx info` shows the tier and the reason; `nx search` lists nixpkgs after the repos.
- *Commands.* `nx add` (`--host`, `--tool`, `--override`, `--without`), `nx remove` (either tier, by attribute or by a command it exposes), `nx upgrade [--host|--tool]` (the host tier re-resolves without preferring current builds; the tool tier moves the pin and evaluates everything), and the tool tier in `list`, `why`, `status`, `search`, `info` and `gc`.
- *PATH.* `shell/nx.sh` (profile.d) and `shell/nx.fish` (`/usr/share/fish/vendor_conf.d`: a bootc image's fish reads `/usr/etc/fish`, never `/etc/fish/conf.d`) append `/var/lib/nx/current/bin`.
- *Exit check (`tests/vm/m6.sh`).* On the VM, `nx add ripgrep` runs `rg` from an `env -i` environment, a login shell finds it on `PATH`, and the generation's root is registered in `/nix/var/nix/gcroots/auto`; `nx add ngrok` is refused, naming a vendor RPM repo or Flatpak (1.1; 1.0 allowed it with `--allow-unfree` and built it from its derivation in 107 s), `nx add neovim` builds the wrapper from its derivation and it runs; `tester`'s `nx gc` removes generations including root-built ones, runs `nix store gc`, and neovim's path survives only while a kept generation uses it; root's `nx gc` then leaves no `.trash-` entry. A tool already in the store takes about 3 s (evaluation, a generation, nxd).
- *Left for later.* The refresh timer fetching the next release's nixexprs ahead of time. (Done in M7: `nx doctor`'s checks; `nx upgrade` keeping a package the new release lacks. Dropped: `state.sqlite`, the lockfiles hold it.)

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

- On the next image digest, nx sees the package in the image's rpmdb, clears the hint, and drops any host-tier copies the image now provides.

## Tier routing and name ownership

The host tier is only for packages that plug into the system; standalone tools default to the tool tier. `nx add <pkg>` decides automatically from the repo metadata (`primary.xml`: name, provides, requires, the files in `bin` directories) and the nixpkgs catalog; `--host` and `--tool` override it, and `nx why` shows the reason.

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
3. Standalone, free, and in nixpkgs at the pin → **tool tier** (built locally when cache.nixos.org has no build; see *Uncached and unfree packages*).
4. Standalone and available from an enabled RPM repo → **host tier** (e.g. a vendor's own repo for an unfree tool).
5. Otherwise → refuse with fuzzy-match suggestions, or point to the vendor's installer.

Routing decisions and their reasons are recorded in the lockfile and cached per NEVRA or store path. `routing.cli_tools = "fedora"` in the manifest restores Fedora-first for standalone tools.

**Name mapping.** Fedora names are canonical. A nixpkgs name matches by attribute, then top-level `pname`, then `mainProgram` (`rg` → `ripgrep`); there is no alias table. `nx search` shows both sources with the source marked.

**Name ownership**

- Every exposed binary name has exactly one owner: the image, the host tier, or one tool-tier package.
- Installing a package that would expose a name already owned elsewhere is refused unless `--override <name>` (another tool package's name) or `--without <name>` (the image's or the host tier's) is passed. Both are recorded in the manifest's `[overrides]`.
- Within the host tier, ownership is enforced by RPM file conflicts as normal.
- `nx why <name>` prints the owner, tier, source, version, and which generation added it.

## Repositories and COPR

The host tier accepts COPR and other third-party RPM repos through the same pipeline as Fedora's own. Adding a repo is always explicit, its key is pinned, and by default Fedora wins name collisions.

**Adding repos**

- `nx repo add copr:owner/project` expands to the COPR URL for the booted release and architecture, e.g. `https://download.copr.fedorainfracloud.org/results/<owner>/<project>/fedora-$releasever-$basearch/`, and the project key at its `pubkey.gpg`.
- `nx repo add <url>` adds any other RPM repo (VS Code, Tailscale, etc.).
- Repos the image already enables in `/etc/yum.repos.d` are imported read-only and marked as image-provided. nx never edits those files.
- `nx repo list` / `nx repo remove` manage them. Repos are recorded in `manifest.toml`; the metadata checksum and key fingerprint of each go in `lock.toml`. An added repo's key is pinned by content, `/var/lib/nx/keys/<sha256>.asc`, and `lock.toml` names it (`key`): a pin is never replaced, `nx repo remove` leaves it for the generations that still name it (a rollback verifies with the key it was built with), and `nx gc` removes the ones no kept generation names. A pin is checked against its name when read.

**Pipeline:** identical to Fedora's. repomd → cached `.solv` → libsolv → classification from the metadata → host tier. nx reads the primary metadata whole (gzip, xz or zstd), never zchunk.

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
| `ldconfig` | Nothing (paths are standard in the merged `/usr`) |
| Cache triggers (`glib-compile-schemas`, `update-desktop-database`, `gtk-update-icon-cache`, `fc-cache`) | Run after refresh as post-activation hooks |
| `update-alternatives`, edits to existing `/etc` files, `semodule`, `dracut`, `depmod` | Refuse with an image hint |

**Implementation notes**

- Most Fedora scriptlets come from a small set of macros, so match on the expanded macro bodies (`%systemd_post`, `%sysusers_create_compat`, `%tmpfiles_create`). Keep a test corpus of real scriptlets from popular packages.
- Ship a per-package allowlist override file so a user can mark a scriptlet as "safe to ignore" after reviewing it.
- Units shipped in `/usr/lib/systemd/{system,user}` work as normal after the refresh and a daemon-reload. Enabling writes symlinks in `/etc`, which is writable on bootc.

**As built (M4)**

- *Where it lives.* `nx-scriptlet` is a pure function from a package's scripts (read from the signed header through `headerFormat`, the same text `rpm --qf` prints, and stored beside the unpacked package as `.nx-meta.json`) to *intents* (preset, remove, restart, sysusers, skipped) plus *leftovers*. The corpus is `tests/fixtures/scriptlets/`, recorded from 23 real Fedora 45 packages by `record.sh`, never written by hand.
- *How it matches.* A body is normalised (continuation lines joined, comments and `|| :` and `>/dev/null 2>&1` noise dropped), known blocks are consumed, and whatever remains is a leftover. The `%systemd_*` macros are matched by their guard (`$1 -eq 1` install, `-eq 0` remove, `-ge 1` upgrade): a preset needs install in its guard, a removal exactly remove, a restart upgrade. Also known: `ldconfig`, `exit 0`, cache commands (skipped, and noted), and `getent … || groupadd/useradd`, which becomes a sysusers line. A shipped `sysusers.d` file is already declarative. Unit names must be plain `*.service`-style names (or `foo@.service` templates); nothing else is accepted.
- *Triggers.* A trigger that can't fire is inert: `in`/`prein` fire only if the target is present (image or generation), `un`/`postun` only if it is in the generation (nx never removes an image package). File triggers are matched against the idioms above.
- *Refusal and the allowlist.* A package with leftovers is refused: the message prints them, the `RUN dnf install -y …` line for the image, and `--allow-scriptlets package=digest` (a sha256 of the leftover text, 16 or more hex digits; `ALLOW_DIGEST_LEN`). An allowance is stored in the manifest (`[[scriptlets]]`) and dropped when the package leaves. The skipped scriptlet is reported on every add that uses it.
- *`/etc` and `/var` content.* Files outside `/usr` go to `usr/share/factory/{etc,var}`, with a generated `usr/lib/tmpfiles.d/nx-<package>.conf` (`C` for files, `d` for directories, `L` for symlinks, ghosts skipped), so nothing in `/etc` is ever overwritten and what a user edited stays. Anything else (`/opt`, …) is reported and left out. `/bin`, `/sbin`, `/lib` and `/lib64` are moved under `/usr` first.
- *Activation.* Each generation carries `activation.toml`: per package its nevra, intents, and sysusers and tmpfiles files. nxd diffs the old merged generation against the new one in the single `Activate` request: a new package gets `systemd-sysusers`, `systemd-tmpfiles --create`, `systemctl preset` and a start of units that are then enabled; a removed one gets stop and disable (instances of `foo@.service` included, since `disable` takes no globs) before the refresh; a package whose version changed gets `try-restart`. `preset --now` is not valid in systemd 262, hence preset, `is-enabled`, `start`. nxd only acts on units the extension actually ships, whatever the client says. The CLI prints each step with `⚙`.
- *Known limits.* User-scope template units (`foo@.service` under `usr/lib/systemd/user`) are disabled by name, not per instance. A merge that fails after the stop leaves removed packages' units stopped until the next activation. nxd looks wheel members up with `id` and `getent` under `RestrictAddressFamilies=AF_UNIX`, which suits SSSD (a unix socket) but not an NSS module that opens its own network connection. The sysusers and tmpfiles files nxd applies are package content: a wheel user already has the right to activate anything, so they are checked by name, not by content.

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
    activation.toml            # units, users and files its packages need
  keys/<sha256>.asc            # added repos' keys, pinned by content
  sync-failures.json, upgrade-failure.json   # what the background jobs couldn't do
  cache/                       # shared by administrators (else ~/.cache/nx)
    solv/<repo>-<checksum>.solv
    solv/image-<image-digest>.solv
    repodata/<repo>/           # repomd.xml and primary metadata
    packages/, koji/           # downloads
    nix/                       # one catalog per nixpkgs release
/var/lib/nx-merged/<N>.<pid>/  # root's copy of the merged generation's sysext/
/var/lib/extensions/nx -> /var/lib/nx-merged/<N>.<pid>
# PATH ends in /var/lib/nx/current/bin (/etc/profile.d/nx.sh, vendor_conf.d/nx.fish)
```

**Manifest** (`manifest.toml`, user intent, safe to edit and commit to dotfiles):

```toml
[host]
packages = ["wayland-devel", "libxkbcommon-devel", "alsa-lib-devel", "syncthing"]

[tool]
packages = ["ripgrep", "neovim", "zig"]

[routing]
cli_tools = "nix"        # or "fedora"
standalone = ["zig"]     # a toolchain the tool tier may have
toolchains = []          # commands to keep in Fedora besides nx's list

[overrides]
names = []

[gc]
keep = 5                 # newest generations kept besides the deployed images'
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
3. Build `image-<staged-digest>.solv` from the staged deployment's rpmdb, read in place at `/sysroot/ostree/deploy/<osname>/deploy/<checksum>.<serial>/usr/lib/sysimage/rpm` (readable unprivileged).
4. Re-solve the manifest against it and assemble a new generation tagged with the staged digest. Packages now provided by the image are dropped from the host tier automatically.
5. If anything fails to resolve, keep going with the rest and record the failures for `nx status`. Send a desktop notification.
6. The tool tier is carried over as it is: it doesn't depend on the image.

**At boot**

- `nx-select.service` runs `Before=systemd-sysext.service`. It reads the booted digest and points `current` at the newest generation built for it.
- If none exists (the staging step failed or was skipped), it runs the compatibility check from \*Updates and edge cases\* on the previous generation and merges it if it passes; otherwise it points at an empty generation so the host tier simply isn't merged. The system still boots cleanly; nx resolves in the background and refreshes live when done.
- Old generations for digests that are no longer deployed are garbage-collected.

**Partly answered (2026-11-19):** on Neptuno the rollback deployment's rpmdb opens unprivileged in place (`rpm --dbpath <deployment>/usr/lib/sysimage/rpm`). Still to confirm in M5: that a *staged* deployment's tree exists under `/sysroot/ostree/deploy` before reboot (bootc may finalize it only at shutdown); if not, fall back to pulling the image with podman. The boot-time fallback above makes this non-fatal.

**As built (M5)**

- *Who does what.* `nx select` (hidden; `nx-select.service`, `Before=systemd-sysext.service`) runs at boot as root and reads only local state. `nx sync` (`nx-sync.service`, wanted by `multi-user.target` and by bootc's `bootc-status-updated.target`) gives every deployed image a generation. `nx-refresh.timer` runs `nx sync --refresh` every 6 hours, which fills the shared metadata cache first. `nx gc` removes what nothing needs. `nx rollback` and `nx status` know about other images.
- *Selecting at boot.* The booted deployment comes from the kernel's `ostree=` argument (a symlink to the deployment) and its image digest from the commit's `ostree.manifest-digest`, read with `ostree show`: rpm-ostree's daemon isn't up that early. `current` goes to the newest generation whose `lock.toml` names that digest. With none, the candidate is the generation that was current, and `compat::check` decides (below). If it fits, it is merged and `nx sync` resolves for the new image once the machine is up. If not, an empty generation (same manifest, no packages) for the booted digest becomes current, so nothing is merged and boot is clean. That generation is marked `pending = true` in its lockfile, so `nx sync` treats it as no host tier and resolves for the image. If `nx select` itself fails (the digest can't be read, the lock can't be taken) it removes `current` and merges nothing: a generation for another image is never merged unchecked.
- *The compatibility check (invariant 2).* Same `VERSION_ID` and architecture; no file of the extension hides one of the image's (`check_extension` against the booted deployment's tree, not `/`); and solving the generation's manifest against this image's rpmdb, from the metadata already cached (`Refresh::Never`, no network), with its locked builds preferred, yields no package the generation doesn't have. Image relatives must be the image's exact build, so a `-devel` whose library the image moved past fails here. A repo with no cached metadata, or a relative only Koji has, fails closed. The same check lets `nx rollback N` cross image digests; it names the reason when it refuses.
- *Re-resolving before the reboot.* `Machine::targeting(deployment)` points the resolver at a deployment's own tree under `/sysroot/ostree/deploy/<osname>/deploy/<checksum>.<serial>`: its repo files, os-release, rpmdb and keys, and its digest. `nx sync` builds a generation for the staged and the rollback image that way (`Session::build`: everything `nx add` does except making it live). Only the booted image's generation is ever activated, and only if `current` hasn't changed since the sync started (a concurrent `nx add` wins). A generation counts as up to date when the newest one for that digest has the current generation's manifest and an nx no older than `select::REBUILD_BEFORE` built it (1.1: `lock.toml` records its `builder`; one without it is nx 1.0's). The floor is raised whenever a bug makes older generations suspect: 1.0.0's bootc hook built the staged image's generation from the booted image's rpmdb, and the next boot trusted it, same digest and same manifest. `nx sync --rebuild` rebuilds every image's by hand. A rebuild that fails leaves the image the generation it has; the failure goes to `rebuild-failures.json` (by digest, like `sync-failures.json`), so `nx status`, `nx doctor` and a notification show it, and `nx sync` exits non-zero. If one package of the manifest can't be resolved for an image (a COPR with no build for the new release), that image gets a generation without it (*Partial re-resolve*, below). If nothing can be built for an image (the failure isn't one package's), it gets none, the others still do, and the failure is a warning and a non-zero exit; `nx status` shows the reason.
- *Every change reaches every image (invariant 6).* `Session::install`, which `add`, `remove` and `repo` share, runs the same sync after the change is live, best effort.
- *GC (invariant 5).* `nx gc`, which `nx sync` also runs after every image has a generation, keeps the current generation and its parent (where `nx rollback` goes), the newest generation for each deployed digest (booted, staged, rollback), the newest few (`[gc] keep` in the manifest, 5 by default), and any a newer nx wrote; it drops the rest and the store entries only they used. On `nx sync --refresh` (the refresh timer) it also runs `nix store gc`, at most once a day (the `nix-gc` stamp in the state directory; it walks the whole store), after rooting kept generations that lack roots; a failure there is a warning. If the deployments can't be read on a bootc system it refuses rather than guess. Holding the state lock, it also removes the `.N.part` and `.<sha256>.part` staging directories an interrupted build left (1.1): no build runs while it holds the lock. `--dry-run` lists what it would remove.
- *Left for later.* Desktop notifications (the root service has no session to notify; `nx status` and the journal carry the failures); the compatibility check compares exact builds, so it is conservative (a generation whose packages the repos have since moved past is judged unfit even if the image would run it); several updates staged before one reboot need nothing special (only one deployment can be staged).
- *Partial re-resolve.* If the manifest doesn't resolve for an image as a whole, `nx sync` resolves each package alone, leaves out the ones that fail (with a warning) and builds the rest, recording them as `skipped` in `lock.toml`. Better a host tier without one package than none, which would break invariant 5. `nx status` shows `without <pkgs>`. A generation with `skipped` is retried only on `nx sync --refresh` (the daily timer), and a retry that resolves to the same packages and the same skipped list builds nothing (it compares packages, the skipped list and the manifest). If no one package is at fault (two conflict), the error stands and the image has no host tier. Why an image has none is kept in `sync-failures.json` in the state directory, by digest, and `nx status` prints it.

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
| nx itself | releases | an image update (nx ships in it; no self-update) | Newer formats are read; an older nx reads the newest snapshot it can (invariant 9) |

**Invariants** (every code path must keep these; write tests for each)

1. **No overlap with the image.** A generation is never merged if any of its files also exists in the booted image's `/usr`. An overlay above `/usr` would silently shadow the image's newer files with older ones.
2. **Generations are bound to a digest.** A generation built for another digest is merged only if a compatibility check passes against the booted image: same `VERSION_ID`, every host-tier package's requirements satisfied by the booted rpmdb, and invariant 1.
3. **Exact builds for image relatives.** A host-tier package built from the same source package as something in the image (typically `-devel`, `-libs`, `-static`) uses that exact build (same NEVR), from the repos if still present, otherwise from Koji (`kojipkgs.fedoraproject.org`), verified against Fedora keys. Never a newer build that would require upgrading the image's copy.
4. **Image packages are never upgraded or replaced.** If a package needs a newer version of something in the image, nx refuses and says to update the image.
5. **Every deployed digest keeps a generation.** Booted, staged and rollback deployments each retain a matching generation; GC never removes them.
6. **Every manifest change resolves for all deployed digests.** `nx add` while an update is staged rebuilds both the current and the staged generation.
7. **Boot never needs the network.** Generation selection uses only local state.
8. **Manifest is intent, not state.** A package the image now provides stays in the manifest, marked image-provided, so it comes back if the image drops it.
9. **State is versioned.** The manifest, lockfile and `activation.toml` carry a version. An older nx facing newer state falls back to the newest snapshot it can read, else goes read-only.

**Default update policy (as built)**

- `nx-refresh.timer` runs `nx sync --refresh` every 6 hours (1.1; daily before, so nearly every command warned that `updates`, with `metadata_expire=6h`, was out of date): fresh repo metadata, a generation for every deployed image, `nx gc` and `nix store gc`. It never moves a package past what the manifest's resolve gives.
- `nx-upgrade.timer` runs `nx upgrade --tool` weekly: the nixpkgs pin moves; a package the new release lacks keeps its build.
- `nx upgrade` applies host-tier leaf upgrades (packages unrelated to the image) and tool-tier upgrades. `--host` / `--tool` restrict it. Image relatives never move here; they move with the image.
- What the background jobs couldn't do reaches the session as a notification (`nx-notify.path`, `nx notify`) and `nx doctor`.
- Image updates need no nx command: the staged-image hook and `nx-select` handle them.
- Not built: security advisories (`updateinfo.xml`) and applying them automatically, and notifications about stale tools.

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
| `updates` has a newer build of a host-tier leaf | None, normal upgrade | Applied on `nx upgrade` | Notification, then the normal upgrade output |
| `updates` has a newer build of a library that is in the image | Newest `-devel` no longer matches the image's library | Invariant 3: keep the exact image build of the relatives; fetch from Koji if the repo dropped it | Nothing; `-devel` always matches |
| A new leaf needs a newer library than the image has | Would require upgrading the image's copy | Invariant 4: refuse | "foo needs libbar ≥ 2.1, image has 2.0: update your image" |
| A host-tier package is retired from the repos | No future updates | Keep the installed build from the store; mark orphaned; exclude it on the next release change if unresolvable | `nx status`: "orphaned" |
| Metadata stale or offline | Cannot fetch | Everything local still works; `add` of an uncached package fails cleanly; boot unaffected (invariant 7) | Clear offline error |
| Mirror out of sync (metadata newer than packages) | Checksum or 404 failures | Retry on another mirror; verify checksums from metadata | Nothing, or a retry message |
| Fedora release reaches EOL while the image stays on it | No more updates; repos move to the archive | Warn; point repo URLs at the archive so resolves keep working | Persistent warning in `nx status` |
| User runs `nx upgrade` and later `bootc upgrade` | Host-tier leaves newer than what the new image expects | Re-resolve for the staged digest re-checks everything; invariant 4 refuses anything that would need image changes | Nothing in the common case |

### nx itself

nx ships inside the user's image, installed by the user's own Containerfile. nx still never modifies the image; it just lives there.

**Packaging (decision, M7):** a release tarball per Fedora release and architecture that an image unpacks at `/` (the binaries, units and presets, sysusers, tmpfiles, polkit, PATH snippets, completions, man pages; *Milestones*, M7). Nothing for `/nix`: the tool tier needs the image's Nix (`nix-core`, `nix.mount`, the daemon), which Neptuno already ships; without it the tool tier is off with a hint.

On Neptuno the units are declared in `build/packages/*.toml` like every other unit ("declare, then enable"); `20-overlay.sh` unpacks the tarball, pinned by `SHA256SUMS`.

**Consequence:** nx's version changes only when the image changes. CLI and `nxd` always match, and there is no self-update. The hard case becomes `bootc rollback`: an older nx boots against state a newer nx already migrated.

| Scenario | Risk | nx behavior | User sees |
| --- | --- | --- | --- |
| Image update brings a newer nx | State format changed | Migrate the manifest / lockfile / generation format at first run after boot (or in `nx-select`), after snapshotting the old state | One-line "migrated state to v3" |
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
| Pin moves while uncached tools are installed | Their store paths change; not in any cache | Build each again from its derivation; if one fails, keep the old build in the generation and report it | Slightly longer upgrade; a note if a rebuild failed |
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
| Overlay stacking depth exceeded on a new kernel | Sysext won't merge | The next activation fails with `systemd-sysext`'s reason and puts the previous generation back; at boot nothing merges | `nx doctor` fails "the host tier isn't merged over /usr" and points to the journal |

### Fallback telemetry (1.1)

Each fallback writes one journal entry with its own `MESSAGE_ID` (`nx_core::journal`), so `journalctl MESSAGE_ID=<id>` (or `journalctl NX_FALLBACK=koji`) says how often it runs on a real machine, and a fallback that never does can go. nx writes to journald's native socket directly (`KEY=value` datagrams, no libsystemd); a missing journal loses the entry and nothing else. The CLI logs as `nx`, nxd as `nxd`, at priority notice.

| `NX_FALLBACK` | `MESSAGE_ID` | When |
| --- | --- | --- |
| `koji` | `44f21298d15a4e8a92ad200912912dd7` | An image relative's exact build is downloaded from Koji: no repo has it |
| `partial-resolve` | `13b86f26dcdf40a68ab4002ee34da519` | `nx sync`: the manifest didn't resolve whole, so packages were left out |
| `local-build` | `52cf727a75204b31aea91b293bded854` | A tool is built from its derivation: no binary cache has it |
| `other-mirror` | `397e4099b3774e0c8aa3144b7392f21a` | A repo's metadata came from a mirror after the first, or a package from a mirror other than its metadata's |
| `cached-metadata` | `67553b5c473a4661952755ca9d659412` | Every mirror of a repo failed, so its cached metadata was used |
| `merge-failed` | `e7aa490fdaaf491a9c62c78cd738a59b` | nxd's `systemd-sysext refresh` failed or merged nothing, and the previous generation was put back |

### Open questions for updates

- [ ] Do Koji's signed copies (`data/signed/<key>/`) exist for every build nx might need, and how should nx verify one that only exists unsigned? *Partly answered (M2):* they exist under the key's short ID (`f577861e` for Fedora 45) for the mesa builds tested; nx tries each of the repo's keys and refuses anything that doesn't verify, so an unsigned-only build is refused for now.
- [ ] How long does the boot-time compatibility check take for a realistic host tier? It must stay well under a second.
- [ ] Should the pre-reboot notification also be written somewhere `bootc status` users will see it?

## Privilege model

The `nx` CLI runs unprivileged. A small root daemon, `nxd`, does only the steps that need root, so there are no sudo prompts in normal use.

**nxd**

- Socket-activated systemd service on a Unix socket, one process per connection (`nxd.socket` with `Accept=yes` starts `nxd@.service` with the connection on stdin), so a request is handled in a fresh process that exits when it answers. A lock serializes concurrent requests. The unit uses no mount-namespace options (`ProtectSystem=`, `PrivateTmp=`): the merge would be invisible outside it.
- Exposes a narrow, typed API, one JSON line each way (`nx-proto`): `Activate { generation }` (M3, and M4 too: sysusers, tmpfiles and units are derived by nxd from the generation's own `activation.toml`, so no request names a unit or a path). The client names a generation by number and never sends a path.
- Never downloads, resolves or parses packages itself. The unprivileged CLI does that work and writes into store directories owned by a dedicated `nx` system user; nxd validates that a generation is complete and correctly owned before activating it.
- Authorization via polkit: the active local session user in `wheel` may call it without a password. Everyone else needs admin auth.

**Decisions for M3 (best practice, checked in the VM 2026-11-19)**

- *Extension link.* `/var/lib/extensions/nx -> /var/lib/nx/current/sysext`, created by `tmpfiles.d`. `man systemd-sysext` names `/var/lib/extensions` the primary place to install extensions and `/run` and `/etc` suitable only for symlinks to them; only `/var/lib/extensions` is merged at boot without extra setup, and the link name `nx` is what matches `extension-release.nx`. On Neptuno the link merged, kept `usr_t` labels, and was merged again after a reboot. `setfiles -r` wants the resolved directory (`realpath`), not a path through `current`. *Since the 1.0 audit* the link names root's copy instead: see *Root's copy*, below.
- *Root's copy (1.0 audit).* The CLI unpacks as its user and a generation hardlinks the store, so the files of `generations/<N>/sysext` belong to whoever built them, and before this the merged `/usr` was those very inodes: anything running as that user (not only a person at an active session, which is all polkit checks) could rewrite a binary that root's services run, with no prompt. nxd now copies the generation's tree into `/var/lib/nx-merged/<N>.<pid>` (root's, 0755, outside the wheel-writable state directory), reflinked (`FICLONE`; btrfs on Neptuno, so it costs no space), and merges the copy: files are root's, modes lose group and other write, directories lose setgid, ACLs aren't copied. The checks run as the copy is made (each file opened `O_NOFOLLOW` and checked on its descriptor; invariant 1 and the release on the copy), so what was checked is what merges. Only the merged copy is kept; a failed merge puts the link back and drops the new one. Chowning the store in place was not an option: `fs.protected_hardlinks` would then stop the next build from hardlinking it. At boot `nx select` stages the generation it picks the same way (`nx_activate::stage_at_boot`, labelled, no refresh: `systemd-sysext.service` merges it next), and when it can't tell which fits it removes the copies so the link names nothing. Cost: an activation went from 80–150 ms to about 240 ms on the VM, and a boot whose generation changed takes about 2 s in `nx select` (the copy and `setfiles`). Not covered: root's `nx sync`/`nx upgrade` still read caches and lockfiles wheel can write (a libsolv or librpm bug would be needed).
- *The `nx` user.* A system user from `sysusers.d` (`u!`, no login shell, home `/var/lib/nx`) owns the state directory. The background services (`nx-sync`, `nx-refresh`, `nx-upgrade`) run as root in 1.0, which is why root-built generations wheel can't delete go to `.trash-*` for root's next gc; running them as `nx` waits for an `nx`-user builder (*Why not unpack as `nx`*), since alone it would only move that problem. `tmpfiles.d` creates `/var/lib/nx` setgid with a default ACL for group `wheel`, so interactive wheel users, who may call nxd without a password, write generations as themselves without being members of a group nx has to manage. nxd accepts a generation whose files are owned by root, the owner of the state directory (`nx`) or the calling uid (`SO_PEERCRED`). It refuses setuid and setgid files, file capabilities, device nodes, FIFOs and sockets, and files writable by everyone, and an extension-release that doesn't match the running system. It never follows a symlink: a link in a package is just a link in the merged `/usr`. It does not use `DynamicUser=`: state under `/var/lib/nx` must outlive the service and be owned by a stable uid.
- *No wider trust.* A wheel user can already activate without a password, so a tree they own widens nothing for them. A user outside wheel is refused by polkit before nxd opens the tree, so what they own never reaches `/usr`. What reaches `/usr` is root's copy (*Root's copy*, above): owning the generation gives no write access to what is merged.

- *Several administrators.* nxd also accepts files owned by any member of group `wheel` (`/etc/group` and `/etc/passwd` first, then `getent group wheel` and `id -G <uid>`, so accounts that exist only in SSSD or LDAP count; answers are cached per request), so one administrator can activate generations that link packages another unpacked. The state lock opens read-only when the file belongs to someone else but the directory is writable, since flock needs no write access.
- *Why not unpack as `nx`.* Having a service running as `nx` do the downloads and unpacking would make every tree nx-owned and unwritable by users, closing the window between nxd's check and the merge (a wheel user can chmod files they own, which they could do to `/usr` anyway through nxd). Root's copy closes that window and the write access to `/usr`; what an `nx`-user builder would still add is that root's background services no longer parse wheel-writable caches, and one owner for the store and generations (no `.trash-*`). It moves resolving and unpacking out of the CLI, so it is the direction after 1.0, not part of it.

**Who may do what.** The packages are one per machine, not per user: one manifest, one set of generations, one `current`.

| Who | Can |
| --- | --- |
| Anyone | `list`, `why`, `generations`, `status`, `shell`, `search`, `info`, `add --dry-run`, `remove --dry-run`, `rollback --dry-run` (search, info and dry runs fetch repo metadata into the shared cache, or their own `~/.cache/nx` if they can't write it) |
| Administrators (group `wheel`) | everything that changes the packages. They write `/var/lib/nx` through the ACL, and nxd lets an active local session of one activate without a password. Over ssh, polkit asks for authentication instead |
| Everyone else | is told changing packages needs an administrator, and nxd refuses them anyway (polkit) |

Repo metadata is cached in `/var/lib/nx/cache`, which administrators share (`nx.tmpfiles` gives it the same wheel ACL), so a second administrator doesn't download it again. Someone who can't write it keeps `~/.cache/nx`. A poisoned shared cache can't install anything unsigned: package signatures and checksums are checked again, so the worst is a failed or stale solve. The refresh timer (M5) will fill it.

Every merge also checks again that no file of the extension hides one of the booted image's (invariant 1), from the booted deployment under `/sysroot`: a rollback after the image changed is where it matters, and a bootc system whose deployment can't be read is refused rather than merged unchecked.

**Trust boundaries**

- RPMs (repos and Koji): GPG-verified against Fedora keys from the image before unpacking; third-party repos against their pinned keys.
- Nix store paths: written only by `nix-daemon`, which verifies narinfo signatures against cache.nixos.org's key, and every NAR against its `NarHash`. Locally built packages are trusted via the pinned nixpkgs revision and the source hashes it fixes.
- Generation contents: files owned by root in the final assembled tree; no setuid/setgid bits allowed from the host tier unless the RPM ships them and the user allowlists the package.
- Scriptlets are never executed (see *Scriptlet translation*).

## Performance design

nx should feel instant for reads and take about a second plus download time for installs. It gets there by never waiting on metadata in the foreground, evaluating Nix only once per new attribute and pin, and caching everything tied to the immutable image.

| Target (estimate) | Operation |
| --- | --- |
| < 100 ms | `list`, `why`, `search`, `info`, `status` (lockfiles and cached metadata only) |
| \~1 s + download | `add` of a host-tier package with warm metadata |
| \~1 s + download | `add` of a tool-tier package new to this pin (one evaluation); near-instant when the lockfile has it |
| measure first | `systemd-sysext refresh` on the target image (the slowest remaining step) |

**Techniques**

1. **Background metadata refresh.** A systemd timer refreshes repo metadata and the nixpkgs index every 6 hours. Reads (`search`, `info`, `list`, `why`) use the cache and mention it only when it is more than a day old or the last refresh failed (a fetch that falls back to the cache leaves `last-error` beside it until one works). A change (`add`, `remove`, `upgrade`, `apply`, `repo`) refreshes a repo past its `metadata_expire` (`Refresh::IfStale`): one `repomd.xml` fetch, and `primary` only when it changed; offline it uses the cache with a warning (1.1). `--refresh` forces it. Never auto-update in the foreground like brew.
2. **Primary metadata only.** `filelists` is never fetched: the classifier needs only the files `primary.xml` lists. zchunk isn't used; a refresh downloads a repo's primary when it changed.
3. **Precomputed installed set.** The image's rpmdb is turned into `image-<digest>.solv` once per image digest.
4. **libsolv directly** (the solver dnf5 uses), loading cached `.solv` files. No Python, no dnf process.
5. **Nix evaluation only for new attributes.** Search and info use the prebuilt catalog; store paths are evaluated once per attribute and pin and kept in the lockfile; closures come from `.narinfo` references.
6. **Parallel I/O.** Four package downloads at once, each streamed to disk and hashed on the way; mirrors in the metalink's order, the next on a failure.
7. **Cheap generation assembly.** Reflinks (btrfs/xfs) or hardlinks from the store; directory sysext, so no squashfs/erofs build.
8. **Skip work.** Evaluate only new attributes; download only what the store lacks. Every activation copies (reflinked), labels and refreshes the extension, a tool-only change too: about 240 ms on the VM, so not worth a second path.
9. **Startup.** One Rust binary, no runtime to boot, no network unless the command needs it.

## CLI surface

The verbs mirror brew. Every mutating command prints the tier and source it chose, and supports `--dry-run`.

| Command | Does |
| --- | --- |
| `nx add <pkg> [--host\|--tool] [--override <name>] [--without <name>]` | Install; routed automatically; live in seconds. Image-only packages get a hint instead |
| `nx remove <pkg>` | Uninstall; stops and disables its units first |
| `nx upgrade [--host\|--tool] [pkg]` | Host-tier leaf upgrades and/or moving the nixpkgs pin; new generation |
| `nx search <term>` | Search Fedora, third-party repos and the nixpkgs catalog, source marked |
| `nx info <pkg>` | Version, tier, source, routing reason, files, dependencies |
| `nx list` (`ls`) | Installed packages by tier |
| `nx why <name\|path>` | Owner of a binary or file, its routing reason, and the generation that added it |
| `nx repo add\|list\|remove` | Manage COPR and third-party RPM repos. `add` takes `copr:owner/project`, a `.repo` file's URL, or a base URL with `--key`; `--priority` sets the dnf priority; `--new-key` replaces a pinned key after a rotation |
| `nx apply` | Make the system match `manifest.toml` |
| `nx generations` / `nx rollback [N]` | List / switch generations |
| `nx status` | Image digest, generations, pending re-resolves, failures, outstanding image hints |
| `nx doctor` | Health checks (below) |
| `nx sync [--refresh] [--rebuild]` | Give every deployed image (booted, staged, rollback) a generation that matches the manifest; the staged-image hook, boot and the refresh timer run it |
| `nx gc` | Remove generations nothing deployed needs, and the store content only they used |

**`nx doctor` checks**

- ELF scan of every exposed binary: `NEEDED` libraries, `RUNPATH`/`RPATH`, and glibc symbol versions. Flags any binary that would load libraries from both `/usr` and `/nix/store`.
- Environment leaks: `LD_LIBRARY_PATH`, `PKG_CONFIG_PATH` or `XDG_DATA_DIRS` entries pointing into `/nix` in the user's shell, and Nix wrappers that set them for children.
- Name collisions and active overrides.
- Sysext merge status, overlay stacking depth headroom, SELinux labels on the extension.
- Lockfile vs. booted image digest mismatch.

*As built (M7):* everything above except the ELF scan and overlay headroom, which are 1.0's known limits: the variables that make a program mix Nix's libraries with the host's (`LD_LIBRARY_PATH`, `LIBRARY_PATH`, `PKG_CONFIG_PATH`, `CPATH`, `LD_PRELOAD` pointing into `/nix` or a Nix profile) are checked instead, since nixpkgs binaries are patched to their own loader and only those variables reach past it. Also checked: nxd's socket, the Nix daemon's, tool paths gone from the store, the weekly upgrade's and the last sync's failures, and `PATH` order. A failure exits 1; warnings alone exit 0.

## Implementation: Rust, crates, layout

nx is written in Rust as one Cargo workspace. Rust was chosen because the namespace/mount syscalls, libsolv FFI and the bootc/composefs ecosystem are all first-class there, while Go's multithreaded runtime makes namespace work awkward and cgo complicates static builds.

**Workspace layout** (AGENTS.md *Layout* has the detail)

```
nx/
  crates/
    nx-cli/        # nx: clap, output, every command; the Session pipeline
    nxd/           # root daemon: authorization, one request per process
    nx-activate/   # what nxd runs: validate and copy, label, merge, units
    nx-core/       # state, store, generations, manifest, lockfile, deployments
    nx-rpm/        # repos, metadata, the libsolv universe, downloads, keys, Koji
    nx-solv/       # safe libsolv (bindgen)
    nx-librpm/     # safe librpm: keys, signatures, payloads (bindgen)
    nx-scriptlet/  # scriptlet translator and tmpfiles planner
    nx-nix/        # channel, catalog, evaluation, narinfo, nix, its log
    nx-http/       # the HTTP client and retry policy
    nx-proto/      # CLI <-> nxd protocol
    nx-shim/       # the tool-tier shim
  systemd/, polkit/, shell/   # what an image installs
  tests/vm/        # milestone checks in a bootc VM
```

**Linking.** The binaries link dynamically against the image's glibc and libsolv (`libsolv` is already in the image through dnf5/rpm-ostree; build against `libsolv-devel` in a `fedora:<release>` container). nx then shares the host ABI like any other host-tier program, and releases are built per Fedora release instead of as static tarballs ([ADR-0006](adr/0006-rust-rebuild-two-tiers.md) amends ADR-0005).

**Dependencies.** clap, serde and toml, ureq (rustls, the platform verifier), rustix, thiserror and anyhow, sha2, quick-xml, brotli-decompressor; libsolv and librpm from the image through bindgen (ADR-0007, ADR-0008). No async runtime, no SQLite, no ELF parser.

**Conventions:** AGENTS.md (gates, output, pitfalls).

## Risks and open questions

The biggest risk was the sysext merge on bootc. M0 through 1.0 showed it works on Neptuno, so 1.1 dropped the prefix fallback (ADR-0011).

| Risk | Evidence | Mitigation |
| --- | --- | --- |
| sysext/confext support on bootc is distro-dependent | The [bootc filesystem docs](https://bootc.dev/bootc/bootc-filesystem.7.html) say support depends mostly on the distribution, and a bootc PR documented them as unsupported ([Fedora Discussion](https://discussion.fedoraproject.org/t/unsupported-systemd-confext-and-sysext-on-bootc/202199)) | M0 measured it on the image; a failed merge is an error, and `nx doctor` checks the merge |
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
- [x] Best way to read a staged deployment's rpmdb before reboot (M5): `bootc switch` and `bootc upgrade` write the staged deployment's whole tree to `/sysroot/ostree/deploy/<osname>/deploy/<checksum>.<serial>` at once, rpmdb included, readable unprivileged; no podman pull is needed. bootc's `bootc-status-updated.target` fires `nx-sync.service` as soon as it is staged: on the VM the host tier for the staged image was built in 2 s, before any reboot.
- [ ] Whether to use `ID`+`VERSION_ID` or `SYSEXT_LEVEL` in extension-release, given re-resolution happens per digest anyway.
- [ ] After an upgrade, list processes still holding replaced files (`needs-restarting`); M4 does `try-restart` only.

**Open questions for the tool tier**

- [x] How many top-level attributes map ambiguously when joining `packages.json.br` with `store-paths.xz`? Nearly all (M6 spike, nixpkgs-26.11pre1122334): of 133,489 free, unbroken attributes, 1,143 match exactly one path, 73,090 match several (the file covers x86_64-linux, aarch64-linux and aarch64-darwin, and narinfos carry no `System`), 59,256 match none (store names differ from catalog names, or Hydra doesn't build the attribute). nx evaluates instead ([ADR-0009](adr/0009-store-paths-by-evaluation.md)).
- [x] Does realising a store path the local store has never seen substitute cleanly? Yes: `nix build <path> --out-link <link>` as an unprivileged wheel user, through the daemon, with no derivation known locally, substituted hyperfine and registered the root (1.0 s with its dependencies present; 31 ms when already valid). A path no substituter has fails with "is required, but there is no substituter that can build it".
- [x] Can nxd add extra substituters without restarting a daemon that is mid-build? No restart-free way: the daemon reads `nix.conf` at start, and an untrusted client's `extra-substituters` is ignored until the daemon lists it in `trusted-substituters`. A restart is cheap and safe (11 ms, `KillMode=process`), and it is needed only when a cache is added or removed.
- [x] Which shim environment variables are actually needed? Only `TERMINFO_DIRS` (see *Exposing binaries*).

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

## Milestones

All done: 1.0 is M0 to M7, kept here for their results and the decisions recorded with them. They were built in this order, riskiest first, each ending with a working, tested command. Spikes and activations still run in a bootc VM built from the Neptuno image, never on the daily driver.

1. **M0 — Spike: activation on the real image.** Hand-build a directory sysext containing one `-devel` package (`spike/m0-sysext.sh`); measure `merge`/`refresh` time; confirm SELinux labels and overlay headroom; build a Bevy hello-world against it. Write the result into this doc. *Exit:* backend decision confirmed with numbers. *Result (Neptuno 45.20261118, systemd 262, kernel 7.3.3, SELinux enforcing, in a KVM VM):* the sysext backend works and stays the default. `wayland-devel` and `libxkbcommon-devel` (60 files) unpack in 33 ms, label in 30 ms, merge in 12 ms; `pkg-config` and the headers work at once, and a C program using both compiles, links and runs (the Bevy build is the same link against the same files, so it waits for M3's exit check). The extension survives a reboot. Two findings changed the design: labels need `setfiles -r`, not `restorecon` (above), and overlay headroom is zero: `/` is already a composefs overlay, the sysext adds the second level, and a third overlay on `/usr` fails with "maximum fs stacking depth exceeded". So exactly one extension and no transient root are hard limits, and anything else that overlays `/usr` (`ostree admin unlock`) cannot coexist with a merged extension. Podman is unaffected. `dnf download` needs `--arch $(uname -m) --arch noarch`, or i686 builds collide on unpack.
2. **M1 — Host-tier resolver.** Repo metadata download and caching, libsolv bindings, installed-set `.solv` from the image rpmdb, `nx search` / `nx info` / `nx add --dry-run`. *Exit:* dry-run on `libxkbcommon-devel` lists only the missing RPMs, in < 1 s with warm cache. *Status:* resolver done. On Neptuno 45.20261118 the dry-run lists only `libxkbcommon-devel` (the image has `libxkbcommon` and `pkgconf`) in 0.57 s warm (installed set and three repos from `.solv` caches); cold, it is bound by the metadata download (~45 MB). `nx repo add` moved to M2, since repos are recorded in `manifest.toml`; until then the resolver uses the repos the image enables (`fedora`, `updates`, `updates-archive`). Commands read the metadata cache and download only for a repo with nothing cached; `--refresh` forces a download, and metadata past its `metadata_expire` (or kept because no mirror answered) draws a warning. The refresh timer that keeps it current comes with M5. Still to come from the spec: the Koji fallback for image relatives (M2, with package downloads); `nx info`'s files, dependencies and routing reason (M2 and M6); the image-only hint on `nx add` (M7); and search across the tool tier (M6).
3. **M2 — Store and generations.** Download, verify, unpack into the store; assemble generations; manifest and lockfile; `nx list`, `nx why`. *Exit:* generations build correctly without activation. *Status:* done. Package files are verified and unpacked in-process with the image's librpm ([ADR-0008](adr/0008-librpm-from-the-image.md)), against the repo's own keys only; every file is checked against the signed header's digest, and setuid, file capabilities and device nodes are refused. `nx add` re-solves the whole manifest (locked builds favored), downloads four at a time with each file checked against the metadata's SHA-256 (falling back to other mirrors), unpacks into `store/rpm/<sha256>` (refusing any entry whose path goes through a symlink, so a package can't write outside its directory), and builds `generations/<N>` (hardlinks into the store, `extension-release.nx`, `manifest.toml`, `lock.toml`), refusing any file the booted deployment's `/usr` already has (invariant 1) or two packages disagree about. A path under a directory the image has as a symlink (`/usr/sbin -> bin`) moves to where the symlink leads, as rpm would install it. The tree is checked against the plan and the TOML read back, everything is flushed (one `syncfs` for the store, one for the generation) before each rename, and `current` is swapped last. There is a space check before downloading. A spec the image already provides stays in the manifest (invariant 8), and `nx list` shows it as in the image. An older nx facing a generation a newer one wrote shows the newest one it can read and refuses changes (invariant 9). Added repos' fingerprints are shown before anything changes; a pinned key that no longer matches the lockfile blocks the repo until `nx repo add <repo> --new-key`, and a package that fails to verify suggests the same. `foo@copr:owner/project` takes a package from that repo only. The Koji fallback works: with `updates-archive` disabled on Neptuno 45.20261117, `nx add mesa-libEGL-devel` fetched the image's exact 26.2.6-1.fc45 from Koji's signed copies (`data/signed/f577861e/`), and a C program built against the merged generation (EGL, xkbcommon, wayland-client) linked and ran. `nx add wayland-devel libxkbcommon-devel` took 3.6 s with downloads; `nx list` takes 5 ms. `nx repo add` takes `copr:owner/project`, a `.repo` URL, or a base URL with `--key`, shows and pins the key, and builds a generation like any manifest change. Until nxd (M3) the CLI needs write access to the state directory (`NX_STATE_DIR` overrides `/var/lib/nx`), store files belong to that user, and files outside `/usr` are left out of the extension (reported) until the scriptlet translator (M4). Left for later: the live download block under the `↓` lines; `state.sqlite` (`nx why` reads the generation's tree and lockfile, which is instant at this size; the index arrives when history and `nx generations` need it, M3); the `nx gc` hint on a full disk (with `nx gc`, M5); and `nx remove`, `nx rollback` and `nx generations` (M3).
4. **M3 — Activation and nxd.** Sysext and prefix backends with probing; nxd with polkit; `nx add`, `nx remove`, `nx rollback` live. *Exit:* `nx add wayland-devel` then `cargo build` of a Bevy app succeeds with no reboot. *Status:* done. `nx add`, `remove`, `repo add|remove` and `rollback` build or pick a generation and ask nxd to make it live (`nx generations` lists them from their lockfiles; `state.sqlite` stays deferred). nxd is socket-activated, one process per connection; it authorizes through `pkcheck` (the wheel rule allows an active local session without a password), validates the tree, labels it with `setfiles -r`, swaps `current`, links `/var/lib/extensions/nx` and runs `systemd-sysext refresh`, then checks `status --json` lists `nx` under `/usr`. On the Neptuno 45.20261118 VM an activation takes 80–150 ms end to end, and a request from a user polkit doesn't allow is refused with `current` untouched. `nx shell` exports the prefix backend's variables; with sysext it has nothing to add. Exit check: `nx add cargo wayland-devel libxkbcommon-devel alsa-lib-devel systemd-devel` (Fedora's own cargo, from the host tier, 295 s of that was the 81 MB download in the VM) and then `cargo build` of a Bevy 0.20 app with default features (wayland, x11, alsa, udev) succeeded in 5 m 52 s with no reboot. `tests/vm/m3.sh` repeats the add, remove, rollback, refusal and prefix checks. Left for later: `--dry-run` on `nx repo add|remove`, forcing the backend from the CLI (only `[host] backend` in the manifest), the compatibility check that would let `nx rollback` cross image digests (M5, so it refuses now).
5. **M4 — Scriptlets and services.** Translator for sysusers/tmpfiles/presets/triggers with a test corpus; unit enable/start/stop. *Exit:* `nx add syncthing` installs and runs the service; unknown scriptlets are refused with a clear message. *Status:* done; see *Scriptlet translation, as built*. In the VM `nx add syncthing` makes `syncthing@.service` visible at once, `systemctl enable --now syncthing@tester` runs it, and `nx remove syncthing` stops and disables every instance before the refresh. `memcached` is refused for `memcached-selinux`'s `%pre` (`fixfiles`) with the image hint, and passes with `--allow-scriptlets`; its user and `/etc/sysconfig/memcached` appear through sysusers and tmpfiles. `tests/vm/m4.sh` repeats this and checks the shared cache. Also in M4: the shared metadata cache and NSS-aware wheel membership. Left for later: processes holding replaced files, and services on the prefix backend.
6. **M5 — Image upgrade lifecycle.** Staged-image re-resolve, `nx-select.service`, notifications, GC. *Exit:* a bootc upgrade + reboot comes up with a matching host tier and no manual steps. *Status:* done; see *Image upgrade lifecycle, as built (M5)*. In the VM, with `wayland-devel` and `libxkbcommon-devel` added on Neptuno 45.20261118 and 45.20261117 staged (`tests/vm/m5.sh`): the bootc hook built the staged image's generation in 2 s with no command; a change made while it was staged (`nx add libxkbcommon-devel`) built both images' generations; after the reboot `nx-select` made the new image's generation current and sysext merged it, and after `bootc rollback` and a reboot it picked the old image's. With the booted image's generations deleted (the hook never ran), boot merged the previous generation because the offline check passed, and `nx sync` then built and activated the right one in 2.5 s (`nx-select` takes 0.1 s with a digest match, 1.3 s with the offline check). A generation of another Fedora release or one that doesn't resolve offline yields an empty generation and a boot with no host tier; `nx gc` kept the booted, rollback and newest generations. Left for later: desktop notifications. Afterwards: `nx-settle.service` runs `nx select --after-merge` once `systemd-sysext` has, and with the extension forced out of `/usr` in the VM (a wrong `VERSION_ID`, so nothing merged) the boot recorded the prefix backend and `nx shell` exported the generation's `usr`; the next `nx add` merged it again.
7. **M6 — Tool tier.** *Status: done; see* Tool tier, *As built (M6)*. Port nx 0.x's catalog, narinfo walker and internal-json progress; store paths by evaluation (ADR-0009); realisation through the daemon with per-generation GC roots; compiled shims; the system-plugging classifier and routing; the unfree path and extra caches. *Exit:* `nx add ripgrep` works with no Nix profile and no Nix env on `PATH`, and an allowlisted unfree tool installs. This is where nx 1.0 replaces nx 0.x in Neptuno. *Spike (`spike/m6-tooltier.sh`, Neptuno 45.20261118, Nix 2.35.4):* the four tool-tier open questions are answered below; the `store-paths.xz` join doesn't work, so store paths come from evaluation ([ADR-0009](adr/0009-store-paths-by-evaluation.md)); Fedora's nix has no `nix-store`, so realising is `nix build --out-link`; and the shim needs only `TERMINFO_DIRS`.
8. **M7 — 1.0: doctor, migration, packaging, Neptuno.** The last milestone: what is left of the spec, a release an image can install, and Neptuno moved onto it. *Exit:* the laptop runs nx 1.0 from a Neptuno image, migrated from nx 0.x with `nx migrate`, and `v1.0.0` is tagged.
   - *Commands.* `nx doctor` (the checks under *CLI surface*); `nx apply` (make the system match `manifest.toml`: a new machine copies the file and runs it); `nx migrate` (nx 0.x's `~/.nix-profile` into the tool tier, then the profile and its registry pin emptied); the image-only hint on `nx add`; `nx upgrade` that leaves out, and names, a tool whose attribute the new release dropped instead of failing; shell completions and man pages generated from clap.
   - *Background.* `nx-upgrade.timer` runs `nx upgrade --tool` weekly as root (the host tier follows the image and the daily `nx sync --refresh`). Desktop notifications come from the user's session: `nx-notify.path` (a user unit) watches the state directory, and `nx notify` sends what is new (a sync failure, a package left out of an image's generation, a failed upgrade) to `org.freedesktop.Notifications`, once each.
   - *Packaging.* A tag builds one tarball per Fedora release and architecture, `nx-<version>-fc<release>-<arch>.tar.gz`, in `fedora:<release>` (nx links the image's libsolv and librpm), with `SHA256SUMS`. It holds the tree an image unpacks at `/`: `usr/bin/nx` and `nxd`, `usr/libexec/nx/nx-shim`, the units and preset, sysusers, tmpfiles, the polkit action and rule, completions and man pages, and the PATH snippets: `etc/profile.d/nx.sh` and `usr/share/fish/vendor_conf.d/nx.fish` for shells, `usr/lib/environment.d/60-nx.conf` for the session (keybinds, launchers, user units).
   - *Neptuno.* `20-overlay.sh` unpacks the tarball (pinned by `SHA256SUMS`, bumped by Renovate) and the units are declared in `base.toml`. What goes: the Nix-profile PATH reordering (`zz-neptuno-nix.sh`, the block in `zz-neptuno.fish`, `60-nix.conf` and their tests), since nothing from `/nix` reaches `PATH` except nx's shims, after `/usr/bin`; the weekly `nx upgrade` and `nx clean` user timers; the `-devel` packages in `devlibs.toml` (compilers stay) and the `neptuno devbox` distrobox, since `nx add` installs those live. Nix itself stays (`nix.mount`, the daemon, busybox, the download buffer).
   - *Status (Neptuno 45.20261118 VM, `tests/vm/m7.sh`):* the commands, the timer, notifications, completions and the release tarball are done. `nx doctor` passes on the VM and fails on a leaked `LD_LIBRARY_PATH`; `nx apply` adds `libyaml-devel` from a manifest file and the old file takes it out again; `nx migrate` moved `hello` from tester's Nix profile (and dropped a registry pin), after which `hello` is nx's; `nx-notify.path` starts `nx notify` when a failure is recorded, and it sends each once; `nx-upgrade.service` runs clean; `kernel-modules-internal` (`/lib/modules`) and `usbguard-selinux` get the image hint, while `memcached-selinux`, pulled in by memcached, is still the scriptlet translator's (the hint applies to packages the manifest names). m3, m4 and m6 pass unchanged. The image hint checks directories the RPM ships, as it names them: `lib/modules` and `usr/lib/modules`, `usr/share/selinux/packages`, `usr/lib/dracut/modules.d`. The packages the manifest names are downloaded and unpacked first, so the hint comes before any of their dependencies is fetched (`usbguard-selinux` no longer downloads `usbguard`); `--dry-run` can't give it, since it needs the files. A change holds the state lock until nxd has made its generation live, so two at once both land (`race` phase: before, the second built on the generation before the first and dropped it). Still to do: the Neptuno switch and `v1.0.0`.
   - *Known limits of 1.0* (documented, not built): processes still holding replaced files aren't listed, only services are `try-restart`ed; the prefix backend sets up no services; the tool tier exposes commands only (no `.desktop` files, man pages or completions of tools: GUI apps are Flatpak's); a Koji build that exists only unsigned is refused; `nx doctor` has no ELF scan and no overlay-headroom check; `nx add` completes nixpkgs attributes only (RPM names would need the repos loaded on every key press); the boot-time compatibility check compares exact builds, so it is conservative; nx has no self-update (it changes with the image); no `state.sqlite` (the lockfile holds everything it would).

**Working on it:** `AGENTS.md` has the gates and conventions, and [plan-1.1.md](plan-1.1.md) the work in progress. Update this doc's open questions as they get answered. `compat::check`, `nx select` and `nx rollback` treat a generation with `skipped` as a normal one: it is what the image could resolve. A package is left out only for a resolve problem (`Unresolvable`); a network or cache error fails the sync instead. `nx add`, `remove` and `repo` keep a package the current generation left out out while it still doesn't resolve (`resolve_manifest`), with a warning; a package named on the command line must resolve, so a typo still fails.
