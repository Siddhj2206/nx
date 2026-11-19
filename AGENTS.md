# nx

nx 1.0 is a package manager for a Fedora bootc system, in Rust: Fedora RPMs
overlaid live onto the image's `/usr` (the host tier) and sealed nixpkgs
tools beside it (the tool tier). It is a rebuild of nx 0.x, a Go front end to
a Nix profile that lives on as tag `v0.2.0` and is still what Neptuno ships.

Start with `docs/design.md` (the design, milestones and open questions), then
`docs/adr/0006-rust-rebuild-two-tiers.md` (why a rebuild, and what changed
from the "bx" draft), then the other ADRs.

## Gates

- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and
  `cargo test` before every commit. CI runs the same, plus shellcheck on
  `spike/`.
- No `unwrap()`/`expect()` outside tests (clippy denies them). Errors via
  `thiserror` in library crates, `anyhow` in the `nx` binary.
- Everything that touches the system goes behind a trait or a root path, so
  tests run unprivileged against fixtures. Record real output into
  `tests/fixtures/` rather than writing it by hand.
- Add a crate or a dependency when a milestone needs it, not ahead of time.
  A dependency with a system library (libsolv) needs an ADR.
- Conventional Commits. End every commit with `Assisted-by: <Model> via <Tool>`.
- Confirm before pushing: show the diff and wait.
- Never run a spike or an activation on the daily driver; use a bootc VM
  built from the Neptuno image (design doc, *Milestones*).

## Output

nx 1.0 keeps nx 0.x's look; `docs/product.md` *Output* is the reference.

- clap parses, nx draws. Never let clap print: help and errors go through
  `help.rs` and `ui.rs` (`Cli::try_parse`, then render). A command's line in
  `nx help` is its doc comment; its group is in `help::PLACEMENT`.
- Every line is indented two spaces and starts with its kind's symbol
  (`› ↑ + - ↓ ⚙ ✓ ! ✗`). Results on stdout, steps, warnings and errors on
  stderr. Detail views follow `info`: bold head, dim version, then rows with
  dim keys padded to 11 (`Stream::row`).
- Colour comes from anstream's decision (TTY, `NO_COLOR`, `TERM=dumb`,
  `CLICOLOR_FORCE`); never write escape codes by hand outside `Style`.
- Exit codes: 0 success, 1 failure, 2 misuse, 130 interrupted.
- Test output as plain text through `Ui::plain` and compare whole blocks.

## Layout

- `crates/nx-cli`: the `nx` binary. `main.rs` holds the clap definitions and
  exit codes, one module per command (`status.rs`), `help.rs` draws help and
  rewords clap's errors, `ui.rs` is the output kit ported from nx 0.x.
- `crates/nx-core`: the system as nx sees it: deployments and image digests
  (`deployment.rs`), os-release (`os_release.rs`). Store, generations,
  manifest and lockfile land here in M2.
- `spike/`: throwaway scripts that answer a design question (M0). Their
  results go into `docs/design.md`; the script can go once they have.
- `testdata/`: nx 0.x's recorded Nix internal-json logs, kept for the M6
  progress parser.

## Pitfalls

- `bootc status` needs root; the CLI is unprivileged. Use
  `rpm-ostree status --json` (about 50 ms), and read other deployments in
  place under `/sysroot/ostree/deploy/<osname>/deploy/<checksum>.<serial>`:
  their os-release and rpmdb are readable without root.
- A deployment's digest is `container-image-reference-digest`; the
  `checksum` field is the ostree commit, which is not what generations are
  keyed by.

Nix log pitfalls from nx 0.x, for M6:

- Nix logs download retries at error level with "warning:" in the text, and
  logs recovered failures as "error (ignored):". Classify by text, not level.
- A plan's paths arrive one message each, and a build plan and a fetch plan
  are two separate announcements.
- `--impure` (needed for `NIXPKGS_ALLOW_UNFREE`) also turns off the eval
  cache. Use it only when an unfree package is involved.

## Agent skills

### Issue tracker

Issues are tracked in GitHub Issues on `Siddhj2206/nx` (via the `gh` CLI). See `docs/agents/issue-tracker.md`.

### Triage labels

Default vocabulary: `needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, `wontfix`. See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: `GLOSSARY.md` and `docs/adr/` at the repo root. See `docs/agents/domain.md`.
