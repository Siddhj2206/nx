# nx 1.1 plan

Status: plan, 2026-11-20. nx 1.1 makes nx smaller and safer rather than
bigger: one builder that runs as `nx`, systemd doing what it already
can, the fallbacks nobody uses deleted, and the open items of
[audit-1.0.md](audit-1.0.md) closed. Strike an item here once done and
record the decision in `design.md` (an ADR where marked).

## Decisions taken

- **Neptuno is the only target, with one administrator.** Multi-administrator
  support, the prefix backend and nx 0.x compatibility go.
- **Unfree packages and extra Nix caches go.** They are the only reason nxd
  writes `/etc/nix` and restarts `nix-daemon`, and they carry their own
  manifest keys, command, routing branch and evaluation predicate. A local
  build from a derivation stays: wrappers Hydra doesn't build (`neovim`)
  need it.
- **nx is distributed as an RPM from COPR**, built by Packit, instead of
  release tarballs.
- **COPR search is a stretch goal**: done last, only if the rest is.

## How it fits together

```
0.1 fallback telemetry
1.x sandbox the units ──┐
5.2 Web seam ───────────┼─> 2.x nx-user builder + Varlink merge ─> 3.1 SYSEXT_LEVEL
4.x deletions ──────────┘     (4.x first: less code to move)
5.1, 5.3, 6.x, 7.x: independent, any order
```

Phase 4 comes before Phase 2 now that its decisions are taken: the builder
then moves only the code that stays. 5.2 comes before Phase 2 because the
builder rewrite needs the tool tier testable without the VM.

## Phase 0: measure

**0.1 Fallback telemetry.** Every fallback logs one journal line with its
own `MESSAGE_ID`: Koji fetch, partial re-resolve, local build from a
derivation, mirror fallback, a sysext merge that failed. After a few weeks
`journalctl MESSAGE_ID=…` says what runs. Decides 4.2 (Koji).

## Phase 1: systemd hardening, no change in behaviour

**1.1 Sandbox `nx-sync`, `nx-refresh`, `nx-upgrade`.** They activate
through nxd (`live.rs`), so the merge happens in nxd's namespace and
mount-namespace options are safe here:

```ini
ProtectSystem=strict
ReadWritePaths=/var/lib/nx
ProtectHome=yes
PrivateTmp=yes
PrivateDevices=yes
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectKernelLogs=yes
ProtectControlGroups=yes
ProtectClock=yes
ProtectHostname=yes
RestrictNamespaces=yes
RestrictSUIDSGID=yes
LockPersonality=yes
SystemCallFilter=@system-service
SystemCallArchitectures=native
NoNewPrivileges=yes
```

Check that `nix eval`/`nix build` (the daemon socket under `/nix/var`) and
the bootc deployment reads under `/sysroot` work read-only. Contains audit
6's remainder until Phase 2 closes it.

**1.2 Harden `nxd@.service` without mount namespaces.** Add
`CapabilityBoundingSet=` (what it uses: `CAP_SYS_ADMIN` for the refresh
until 2.2, `CAP_CHOWN`, `CAP_FOWNER`, `CAP_DAC_OVERRIDE`, `CAP_MAC_ADMIN`
for `setfiles`; check each in the VM), `SystemCallFilter=@system-service
@mount`, `SystemCallArchitectures=native`, `ProtectClock`,
`ProtectHostname`, `RestrictNamespaces`, `MemoryDenyWriteExecute`.

**1.3 Gate the exposure.** `tests/vm/` records `systemd-analyze security`
for each unit and fails if a score gets worse.

*Exit:* m3–m7 pass unchanged with the sandboxed units.

## Phase 2: the `nx`-user builder (ADR)

Closes audit 6's remainder and audit 7. Everything that changes state is
built by one service running as `nx`; root sees only a generation number.

**2.1 `nx-build@.service`.** Socket-activated (`nx-build.socket`,
`Accept=yes`), `User=nx`, Phase 1's sandbox with
`ReadWritePaths=/var/lib/nx`, network allowed. polkit authorizes the caller
(the rule nxd uses). The CLI sends the command (add, remove, upgrade,
apply, repo, rollback, gc) and draws the progress events the builder
streams back; `nx-proto` grows an event stream. `Session` and the pipeline
move behind the socket unchanged: `Env` already separates them from the
CLI. Search, info, list, why, status, generations and dry runs stay in the
CLI and only read. The timers run their command in the builder too (or as
`User=nx` units of the same binary).

