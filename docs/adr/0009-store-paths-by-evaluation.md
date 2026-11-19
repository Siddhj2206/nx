# Tool-tier store paths come from evaluating the requested attributes

The tool tier needs, for each requested nixpkgs attribute, the store paths
of its `outputsToInstall` at the pinned release. The design planned to get
them with no evaluation, by joining the catalog (`packages.json.br`) with
the release's `store-paths.xz`. The M6 spike measured that join on
nixpkgs-26.11pre1122334 and it doesn't work:

- `store-paths.xz` lists every platform Hydra builds (x86_64-linux,
  aarch64-linux, aarch64-darwin), and a narinfo carries no `System`, so 98%
  of the attributes it has match several paths by name. Telling them apart
  by the glibc a path references fails for static binaries.
- Catalog names aren't store names for language packages (`actdiag` is
  `python3.14-actdiag-3.0.0`), and 59,256 of 133,489 free attributes match
  nothing.
- Wrappers Hydra doesn't build, `neovim` among them, aren't in it at all.

So nx evaluates: one `nix eval --json --file <release>/nixexprs.tar.xz`
call for all the attributes a command needs, returning each one's output
paths and `drvPath`. The answer is recorded per attribute in `lock.toml`,
so an attribute is evaluated once per pin. A warm evaluation takes 0.5–1.9 s
per attribute and 130–550 MB; the first one at a pin fetches the release's
nixexprs tarball first (34 s on the test VM; 85 s through
`github:NixOS/nixpkgs/<rev>`, which also unpacks into the git cache). The
evaluation runs as the user through the daemon, which writes the `.drv`
files, so ADR-0001 holds. A path no cache has builds locally from its
`drvPath`.

The catalog stays: search, info and routing read it in milliseconds and
never evaluate.

## Considered

- **The join, disambiguated by narinfo references.** One narinfo per
  candidate per attribute, a glibc of known arch to look for, a failure mode
  for static binaries, and still nothing for the 44% that don't match or
  for wrappers.
- **Evaluating every attribute once per pin** (`nix-env -qa --out-path`).
  Instant lookups afterwards, but minutes and gigabytes of memory per pin
  move on a laptop, for a handful of tools.
- **Hydra's API for the release's evaluation.** Slow, rate-limited, and a
  service nx would then depend on beyond cache.nixos.org.

## Consequences

- `nx add` of a tool new to the pin costs about a second more, and the
  first one after a pin move waits for the tarball unless the refresh timer
  fetched it.
- `nx upgrade --tool` evaluates every installed tool once, in one call.
- Nix must be able to evaluate: an image without Nix disables the tool tier,
  as before.

**Revisit when:** the channel publishes per-platform attribute → store path
data, or evaluation becomes the measured bottleneck of `nx add`.
