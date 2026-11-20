# Nix does every store write; nx reads its log

> Still holds in nx 1.0, for the tool tier ([ADR-0006](0006-rust-rebuild-two-tiers.md)). The host tier writes its own RPM store under `/var/lib/nx`.

nx changes the store only by running `nix` (`profile add`, `profile upgrade`,
`registry pin` and so on) with `--log-format internal-json`, and draws its own
progress from that stream. It never writes to the store, the profile or the
registry itself. The daemon stays the only writer, profiles stay ones `nix
profile` understands, and a user can mix `nx` and `nix` freely.

The log format is internal and unversioned. The parser keeps to the activity
and result types that have been stable for years (substitute, copy path, file
transfer, build, progress, set expected, set phase, build log line). Lines it
can't read pass through as text.

## Considered options

- **The daemon protocol, spoken from Go.** It would expose the store with no
  process to start, but it's a large, versioned protocol to keep up with, and
  the time goes to evaluation and downloads, not process start.
- **The Nix C API through cgo.** Breaks the pure-Go, static build.
- **Nix's own progress bar.** One line at a time, with no per-package history,
  which is what nx exists to fix.

## Consequences

- A Nix release that renames activity types degrades nx to plain text until
  the parser catches up. The fixtures in `testdata/` are real logs; record a
  new one when Nix changes.

**Revisit when:** evaluation through nix is the measured bottleneck for a
command, or the internal-json format changes in a way that loses information.