**2.2 Merge through `io.systemd.sysext` (Varlink).** nxd calls `Merge` /
`Refresh` / `List` on `/run/systemd/io.systemd.sysext` (polkit action
`io.systemd.sysext.manage`; nxd is root, so it is allowed) instead of
running `systemd-sysext`. The merge happens in systemd's process, so nxd
can take `ProtectSystem=strict`, `ProtectHome=yes`, `PrivateTmp=yes` with
`ReadWritePaths=/var/lib/nx-merged /var/lib/extensions /etc /var` (sysusers
and tmpfiles write `/etc` and `/var`). `List` replaces parsing
`status --json`. The client is a few dozen lines of JSON over a unix socket
in `nx-proto`, no new crate. Drops the pitfall "the nxd unit must not use
`ProtectSystem=`".

**2.3 nxd accepts files owned by root or `nx` only.** Root's copy stays,
with simpler checks.

**What 2.x deletes**

- `.trash-*` rename and purge (`state::discard`, `State::purge_trash`) and
  "every listing skips dot names".
- The wheel ACL and setgid on `/var/lib/nx` and its cache, `UMask=0002` on
  the units.
- nxd's wheel membership lookup (`/etc/group`, `getent`, `id -G`) and its
  SSSD/LDAP limit: polkit is the only check of who may act.
- The read-only state-lock fallback.
- Generations built by several owners, and nxd accepting the caller's uid.
- The AGENTS.md pitfalls for each of these.

**What stays:** a non-administrator's search and info still use
`~/.cache/nx` when they find no cache; the shared cache becomes readable by
all and written by `nx` only.

*Exit:* `tests/vm/m7.sh` passes with no wheel ACL on `/var/lib/nx`;
`nx add` from tester and `nx sync` from the timer leave every file owned by
`nx`; root's `nx gc` has nothing to purge; nxd refuses a generation owned
by tester.

## Phase 3: systemd enforces the image binding

**3.1 `SYSEXT_LEVEL`.** Neptuno sets `SYSEXT_LEVEL=<IMAGE_VERSION>` in its
os-release; nx writes the same into `extension-release.nx`. systemd then
refuses a generation built for another image even if nx's own check is
wrong. When the compatibility check passes an older generation at boot,
`nx select` rewrites the level in root's copy. Without `SYSEXT_LEVEL` in
os-release nx keeps writing `VERSION_ID`. Closes the design's open question
"`VERSION_ID` or `SYSEXT_LEVEL`".

*Exit:* a generation copied by hand into `/var/lib/extensions` for another
image version doesn't merge.

## Phase 4: deletions

**4.1 Prefix backend.** Goes: probing and the fallback in nxd,
`nx-settle.service` and `nx select --after-merge`, `/var/lib/nx/backend`,
`[host] backend`, `nx shell` (it has nothing to export with sysext; keep
the verb as an error that says so for one release, or drop it), the prefix
tests in `nx-activate`. A merge that fails is an error that names the
cause, and `nx doctor` says whether nx's extension is merged. Update ADR-0006's
"Revisit when".

**4.2 Koji fallback**, if 0.1 shows it never fires. Goes: Koji URLs and
`data/signed/<short key id>/`, `Universe::missing` as a fetch source (it
stays as an error: "the image's build of X is in no repo").

**4.3 Unfree packages and extra caches.** Goes: `[tool] unfree`,
`--allow-unfree`, `allowUnfreePredicate` in the evaluation, the unfree
branch of routing (route says: unfree, so a vendor RPM repo with `nx repo
add`, or Flatpak), `nx cache` and `[[cache]]`, `nx-activate/nixconf.rs`,
nxd's `nix-daemon` restart, the `--option extra-substituters` arguments.
Stays: building from a derivation for a path no cache has. Before
upgrading, `nx remove` the unfree packages; on its first activation 1.1's
nxd removes `/etc/nix/nx.conf` and its `!include` (that cleanup goes in
1.2). A manifest with `unfree` or `[[cache]]` is read with a warning.

**4.4 nx 0.x compatibility.** Goes: `nx migrate` and `nx_nix::profile`,
the `[overrides] names` reading, `keys/<repo id>.asc` pins, the doctor
check for `~/.nix-profile`. The laptop has migrated.

**4.5 Docs.** Remove from `design.md` what no longer exists; the *Who may
do what* table becomes two rows.

## Phase 5: the rest of the audit

**5.1 Audit 3: the image hint on `--dry-run`.** A dry run downloads the
packages the manifest names (not their dependencies) into the package
cache and runs the hint on their file lists, as a real run already does.

**5.2 Audit 10: a seam for `nx-nix`'s `Web`.** The channel, catalog and
narinfo fetches go through `nx-http`'s `Http` trait, as `nx-rpm`'s do.
`session_tests.rs` then runs a tool-tier change that evaluates something
new, with a fake `Nix` and recorded catalog and narinfo fixtures.

**5.3 Audit 21: one solve per sync.** Solve the whole manifest; only when
it fails, re-solve alone the specs `Problems::involved` names, not every
spec. Measure `nx sync` before and after on a manifest of 30 packages.

