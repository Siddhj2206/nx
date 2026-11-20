# Developing and releasing nx

## Tools

nx builds in the dev container (`dev/Containerfile`, the image's Fedora
with libsolv and librpm headers); `dev/run <command>` runs a command in it
with the checkout mounted at the same path. The host's tools come from
[mise](https://mise.jdx.dev) (`mise.toml`): run `mise trust` and
`mise install` once.

| Task | What it does |
|---|---|
| `mise run check` | Every gate CI runs: `cargo fmt --check`, clippy with warnings denied, `cargo test`, shellcheck |
| `mise run fmt` | Format the workspace |
| `mise run lint` / `test` | Clippy, or the tests (`mise run test -- -p nx-rpm some_test`) |
| `mise run dist` | The release tarball for the workspace version, into `dist/` |
| `mise run changelog` | What the next release's changelog section would say |
| `mise run release X.Y.Z` | Cut a release (below) |
| `mise run dev-image` | Rebuild the dev container after `dev/Containerfile` changes |
| `mise run vm-install` | Build nx and install it into the Neptuno test VM |

Every task runs through `dev/run`; in CI (`CI=true`) `dev/run` runs the
command directly, since the job already runs in that Fedora.

## Branches

- `main` is the next release. Work happens on short branches
  (`feat/…`, `fix/…`, `docs/…`) merged by pull request once CI is green.
- `release/X.Y` is cut from the series' latest tag when a fix has to ship
  before the next minor (`git switch -c release/1.0 v1.0.1`). A fix lands
  on `main` first and is cherry-picked (`git cherry-pick -x`) onto the
  release branch; a fix that only applies to the old series goes straight
  there.

## Commits and the changelog

Commits follow [Conventional Commits](https://www.conventionalcommits.org).
The subject is what the changelog prints, so write it for someone running
nx: `fix(add): keep a skipped package out of the manifest`, not
`fix: review findings`.

`CHANGELOG.md` is written by git-cliff (`cliff.toml`) at release time from
the commits since the last tag. Only `feat`, `fix`, `perf` and `revert`
appear (and anything marked breaking with `!`); `docs`, `test`, `ci`,
`build`, `refactor` and `chore` stay in `git log`. The scope (`add`,
`host`, `nx-solv`) is printed in bold before the subject. Don't edit
`CHANGELOG.md` by hand; fix the commit message before it merges.

Merging a pull request with a merge commit keeps its commits, and each
appears in the changelog. Squash a branch whose commits don't read well
on their own, and give the squashed commit a Conventional subject.

## Releasing

On `main` for a minor or major, on `release/X.Y` for a patch, with a clean
tree:

```
mise run release 1.1.0
```

`dev/release` refuses a dirty tree, a wrong branch (1.0.2 only from
`release/1.0`), an existing tag, or a release with nothing for the
changelog. It writes the `1.1.0` section of `CHANGELOG.md`, bumps the
workspace version and `Cargo.lock`, runs the gates, commits
`chore(release): 1.1.0` and makes the annotated tag `v1.1.0`. A failure
puts those three files back. It never pushes: check `git show`, then

```
git push --atomic origin main v1.1.0
```

The tag runs `.github/workflows/release.yml`: the gates and `dev/dist` on
x86_64 and aarch64 in `fedora:45` (`dev/dist` checks `nx --version`
against the tag), then a GitHub release with both tarballs,
`SHA256SUMS`, and the tag's `CHANGELOG.md` section as its notes.

Neptuno pins `NX_VERSION` in `build/20-overlay.sh`; its Renovate opens the
bump PR, and Neptuno's CI boots the image with the new nx before it merges.

A patch release's section lives on its release branch. On `main`, the
fixes cherry-picked from it appear again in the next minor's section,
which is accurate for anyone upgrading from that minor's predecessor.

## CI

`.github/workflows/ci.yml` runs `mise run check` on every pull request and
push to `main` and `release/*`, on x86_64 and aarch64 (arm64 failures
otherwise show up only at release). Pushes also run `mise run dist`, the
build a tag would make, so a release-only failure shows up before the tag.
Pushes save the cargo cache, pull requests restore it, and a new push to
a pull request cancels its old run.

## Dependencies

Renovate (the hosted app, `renovate.json`) opens one PR a week, on Monday
morning, per group: crates, GitHub Actions (after three days), mise's
tools. Non-major updates to crates, first-party actions and tools merge
themselves once CI is green. Majors wait for a human, and the Fedora
release (`fedora:45` in `dev/Containerfile` and both workflows) moves with
Neptuno's image, by hand.
