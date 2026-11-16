# nx

nx is a Homebrew-style front end to a Nix profile, in Go. Start with
`docs/product.md` (what it shows and why), then the ADRs.

## Gates

- `make vet test` before every commit: gofmt, `go vet`, unit tests.
- Pure Go, `CGO_ENABLED=0`. New dependencies need a reason in an ADR.
- Conventional Commits. End every commit with `Assisted-by: <Model> via <Tool>`.
- Confirm before pushing: show the diff and wait.

## Layout

- `cmd/nx`: main.
- `internal/cli`: commands, one method per command; the table in `cli.go`
  produces help.
- `internal/nixlog`: the internal-json parser and tracker. Test changes
  against the recorded logs in `testdata/`.
- `internal/nix`: running nix and drawing its progress (`view.go`).
- `internal/catalog`, `internal/profile`, `internal/cache`: the reads that
  make nx fast. None of them start nix.
- `internal/ui`: colours, symbols, sizes, the live region.

## Pitfalls

- Nix logs download retries at error level with "warning:" in the text, and
  logs recovered failures as "error (ignored):". Classify by text, not level.
- A plan's paths arrive one message each, and a build plan and a fetch plan
  are two separate announcements.
- `--impure` (needed for `NIXPKGS_ALLOW_UNFREE`) also turns off the eval
  cache. Use it only when an unfree package is involved.
- A line wider than the terminal wraps and breaks the live region's redraw;
  `ui.Live.Draw` truncates, so don't write to stderr around it.

## Agent skills

### Issue tracker

Issues are tracked in GitHub Issues on `Siddhj2206/nx` (via the `gh` CLI). See `docs/agents/issue-tracker.md`.

### Triage labels

Default vocabulary: `needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, `wontfix`. See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: `GLOSSARY.md` and `docs/adr/` at the repo root. See `docs/agents/domain.md`.
