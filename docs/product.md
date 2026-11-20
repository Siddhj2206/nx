# nx: product

What nx 1.0 does and shows, command by command. [design.md](design.md) is
how and why it works; nx 0.x, the Go front end to a Nix profile, is tag
`v0.2.0`, and its output is what 1.0 keeps.

nx installs packages on a Fedora bootc system, live, without rebuilding the
image or rebooting. It covers the everyday half of package management:
`dnf` for the image itself and Flatpak for desktop apps stay as they are.

## Principles

- **Two tiers, one ABI per process.** Packages that plug into the system
  (`-devel` packages, libraries, compilers, services) are Fedora RPMs for the
  booted image, overlaid onto `/usr`. Standalone tools come from nixpkgs,
  sealed with their own libraries. `nx add` picks the tier; `--host` and
  `--tool` override it.
- **The image is the anchor.** Host packages are resolved against the
  booted image and resolved again for every new one. nx never writes the
  image and never upgrades what the image ships.
- **One nixpkgs for every tool.** The tool tier follows one pinned nixpkgs
  release; `nx upgrade` (or the weekly timer) moves it.
- **Reads are local.** `list`, `why`, `generations` and `status` read files.
  `search` and `info` use cached repo metadata and wait on the network only
  for a repo with nothing cached, or with `--refresh`.
- **Every change is a generation.** `add`, `remove`, `upgrade`, `apply` and
  `repo` build a new generation and make it live; `rollback` goes back to an
  earlier one.
- **Results on stdout, progress on stderr.** `nx list | grep x` sees only
  results. Colour turns off for pipes, `NO_COLOR` and `TERM=dumb`.

## Commands

| Command | Does |
|---|---|
| `add <pkg>…` | Adds packages, live. Takes names, commands (`rg`), provides (`pkgconfig(xkbcommon)`) and files; `name@repo` pins the repo. Typos get "did you mean". A package the image must ship (kernel, drivers) gets a hint instead. `--host`/`--tool` pick the tier; `--override` and `--without` settle a command two packages provide; `--allow-scriptlets` installs despite scriptlets nx can't translate. |
| `remove <pkg>…` | Removes packages and the dependencies only they needed; stops and disables their units first |
| `upgrade` | Host packages to newer builds for the image, and the nixpkgs pin to its current release (`--host` or `--tool` for one tier) |
| `list` (`ls`) | Installed packages: host packages with their versions and what pulled them in, then tools with their release and commands |
| `search <term>…` | Fedora, added repos and the nixpkgs catalog, each result marked with its source; 25 per source |
| `info <pkg>` | One package: version, tier and why, repo or release, license, size, commands |
| `why <name\|path>` | Who provides a package, command or file, why it is there, and the generation that added it |
| `generations` | Every generation, what it changed, and the current one |
| `rollback [N]` | Makes the previous generation (or `N`) current again |
| `apply [file]` | Makes the system match a manifest: a new machine copies `manifest.toml` and runs it |
| `repo add\|list\|remove` | Third-party RPM repos: `copr:owner/project`, a `.repo` file's URL, or a base URL with `--key`. Keys are pinned by content |
| `shell [cmd]` | A shell, or one command, with the host tier on its paths |
| `status` | The images nx resolves against (booted, staged, rollback), each one's generation, and the tool tier's release |
| `doctor` | Checks that what nx set up is what the system runs: nxd, the merge, `PATH` order, leaked Nix variables, background failures. Exits 1 on a failure |
| `sync` | Builds a generation for every deployed image; the staged-image hook, boot and the daily timer run it |
| `gc` | Removes generations nothing deployed needs, and the packages only they used |

`add`, `remove`, `upgrade`, `rollback`, `apply` and `gc` take `--dry-run`. Exit codes: 0
success, 1 failure, 2 misuse, 130 interrupted.

In the background, `nx-sync` builds generations for a staged image and
`nx-refresh.timer` refreshes repo metadata daily; `nx-upgrade.timer` moves
the nixpkgs pin weekly. What they couldn't do (a failed sync, a package
left out of an image's generation) reaches the desktop as a notification.

## Output

Every line is indented two spaces and starts with one symbol for its kind:

```
› step     ↑ upgraded   + added    - removed   ↓ fetched
⚙ built    ✓ done       ! warning  ✗ error
```

Results go to stdout; steps, warnings and errors to stderr. A change lists
what it added, upgraded and removed, then one summary line:

```
  + libffi-devel   3.5.2-3.fc45   fedora  29 KiB
  + wayland-devel  1.26.0-3.fc45  fedora  161 KiB

  2 packages · 190 KiB download, 758 KiB installed · dry run, nothing changed
```

Detail views (`info`, `why`) have a bold name, a dim version, then rows
with dim keys padded to 11:

```
  ripgrep 15.2.0

  Asked       rg
  Tier        tool · nixpkgs, sealed beside the image
  Release     nixpkgs-26.11pre1124565.19cfded70b91
  Why         you added it · a standalone tool
  Generation  65 · added in 63
```

An error is one line of context joined with `: `. Phase 8 of
[plan-1.1.md](plan-1.1.md) splits it into a headline, reasons and fixes.

Nix's own messages are reworded or dropped:

- An evaluation trace shrinks to its innermost error ("unrar-7.3.2 has an
  unfree license (unfreeRedistributable)").
- A download retry, which Nix logs at error level with the URL, becomes
  `! retrying ripgrep · <reason>`.
- "error (ignored)" messages are dropped, since Nix recovered by itself.
- Lines that aren't JSON are passed through unchanged, so a format change in a
  newer Nix shows Nix's own words instead of nothing.

## Speed

nx 1.0.1 on the reference machine (Neptuno 45, 6 host packages, 25 tools,
warm caches):

| Command | Time |
|---|---|
| `list` | 12 ms |
| `generations` | 13 ms |
| `why rg` | 70 ms |
| `status` | 170 ms |
| `info ripgrep` | 1.5 s |
| `add --dry-run wayland-devel` | 1.8 s |
| `search ripgrep` | 2.0 s |

`search`, `info` and a resolve load the image's rpmdb and every repo into
libsolv from `.solv` caches; that load is most of their time.

## Later

[plan-1.1.md](plan-1.1.md) is what comes next: one builder running as `nx`,
the prefix backend, unfree packages, extra caches and `nx migrate` removed,
and `nx outdated`.
