# nx 1.0 audit

A top-to-bottom read of nx before 1.0 (2026-11-20): the design docs, every
command in `nx-cli`, `nx-core`, `nx-activate`, `nxd`, `nx-proto`, the units
and packaging, and the Neptuno `nx-1.0` branch. `nx-solv`, `nx-librpm`,
`nx-scriptlet` and the catalog/eval/narinfo code were read at their
interfaces only. Each finding is open unless its status says otherwise;
strike it from here once done, and record the decision in `design.md`.

## Bugs

1. **Fixed.** *A concurrent change could be lost.* `Session::build` dropped
   the state lock before `install` asked nxd to activate, so a second
   `nx add` waiting on the lock built on the generation before the first
   and replaced it. The session now holds the lock until `current` has
   moved and releases it before the staged/rollback sync (which takes it per
   image). VM: `m7.sh race` failed before, passes now.
2. **Fixed.** *Nothing ran `nix store gc` on its own.* Only `nx gc` did, while
   the weekly `nx-upgrade.timer` leaves an old tool tier each week, and
   Neptuno dropped `neptuno-nix-clean` on the grounds that nx collects its
   own garbage. `nx sync --refresh` (the daily timer) now roots kept
   generations and runs `nix store gc`; a failure there is a warning.
   VM: `m7.sh upgrade`.
3. **Fixed (real runs).** *The image-only hint came after the whole closure
   was downloaded and unpacked.* The packages the manifest names are now
   fetched and unpacked first and checked before their dependencies are
   downloaded. VM: `m7.sh hint` (usbguard-selinux no longer fetches
   usbguard and libqb). Still open: `--dry-run` can't give the hint, since it
   needs the files; doing so would need the file list from the header
   (download the named packages only, or a range request).
4. **Fixed.** *`nx why` dated "added in" by generation numbers.* Generations
   for other images and ones a rollback left sit between the numbers; it
   now follows `parent` (`why::added_in`, tested).
5. **Fixed.** Keys are pinned as `keys/<sha256>.asc`, named by the lock's
   `key`, never removed while a kept generation names them; `Pins`/`undo`
   are gone (VM: `m7.sh keys`). The finding as written:
   *Repo keys live outside generations.* Pinned keys are files in
   `/var/lib/nx/keys/<id>.asc`, mutable and shared; the lockfile records
   only fingerprints. `nx repo remove` deletes the file, so rolling back to
   a generation that still has the repo can't verify its packages. The
   `Pins`/`undo` code in `repo.rs` and `converge.rs` only compensates for
   this. Keep keys by fingerprint (`keys/<fingerprint>.asc`), referenced
   from the lock: rollback works and the undo goes.

## Security architecture

