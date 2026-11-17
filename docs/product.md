# nx: product

nx installs command-line packages from nixpkgs into your Nix profile. It fills
the role Homebrew plays on macOS: per-user packages, no root, and one command
to keep them current. It covers the everyday half of package management and
leaves the rest to `nix`.

## Principles

- **Fast by not asking Nix.** Search, info, list, history and outdated read
  files: the profile manifest, the registry, a local catalog of nixpkgs, and
  narinfo files from the binary cache. Nix runs only to change the store.
- **One nixpkgs for everything.** The user registry pins `nixpkgs` to one
  channel release. Every package comes from it, so they share dependencies,
  and only `nx upgrade` moves it ([ADR-0004](adr/0004-one-pin-from-the-channel.md)).
- **Say what happens, package by package.** Each download or build gets a
  line as it finishes, with a live block underneath showing what's in flight.
- **Results on stdout, progress on stderr.** `nx list | grep x` sees only
  results. Colour turns off for pipes, `NO_COLOR` and `TERM=dumb`.

## What Homebrew and dnf show, and what nx takes

| Moment | Homebrew | dnf5 | nx |
|---|---|---|---|
| Before installing | "Fetching dependencies for x: a, b" | A table of name, arch, version, repo and size; totals; a y/N prompt | The step (`› Installing ripgrep 15.2.0`), then Nix's plan as one line: count, download and unpacked size. No prompt, like brew. |
| While downloading | Concurrent rows with byte counts (brew 4.5+) | `[3/12] name 100% \| speed \| size \| time` per package, then a total row | One permanent `↓ name version size` line per package as it lands. Below them, a live block with each download's bar and bytes, and a total bar with count, bytes and speed |
| Building | Formula build log, collapsed | n/a | `⚙ name version built in 14s` once done; live phase and last log line while building |
| After | "🍺 /opt/.../ripgrep/15.2.0: 13 files, 6.3MB" | "Complete!" | `+ ripgrep 15.2.0`, then `✓ 1 added · 28 unchanged · 7.8 MiB in 6.9s` |
| info | Version, description, homepage, license, installed path and size, dependencies with ✔/✘ | Name, version, size, repo, summary, URL, license, description | Description, homepage, command, license, unfree/broken notes, installed version and nixpkgs revision, own size and closure size, download size, direct dependencies |
| outdated | `name (old) < new` | `check-upgrade` lists updates | `↑ name old → new` against the newest channel release, without moving the pin |
| list | Names (`--versions` adds versions) | name, version, repo | Name, version, and the description when there's a terminal |
| history | n/a | `dnf history`: id, command, date, altered | Each profile generation with its date and what changed (`↑ + -`); the current one is marked |
| cleanup | "Removing: … (N files, X MB)"; "freed approximately X" | `autoremove` and `clean` | Profile versions dropped, then space freed and the store-path count |

## Commands

| Command | Does | Runs nix? |
|---|---|---|
| `add <name>...` (`install`) | Installs from the pinned nixpkgs. Takes Homebrew spellings (`node@22`). Refuses unfree packages without `--unfree`. Typos get "did you mean". Warns when an earlier PATH directory has a command of the same name, since that copy is the one that runs | `nix profile add` |
| `remove <name>...` (`rm`, `uninstall`) | Removes. Space comes back with `clean` | `nix profile remove` |
| `list` (`ls`) | Installed packages | no |
| `search <term>...` | Searches the catalog: exact name, then prefix, then name contains, then description; top-level before package sets; 25 shown | no |
| `info <name>` | Details and sizes | `nix eval` for the store path |
| `try <name> [-- cmd]` (`shell`) | Runs a package without installing it | `nix build`, `nix shell` |
| `upgrade` (`update`) | Moves the pin to the newest channel release, then upgrades everything. Returns at once when every package is already built from the pin | `nix registry pin`, `nix profile upgrade --all` |
| `outdated` | New versions in the newest release | no |
| `history` | Profile generations | no |
| `rollback` | Back one generation | `nix profile rollback` |
| `clean` (`gc`, `cleanup`) | Drops history older than 14 days and collects garbage | `nix profile wipe-history`, `nix store gc` |

Exit codes: 0 success, 1 failure, 2 misuse, 130 interrupted.

## Output

Every line is indented two spaces and starts with one symbol for its kind:

```
› step     ↑ upgraded   + added    - removed   ↓ fetched
⚙ built    ✓ done       ! warning  ✗ error
```

Nix's own messages are reworded or dropped:

- An evaluation trace shrinks to its innermost error ("unrar-7.3.2 has an
  unfree license (unfreeRedistributable)").
- A download retry, which Nix logs at error level with the URL, becomes
  `! retrying ripgrep · <reason>`.
- "error (ignored)" messages are dropped, since Nix recovered by itself.
- After `add`, any of the new package's commands that an earlier PATH
  directory also provides is listed once per directory:
  `! clang: addr2line, ar, as, c++, c++filt and 15 more · /usr/bin comes first on PATH, so its copy runs`.
  The check is generic and names no package. A host that puts the system
  before the profile (Neptuno does, so a Nix compiler can't replace
  `/usr/bin/cc`) still lets the user install anything.
- Lines that aren't JSON are passed through unchanged, so a format change in a
  newer Nix shows Nix's own words instead of nothing.

## Speed

Measured on the reference machine with 28 packages installed:

| | bash nx / nix | nx |
|---|---|---|
| `list` | ~0.2 s (`nix profile list`) | 3 ms |
| `search ripgrep` | 1.2 s cached, 20 s+ cold (`nix search`) | 90 ms |
| `upgrade` with nothing new | several seconds (re-evaluates every package) | 0.9 s (one HTTP request) |
| `info ripgrep` | n/a | ~1 s (eval plus parallel narinfo lookups) |
| `add` of a package already in the store | ~1 s | 0.25 s |

Downloads are as fast as Nix makes them: `max-substitution-jobs` and
`http-connections` in nix.conf decide parallelism, and nx only draws them.

## Later

- Build the catalog for a pin older than the channel's current release (the
  release bucket can be listed); for now the newest catalog stands in, and
  search says so.
- `nx which <command>`: which package provides a command, from nix-index's
  database.
- `--json` on list, search, info and outdated.
- Talk to the daemon directly for store queries, if a measurement asks for it.
