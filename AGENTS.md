# nx

nx is a package manager for a Fedora bootc system, in Rust: Fedora RPMs
overlaid live onto the image's `/usr` (the host tier) and sealed nixpkgs
tools beside it (the tool tier). Neptuno ships it from its release tarballs.

Start with `docs/design.md` (the design and its decisions), then
`docs/adr/0006-rust-rebuild-two-tiers.md` and the other ADRs.
`docs/plan-1.1.md` is the work in progress.

Before changing code that touches the system, read the section of
**`docs/pitfalls.md`** for its area: reading the system (bootc, rpm-ostree,
deployments), the host tier (repos, metadata, libsolv), the store,
generations and the state lock, nxd and activation (sysext, SELinux,
systemd units), shell and PATH, the test VM, and the tool tier (Nix).

## Gates

- `mise run check` before every commit: fmt, clippy with warnings denied,
  tests, shellcheck. CI runs the same task on x86_64 and aarch64 in
  `fedora:45`. Everything builds on the host, which needs `libsolv-devel`
  and `rpm-devel` (`nx add` them); `cargo test` is the inner loop.
- `unsafe` only in `crates/nx-solv` (ADR-0007) and `crates/nx-librpm`
  (ADR-0008), each block with a `// SAFETY:` comment; the rest of the
  workspace denies `unsafe_code`.
- Errors via `thiserror` in library crates, `anyhow` in the `nx` binary;
  `unwrap()`/`expect()` only in tests (clippy enforces it).
- Everything that touches the system goes behind a trait or a root path, so
  tests run unprivileged against fixtures. Record real output into
  `tests/fixtures/` rather than writing it by hand.
- Add a crate or a dependency when a milestone needs it, not ahead of time.
  A dependency with a system library (libsolv) needs an ADR.
- Conventional Commits, each ending `Assisted-by: <Model> via <Tool>`. The
  subject is the changelog line (`feat`, `fix`, `perf` only): write it for
  a user of nx. `dev/release` writes `CHANGELOG.md`; leave it alone.
- `main` is the next release; patches ship from `release/X.Y`, with fixes
  landed on `main` first and cherry-picked (`docs/development.md`).
- Confirm before pushing: show the diff and wait.
- Spikes and activations run in the bootc test VM built from the Neptuno
  image (`mise run vm`), never on the daily driver.

## Output

nx keeps nx 0.x's look; `docs/product.md` *Output* is the reference.

- clap parses, nx draws: help and errors go through `help.rs` and `ui.rs`
  (`Cli::try_parse`, then render), so clap never prints. A command's line in
  `nx help` is its doc comment; its group is in `help::PLACEMENT`.
- Every line is indented two spaces and starts with its kind's symbol
  (`› ↑ + - ↓ ⚙ ✓ ! ✗`). Results on stdout, steps, warnings and errors on
  stderr. Detail views follow `info`: bold head, dim version, then rows with
  dim keys padded to 11 (`Stream::row`).
- Colour comes from anstream's decision (TTY, `NO_COLOR`, `TERM=dumb`,
  `CLICOLOR_FORCE`); escape codes come only from `Style`.
- Exit codes: 0 success, 1 failure, 2 misuse, 130 interrupted.
- Test output as plain text through `Ui::plain` and compare whole blocks. A
  command's `render` takes plain data (`Package`s), so its tests need no
  rpmdb or network.

## Layout

Each crate's `Cargo.toml` description says what it holds. The seams to know:

- `crates/nx-cli/src/session.rs`: manifest → generation (resolve, download,
  verify, unpack, expose the tool tier, assemble), shared by every command
  that changes the manifest: `Session::open` (change the manifest under the
  lock) → `Session::plan(How)` → `Session::install` or `session::finish`.
  A new command that changes packages is a manifest edit and a `How`, not a
  new pipeline. `Session::open_in(Env, …)` takes the state, `Http`, `Nix`
  and `Live` it uses; `session/session_tests.rs` drives the pipeline on a
  fixture root that way (test a change there before the VM).
- `crates/nx-cli/src/host.rs`: `Machine`, the system as one value (root,
  os-release, deployments, arch): commands read `/`, tests a fixture.
  `route.rs` is the pure tier decision; `tool.rs` drives the tool tier.
- `crates/nx-activate` is what nxd runs as root, behind `Layout` and
  `Runner` so tests use a temp tree and a fake `systemd-sysext`.
- `sys.rs` in `nx-solv` and `nx-librpm` is bindgen output from their
  `generate.sh`; regenerate it rather than editing it.
- `dev/`: `vm-install` (`mise run vm`), `dist` (the release tarball, the
  tree an image unpacks at `/`) and `release` (`mise run release X.Y.Z`:
  bump, changelog, gates, commit, tag; it never pushes).
- `systemd/`, `polkit/`, `shell/`: what an image installs.
- `tests/fixtures/`: recorded scriptlets and rpmdbs, each with its
  `record.sh`; `tests/vm/`: the VM scripts. `testdata/`: nx 0.x's Nix logs
  and a real signed Fedora package with its keys.

## Agent skills

- Issues: GitHub Issues on `Siddhj2206/nx` via `gh`
  (`docs/agents/issue-tracker.md`); triage labels in
  `docs/agents/triage-labels.md`.
- Domain language: `GLOSSARY.md` and `docs/adr/` (`docs/agents/domain.md`).