6. **Fixed (option 1, root's copy).** nxd and `nx select` merge a reflinked
   copy in `/var/lib/nx-merged`, root's, checked as it is made (design,
   *Root's copy*; VM: `m7.sh owner`, two reboots). Still open: root's
   services parse wheel-writable caches; the `nx`-user builder is the
   direction after 1.0. The finding as written:
   *A wheel user's background processes can change what runs from `/usr`.*
   The store is unpacked as the calling user and the extension hardlinks
   it, so merged binaries (including the `ExecStart=` of host-tier system
   services, run as root) are inodes that user owns and can write. nxd
   checks owners at activation; a later write needs no polkit. The design's
   "a tree they own widens nothing" holds for activation (polkit wants an
   active local session) but not for the files afterwards: anything running
   as the user (an npm postinstall) can replace a binary root runs. Also,
   every input the root `nx sync`/`nx upgrade` services parse (`cache/`,
   `keys/`, lockfiles, `.solv`) is wheel-writable, so libsolv and librpm
   parse user-controlled data as root. Options: nxd copies or chowns the
   extension to root before merging, or a root/`nx`-user unpacker owns the
   store. The one 1.0 design decision left.
7. **Deferred past 1.0**, with the `nx`-user builder (6): alone, `User=nx` only moves the `.trash` problem. The design's *The `nx` user* says what it runs today. The finding as written:
   *The background services run as root, not as the `nx` user* that the
   design and `nx.sysusers` describe (`nx-sync`, `nx-refresh`,
   `nx-upgrade` have no `User=`). That is why root-built generations can't
   be deleted by wheel and the `.trash-*` rename-and-purge exists
   (`state::discard`, `State::purge_trash`, an AGENTS.md pitfall), and why
   `nix eval`/`nix build` run as a trusted Nix user. `User=nx` plus a
   polkit rule for that uid would remove the trash machinery and the
   root-parsing exposure in 6.

## Architecture: duplication and overcomplication

8. **Fixed.** `Session::plan(How)` resolves both tiers for every command
   (`asked`, `newer_host`, `newer_tools`; fresh tool packages are worked
   out from the lock), `apply::finish` builds and reports, and
   `changes.rs` is the one change list and summary. `upgrade.rs` and
   `converge.rs` are options plus the pipeline. `sync`'s partial resolve
   stays its own. The finding as written:
   *Six commands re-implement one pipeline.* `add`, `remove`, `upgrade`,
   `apply`, `repo add|remove` and `cache add|remove` each do: change the
   manifest, resolve, open/resolve/plan/realise the tool tier, diff,
   install, render. Duplicated: the host before/after map (`upgrade.rs`,
   `converge.rs`), the tool version map, the `Tier` sequence (add, upgrade,
   apply), and five renderers of "· generation N in Xs" / "· dry run,
   nothing changed". `nx apply` is the general case; the rest are a
   manifest edit plus apply with options (allow newer builds, move the pin,
   which tools are new). One `converge(session, Intent) -> Changes` and one
   renderer.
9. **Fixed.** `Session` has no public fields: reads go through
   `state()`, `current()`, `manifest()`, `universe()`, `tool()`; `route()`
   records a reason; `edit()` changes the manifest after opening and
   refuses a repo change (the repos were loaded when it opened). The order
   is `open` → `plan` → `install`/`finish`. The finding as written:
   *`Session` is a shallow god object.* Callers set and read `pub tool`,
   `fresh_tools`, `routes`, `manifest`, `current`, `universe` between
   calls; the ordering rules (set `tool`/`fresh_tools` before `install`,
   `leave_out` before `build`) aren't in its interface.
10. **Fixed for the host tier.** `Session::open_in` takes an `Env`: the
    state directory, `Http` (metadata, packages, Koji), `Nix` and `Live`
    (nxd, or a `current` that just moves); `Session::open` is
    `Env::system()`. `Tier`, `tool::restore` and `add_roots` take the `Nix`
    they run; `sync::deployed` and the caches take the state. `apply/
    session_tests.rs` runs plan → install → remove on a fixture root with a
    real signed RPM and no network, Nix or nxd. Still concrete: `nx-nix`'s
    `Web` (channel, catalog, narinfos), so a tool-tier change that
    evaluates something new runs only in the VM. The finding as written:
    *No seam at the orchestration level.* `Session` makes `Ureq::new()`
    itself; `Tier` hardcodes `nx_nix::System` and `Web` though a `Nix`
    trait exists; `tool::restore` and `gc` call `System` directly; the state
    directory comes from the environment through `host::state()` in about
    fifteen places instead of from `Machine`. Leaves are well tested;
    add/remove/upgrade/sync have no unit tests, and every M6/M7 regression
    was found in the VM. `nx-nix`'s `Web` has no seam either (nx-rpm's
    `Http` does).
11. **Fixed.** `host::has_command`, for exposure and doctor (`why` builds paths, not a check). The finding as written:
    *Who owns a command is computed three times:* `Session::expose`'s owner
    closure, `doctor::tools`, `why::owner_of_path`, each scanning
    `COMMAND_DIRS` with `symlink_metadata`.
12. **Fixed.** `problems.rs` reads the records once; `doctor` and `notify` render it (`status` shows them per image row from the same records). The finding as written:
    *Background problems are read three ways.* `status`, `doctor` and
    `notify` each read the sync failures, left-out packages and the upgrade
    failure with their own wording. One `problems(state, deployments)` for
    three renderers. The records themselves live in `select.rs`, which is
    about choosing generations.