Audit 15 stays as decided. Once 2.x, 5.1–5.3 are done, delete
`audit-1.0.md`.

## Phase 6: distribution and early warning

**6.1 COPR with Packit (ADR, amends ADR-0005).** `nx.spec` in the repo;
Packit builds every tag in a COPR project for each Fedora release in
support and rawhide, x86_64 and aarch64, against the release's own libsolv
and librpm. The RPM installs what `dev/dist` puts in the tarball (binaries,
units and preset, sysusers, tmpfiles, polkit, shell snippets, completions,
man pages) and requires `libsolv`, `rpm-libs`, `systemd`, `polkit`; Nix
stays a weak dependency (`Recommends: nix`). Neptuno installs `nx` from the
COPR at a pinned version in `base.toml` (`nx-1.1.0-1.fc45`), and Renovate
bumps it. Goes: `release.yml`'s matrix, `dev/dist`'s tarball, the
`SHA256SUMS` pin in `20-overlay.sh`.

**6.2 Rawhide canary.** A weekly workflow in `fedora:rawhide` builds nx
and runs the fixture tests and a dry-run resolve against rawhide's repos,
so a moved repo file or a soname bump shows up months before Neptuno
rebases. The COPR rawhide chroot gives the build half for free.

**6.3 Nix's log degrades.** An unknown internal-json message or field is
skipped, never an error: a Nix release that changes the log loses the
progress bar, not the install. A test feeds the parser a line it doesn't
know.

## Phase 7: features

| # | What | Notes |
| --- | --- | --- |
| 7.1 | `nx outdated` | Both tiers, from cached metadata and the newest catalog, no network: `↑ name old → new`. A dry run of `nx upgrade`'s plan. |
| 7.2 | `--json` | On `list`, `search`, `info`, `status`, `outdated`; from the plain data the `render` functions already take. |
| 7.3 | `nx try <pkg> [-- cmd]` | A tool-tier package without installing it: evaluate, realise, run, no generation. A host-tier package is refused (it needs a merge). |
| 7.4 | `nx which <command>` | Which package provides a command in either tier: `primary.xml`'s files in bin directories, the catalog's `mainProgram`. |
| 7.5 | Processes using replaced files | After a host-tier change, list processes that map deleted files under `/usr` (`/proc/*/maps`), as `dnf needs-restarting` does. Known limit of 1.0. |
| 7.6 | Man pages and completions of tools | Link a tool's `share/man` and its bash, fish and zsh completions into the generation (`current/share/…`) and add that directory to the shell snippets (and to `MANPATH` without replacing the default). They are data, not libraries: invariant "one ABI per process" holds. Known limit of 1.0. |
| 7.7 | RPM names in `nx add` completion | `nx sync --refresh` writes one name list per repo; completion reads it. Known limit of 1.0. |
| 7.8 | Prefetch nixexprs | `nx sync --refresh` fetches the newest release's `nixexprs.tar.xz`, so the first `nx add` after the weekly pin move doesn't wait for it (34 s on the VM). Left over from M6. |
| 7.9 | COPR chroot check before a rebase | When the staged image's `VERSION_ID` differs, check each added COPR for that release's chroot and name the missing ones in the notification before the reboot. |
| 7.10 | COPR key rotation | `nx add`/`upgrade` show the old and new fingerprints and the exact `nx repo add <repo> --new-key` to run. |
| 7.11 | *Stretch:* COPR search | `nx search --copr <term>` through COPR's API, results marked as COPR with the `nx repo add copr:…` to run. Depends on a service nx doesn't otherwise need; only if the rest is done. |

## Docs

- Rewrite `docs/product.md` for 1.0/1.1: it still describes nx 0.x; keep
  its *Output* section.
- ADRs: the `nx`-user builder (2.x), dropping the prefix backend and
  unfree/caches (4.1, 4.3), COPR distribution (6.1).
- `AGENTS.md`: drop the pitfalls each phase removes; add the builder to
  *Layout*.

## Won't do

Record these in `design.md` so they stop coming back:

- **ELF scan in `nx doctor`:** needs an ELF parser; the variable checks
  cover the way a program mixes ABIs.
- **Overlay headroom check:** it is zero by design; one extension and a
  loud merge failure cover it.
- **Signed DDIs (erofs, dm-verity):** the signing key would live on the
  machine it protects; root's copy already makes the merged tree immutable
  to users.
- **confext** (makes nx's `/etc` content read-only), **systemd-sysupdate**
  (downloads prebuilt images from a server), **units over D-Bus or
  `io.systemd.Unit`** (a D-Bus stack for no change), **Varlink for
  `nx-proto`** (167 lines that work).
- **Security advisories (`updateinfo.xml`):** the host tier follows the
  image, which carries the fixes.
- **Self-update:** nx changes with the image (now its COPR pin).