13. **Fixed**, but the repo identity: `tool::from_drv`, `tool::pinned`, `State::generation_dir`, `nx_core::state::sync_dir`, `generation::image_clash`. The name/id split of an added repo stays: the manifest keeps what the user typed, the id is derived. The finding as written:
    *Small duplications.* `drv^outputs` installables built twice
    (`plan_from`, `restore`); `Release { rev, name, url }` rebuilt by hand
    from `LockedTool` in `tool::pinned`, `status.rs`, `list.rs`; generation
    paths (`generations/<n>/sysext`) hardcoded in nx-activate, doctor and
    `select::read_lock` because `State::generations_dir` is `pub(crate)`;
    `sync_dir` in both nx-core and nx-activate; `check_image` and
    `check_extension` repeat the "first five clashes" message; the repo
    identity is two things (manifest `name`, pool/cache/key `id` via
    `id_of`), which is where the id-collision checks come from.
14. **Fixed.** `nx add --without <cmd>` (`[overrides] without`); older `names` entries still count. The finding as written:
    *`--override` means two opposite things:* against another tool package
    "give this package the name", against the image or host tier "I accept
    losing the name". Two flags (`--take`, `--without`) say it.
15. **Kept, on purpose.** Moving the header types into `nx-librpm` would make the pure `nx-scriptlet` (and its tests) link librpm; the types are plain data the translator defines, and librpm fills them. The finding as written:
    *Misplaced types.* `nx-librpm` (the FFI wrapper) depends on
    `nx-scriptlet` for `PackageMeta`, `Script`, `Phase`: the header data
    model belongs in librpm, the translator consumes it.
16. **Fixed.** `session.rs` is the Session, `apply.rs` is `nx apply`. The finding as written:
    *Swapped names.* `nx apply` is in `converge.rs`; `apply.rs` holds
    `Session` and the generation builder.

## Robustness and performance

17. **Fixed.** `Http::download` streams to disk hashing; `sha256_file` reads in pieces. Was: RPM downloads are held whole in memory (`Http::get` → `Vec<u8>`, 1 GiB
    limit, four threads), and `sha256_file` reads whole files. Stream to
    disk while hashing.
18. **Fixed.** A mirrorlist's lines (or a metalink it answers with) are the bases; `cost=` removed. Was: `mirrorlist=` and `cost=` are parsed and never used: a third-party
    `.repo` with only a `mirrorlist` can't sync.
19. **Fixed.** It watches the two record files and the generations. Was: `nx-notify.path` (`PathChanged=/var/lib/nx`) fires on every mutating
    command (the lock file's close-after-write), not only on failures.
20. **Documented** (design, *Techniques* 8): a second path for tool-only changes isn't worth 240 ms. Was: Every activation runs `setfiles` and `systemd-sysext refresh`, tool-only
    changes included; the design says only when host-tier content changed.
    Fine, but say so.
21. **Kept.** Fine at today's sizes; noted here. Was: `Universe::attribute` re-solves once per spec on every plan, and
    `resolve_what_fits` loops over that: about N² solves per sync. Fine at
    today's sizes.
22. **Fixed.** Both take `--dry-run`. Was: `repo add|remove` and `cache add|remove` have no `--dry-run`, though the
    CLI section says every mutating command does.

## Docs drift (`design.md` against what is built)

23. **Fixed.** The layout, update policy, packaging, techniques, crate list and conventions describe what is built; what isn't is said to be. Was: Still specified, not built and not planned: `state.sqlite` and the
    "local SQLite index" (invariant 9, the performance table, the layout);
    the `nx-elf` crate and a candidate-crate table mostly unused (tokio,
    reqwest, rusqlite, rpm-rs, nix-compat, goblin, tracing); the RPM header
    "by HTTP Range request"; the alias table; the `[tool] channel` manifest
    key (not parsed); security-advisory notifications and `auto_upgrade`;
    `nx self-update` in the clocks table; "Packaging (decision): an RPM";
    nxd's `gc()` request; the ELF re-check after staging; "overrides listed
    by `nx doctor`"; "Conventions for Claude Code". The tarball is
    `nx-v1.0.0-…` while M7 says `nx-<version>-…` (Neptuno matches the `v`
    form). Move these to a "not built" list or delete them, so the doc says
    what is.

## Neptuno (`nx-1.0` branch)

24. **Nothing to do.** With 2 fixed, "nx collects its own garbage" in `doc/product.md` and the
    `neptuno` skill is now true, as of the daily refresh. Removing the Nix
    PATH workarounds, the devlibs and devbox holds up.

## Status

Everything above is fixed or decided except 7 (with the `nx`-user builder,
after 1.0), the dry-run half of 3, and the tool tier's `Web` seam in 10.
